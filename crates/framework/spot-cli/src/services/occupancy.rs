//! Booking holds on a real site (WP-STO-14.T3; ADR-087 section 6): the host half of
//! `spot-occupancy`.
//!
//! This module binds the HTTP seam of `/api/v1/occupancy/*` ([`OccupancyPort`], WP-STO-14.T2) to
//! the booking service on the site's PostgreSQL, and runs the expiry sweeper.
//!
//! - **The port** ([`OccupancyApi`]): the holder is the authenticated principal (`user:<id>` or
//!   `consumer:<id>`), never a request field; confirm and release are the holder's (or a staff
//!   user's with `occupancy_manage`); the conflict body names nothing of the other holder.
//! - **The sweeper** ([`OccupancyService`]): a pass every `sweep_interval` seconds under the
//!   coordinator lease `occupancy_sweeper`, so one node per site sweeps; `FOR UPDATE SKIP LOCKED`,
//!   a bounded batch, `occupancy_expired` through the outbox in the same transaction as the
//!   delete. Correctness never depends on it: a claim takes over an expired row in its own
//!   transaction.
//! - **One owning node per resource.** A write for a unit another node owns is
//!   `partition.not_owner`, answered before storage is touched. This node is the hub unless the
//!   site's role is `edge` (`[services.node_role]`, `[partition] node`); units homed on an edge
//!   are listed in `[services.occupancy.homes]`.
//! - **Events** `occupancy_held`, `occupancy_confirmed`, `occupancy_cancelled` and
//!   `occupancy_expired` are captured in the claiming transaction by the crate and published
//!   after commit by the outbox.
//!
//! # Level and failure
//!
//! The service is **optional**. When it cannot start (no `occupancy` entity in the site's
//! metadata while `enabled = true`, the `btree_gist` extension or the exclusion constraint
//! missing, a service that refuses its configuration), the site serves everything else and its
//! readiness says `degraded: occupancy (<reason>)`; the occupancy routes answer
//! `503 server.unavailable` and nothing is written. With `enabled` unset the module is inert on a
//! site whose metadata declares no `occupancy` entity.
//!
//! # Configuration
//!
//! `[services.occupancy]` of `common.toml` (unknown keys are refused). Every limit is a
//! configurable default, not a measured value (ADR-087 section 13):
//!
//! | Key | Default | Range |
//! | --- | --- | --- |
//! | `enabled` | unset (on when the entity exists) | `true` forces it, `false` turns it off |
//! | `default_ttl_seconds` | 900 | 1 to `max_ttl_seconds` |
//! | `max_ttl_seconds` | 3600 | 1 to 86400 |
//! | `sweep_interval` (seconds) | 30 | 1 to 3600 |
//! | `max_span_days` | 366 | 1 to 3660 |
//! | `sweep_batch` | 200 | 1 to 5000 |
//! | `span` | `"instants"` | `"instants"` or `"dates"` (whole days at UTC midnight) |
//! | `homes` | none | unit record id to edge name |
//!
//! The operator guide is `docs/running-spot/occupancy.md`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use spot_http::api::Ports;
use spot_http::api::codec::parse_record_id;
use spot_http::api::ports::occupancy::{CONTRACT_MAX_TTL_SECONDS, OccupancyPort};
use spot_http::api::router::routes;
use spot_occupancy::{
    Action, AuditEntry, AuditSink, HomeEdges, Limits, Occupancy, Owner, Span, definition,
};
use spot_storage_contract::Name;
use tokio::sync::watch;

use crate::services::node_role::{ActiveRole, Role};
use crate::services::ports::OutboxLease;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "occupancy/adapter.rs"]
mod adapter;
#[path = "occupancy/identity.rs"]
mod identity;
#[path = "occupancy/sweeper.rs"]
mod sweeper;

pub use adapter::{ENTITY, OccupancyApi, Ready, Settings as PortSettings, map_error};
pub use identity::{ConsumerIdentity, Identity, LiveIdentity, MANAGE, Who};
pub use sweeper::{CONSUMER, MAX_BATCHES, OccupancyService, missing};

/// Start order: after the jobs and coordinator ports (1020), the node role (1005) and the process
/// engine modules, and before nothing: no other module reads this one's port yet.
pub const ORDER: u32 = 1170;

/// What the range of the entity is made of.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpanSetting {
    /// Instants (`instant_range`): appointments, equipment slots.
    #[default]
    Instants,
    /// Whole days (`date_range`): hotel nights, desk days.
    Dates,
}

impl SpanSetting {
    fn span(self) -> Span {
        match self {
            SpanSetting::Instants => Span::Instants,
            SpanSetting::Dates => Span::Dates,
        }
    }
}

/// `[services.occupancy]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// `Some(true)` requires the module (a missing entity degrades the site), `Some(false)`
    /// turns it off, `None` runs it when the site's metadata declares the entity.
    pub enabled: Option<bool>,
    /// A hold's lifetime when the request names none, seconds.
    pub default_ttl_seconds: u32,
    /// The longest hold a request may ask for, seconds (at most 86400).
    pub max_ttl_seconds: u32,
    /// Seconds between sweeper passes.
    pub sweep_interval: u64,
    /// The longest claimed range, in days.
    pub max_span_days: i64,
    /// Most rows one sweeper batch deletes.
    pub sweep_batch: u32,
    /// What the range is made of.
    pub span: SpanSetting,
    /// Units whose one owning node is an edge: the unit's record id and the edge's name.
    pub homes: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: None,
            default_ttl_seconds: 900,
            max_ttl_seconds: 3600,
            sweep_interval: 30,
            max_span_days: 366,
            sweep_batch: 200,
            span: SpanSetting::Instants,
            homes: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// Refuses a table that cannot work, naming the key and never a value.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let bad = |key: &str| Err(format!("services.occupancy.{key}: out of range"));
        if !(1..=CONTRACT_MAX_TTL_SECONDS).contains(&self.max_ttl_seconds) {
            return bad("max_ttl_seconds");
        }
        if self.default_ttl_seconds == 0 || self.default_ttl_seconds > self.max_ttl_seconds {
            return bad("default_ttl_seconds");
        }
        if !(1..=3600).contains(&self.sweep_interval) {
            return bad("sweep_interval");
        }
        if !(1..=3660).contains(&self.max_span_days) {
            return bad("max_span_days");
        }
        if !(1..=5000).contains(&self.sweep_batch) {
            return bad("sweep_batch");
        }
        for (unit, edge) in &self.homes {
            if parse_record_id(unit).is_none() {
                return bad("homes");
            }
            if edge.is_empty() || edge.len() > 64 {
                return bad("homes");
            }
        }
        Ok(())
    }

    /// The service's limits.
    pub fn limits(&self) -> Limits {
        Limits {
            default_ttl: Duration::from_secs(u64::from(self.default_ttl_seconds)),
            max_ttl: Duration::from_secs(u64::from(self.max_ttl_seconds)),
            max_span_days: self.max_span_days,
            sweep_batch: self.sweep_batch,
            ..Limits::default()
        }
    }

    /// Who owns which unit: the hub, except the homed units.
    pub fn owners(&self) -> HomeEdges {
        let mut homes = HomeEdges::new();
        for (unit, edge) in &self.homes {
            if let Some(id) = parse_record_id(unit) {
                homes = homes.with_home(id, edge);
            }
        }
        homes
    }
}

/// The audit sink: one line per operation on the `spot::audit::occupancy` target. Who (the typed
/// holder reference), what and which unit and record; never a range or a payload.
struct TracingAudit {
    site: String,
}

impl AuditSink for TracingAudit {
    fn record(&self, entry: AuditEntry) {
        let action = match entry.action {
            Action::Hold => "hold",
            Action::BookNow => "book_now",
            Action::Confirm => "confirm",
            Action::Release => "release",
            Action::Sweep => "sweep",
        };
        tracing::info!(
            target: "spot::audit::occupancy",
            site = %self.site,
            action,
            actor = %entry.actor,
            unit = ?entry.unit,
            record = ?entry.record,
            outcome = ?entry.outcome,
            "occupancy"
        );
    }
}

/// The service of a site that cannot hold: it reports why and degrades the site.
struct Unavailable {
    reason: String,
}

impl Service for Unavailable {
    fn name(&self) -> &'static str {
        "occupancy"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { Err(self.reason.clone()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

/// The ports of the sites built so far, so a reload build mounts the same port.
fn built() -> &'static Mutex<HashMap<String, Arc<dyn OccupancyPort>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<dyn OccupancyPort>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// This node's place among a unit's possible owners.
fn node_of(role: Option<Role>, partition: Option<String>) -> Owner {
    match (role, partition) {
        (Some(Role::Edge), Some(name)) => Owner::Edge(name),
        _ => Owner::Hub,
    }
}

/// Registers the occupancy service: the port, the routes and the sweeper.
///
/// # Errors
/// A setting that does not decode or is out of range; a port another module already provided.
/// A service that cannot start for any other reason degrades the site instead (see the module
/// documentation).
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("occupancy")?;
    settings.validate().map_err(Error)?;
    if settings.enabled == Some(false) {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let declared = Name::new(ENTITY)
        .ok()
        .is_some_and(|name| env.permission.catalog.entity(&name).is_ok());
    if !declared && settings.enabled != Some(true) {
        tracing::debug!(site = %site_name, "occupancy: the site declares no `{ENTITY}` entity; the module is inert");
        return Ok(());
    }
    let port: Arc<dyn OccupancyPort> = if ctx.assembling() {
        let limits = settings.limits();
        let ready = Ready::new();
        let owners = Arc::new(settings.owners());
        let node = node_of(
            ctx.port::<ActiveRole>().map(|r| r.role()),
            env.site.partition.as_ref().map(|p| p.node.clone()),
        );
        let service = Occupancy::new(
            Arc::new(env.backend.clone()),
            Arc::clone(&env.permission.catalog),
            Arc::clone(ctx.clock()),
            node.clone(),
            Arc::clone(&owners) as Arc<dyn spot_occupancy::OwnerRoutes>,
            Arc::new(TracingAudit {
                site: site_name.clone(),
            }),
            limits,
        );
        let service = match service {
            Ok(service) => Arc::new(service),
            Err(e) => {
                // A catalog without the entity or the constraint, or a backend without range
                // exclusion: degrade, never serve.
                let reason = format!(
                    "the occupancy service refused its setup ({})",
                    e.kind.code()
                );
                tracing::warn!(site = %site_name, %reason, "occupancy is unavailable");
                ctx.add(Arc::new(Unavailable { reason }));
                return Ok(());
            }
        };
        let outbox = ctx.require_port::<dyn OutboxLease>()?;
        ctx.add(Arc::new(OccupancyService {
            backend: env.backend.clone(),
            occupancy: Arc::clone(&service),
            outbox,
            interval: Duration::from_secs(settings.sweep_interval),
            ready: Arc::clone(&ready),
        }));
        let identity = Arc::new(LiveIdentity::new(
            Arc::clone(env.live),
            ctx.port::<dyn ConsumerIdentity>(),
        ));
        let port: Arc<dyn OccupancyPort> = Arc::new(OccupancyApi::new(
            service,
            identity,
            owners,
            node,
            Arc::clone(ctx.clock()),
            ready,
            PortSettings {
                max_span: time::Duration::days(settings.max_span_days),
                max_ttl_seconds: settings.max_ttl_seconds,
                span: settings.span.span(),
            },
        ));
        built()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(site_name, Arc::clone(&port));
        ctx.provide::<dyn OccupancyPort>(Arc::clone(&port))?;
        port
    } else {
        let found = built()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&site_name)
            .cloned();
        match found {
            Some(port) => port,
            None => return Ok(()),
        }
    };
    let mut ports = Ports::new();
    ports.provide::<dyn OccupancyPort>(port);
    // Mounted like the other hosted groups: the host authenticates every request before the
    // router sees it, and the port authenticates again.
    let mounted = routes::mount(&[routes::occupancy::registration()], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the occupancy routes were not mounted"
        );
    }
    let router = mounted.authenticated;
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

/// The occupancy definition for a site whose bookable resource is the DocType `unit_doctype`
/// (re-exported so an app and the tests build the same entity the service checks).
pub fn entity_definition(unit_doctype: &str, span: SpanSetting) -> spot_meta::Definition {
    definition(unit_doctype, span.span())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_storage_contract::RecordId;

    #[test]
    fn the_defaults_validate_and_unknown_keys_are_refused() {
        assert!(Settings::default().validate().is_ok());
        assert_eq!(Settings::default().sweep_interval, 30);
        assert!(toml::from_str::<Settings>("sweep_interval_secs = 5").is_err());
        let parsed: Settings = toml::from_str("span = \"dates\"\nsweep_interval = 5").unwrap();
        assert_eq!(parsed.span, SpanSetting::Dates);
        assert_eq!(parsed.sweep_interval, 5);
        assert_eq!(parsed.enabled, None);
    }

    #[test]
    fn every_limit_is_range_checked_without_echoing_a_value() {
        let cases = [
            (
                "max_ttl_seconds",
                Settings {
                    max_ttl_seconds: 0,
                    ..Settings::default()
                },
            ),
            (
                "max_ttl_seconds",
                Settings {
                    max_ttl_seconds: 86_401,
                    ..Settings::default()
                },
            ),
            (
                "default_ttl_seconds",
                Settings {
                    default_ttl_seconds: 3601,
                    ..Settings::default()
                },
            ),
            (
                "sweep_interval",
                Settings {
                    sweep_interval: 0,
                    ..Settings::default()
                },
            ),
            (
                "max_span_days",
                Settings {
                    max_span_days: 3661,
                    ..Settings::default()
                },
            ),
            (
                "sweep_batch",
                Settings {
                    sweep_batch: 0,
                    ..Settings::default()
                },
            ),
        ];
        for (key, settings) in cases {
            let why = settings.validate().unwrap_err();
            assert!(why.contains(key), "{why}");
        }
    }

    #[test]
    fn homes_must_be_record_ids_with_an_edge_name() {
        let unit = RecordId::new_v7().to_string();
        let mut settings = Settings::default();
        settings.homes.insert(unit.clone(), "branch-1".into());
        assert!(settings.validate().is_ok());
        settings.homes.insert("not-an-id".into(), "branch-2".into());
        assert!(settings.validate().is_err());
        settings.homes.remove("not-an-id");
        settings
            .homes
            .insert(RecordId::new_v7().to_string(), String::new());
        assert!(settings.validate().is_err());
    }

    #[test]
    fn the_limits_carry_the_configured_values() {
        let limits = Settings::default().limits();
        assert_eq!(limits.default_ttl, Duration::from_secs(900));
        assert_eq!(limits.max_ttl, Duration::from_secs(3600));
        assert!(limits.validate().is_ok());
    }

    #[test]
    fn only_an_edge_with_a_partition_node_is_an_edge() {
        assert_eq!(node_of(None, None), Owner::Hub);
        assert_eq!(node_of(Some(Role::Hub), Some("n".into())), Owner::Hub);
        assert_eq!(
            node_of(Some(Role::Standalone), Some("n".into())),
            Owner::Hub
        );
        assert_eq!(node_of(Some(Role::Edge), None), Owner::Hub);
        assert_eq!(
            node_of(Some(Role::Edge), Some("n".into())),
            Owner::Edge("n".into())
        );
    }
}
