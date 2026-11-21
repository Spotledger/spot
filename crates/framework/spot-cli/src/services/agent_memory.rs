//! Agent memory, hosted (WP-AI-09.T3; blueprint WS13, ADR-26). Operator guide:
//! `docs/running-spot/agent-memory.md`.
//!
//! `spot-agent-memory` is the model (episodes, procedures, deployment facts, the sanitiser, recall,
//! retention, the learning loop) and `spot-agent-memory-pg` the durable store on PostgreSQL with
//! pgvector. This module hosts them on a site:
//!
//! - **A memory database of its own, per tenant.** At first use the tenant's memory gets a
//!   **separate database and a separate role** on the control server, with no grant on any business
//!   database, and the `vector` extension in the memory database only ([`host`]). Provisioning is
//!   idempotent: a restart, or a resume after a crash, converges. The boundary is enforced by
//!   database privilege (PostgreSQL has no cross-database query), not only by code.
//! - **A write filter** ([`filter`]) on every write: the sanitiser again, and a refusal of any text
//!   that contains a field value of the record the entry is about. Every verdict is counted and
//!   audited; a refusal names the class, never the match.
//! - **Recall for the agent path** ([`facade::AgentMemory::recall`]): vector similarity ranked in
//!   the database with the structured filters, tier-checked (a procedure above the caller's tier
//!   is not returned, counted or hinted at) and tenant-checked. The tier comes from the
//!   authenticated principal (the agent directory's stored registration), never from an argument.
//! - **A retention job** on a schedule: the held lease `agent_memory_retention` runs one pass per
//!   `retention_interval_seconds` (plan, then apply exactly the plan: TTLs, caps, compaction of old
//!   episodes into statistics), legal holds from `holds_file` suspend it for what they cover, and a
//!   hold file that cannot be read stops the pass. The job `platform:agent_memory.retention` runs a
//!   pass on demand.
//! - **The learned-remedy gate** ([`facade::AgentMemory::remedy_usable`]): a candidate is usable
//!   only when CI promoted it; the table's own `CHECK` refuses a promotion with no CI run.
//!
//! # Off by default, per tenant
//!
//! Nothing is created, opened or started unless `enabled = true` **and** the site is listed in
//! `tenants`. Then the memory role's password comes from `password_file` (16 to 128 characters; the
//! provisioner refuses anything else before it uses it); the control connection that creates the
//! database and role is the site configuration's administrative connection.
//!
//! # Settings (`[services.agent_memory]`)
//!
//! `enabled` (false), `tenants` (empty), `password_file`, `retention_interval_seconds` (3600),
//! `holds_file`, `max_recall_k` (20) and `[services.agent_memory.retention]` (the per-kind TTLs and
//! caps of `spot_agent_memory::RetentionPolicy`). Unknown keys and out-of-range values stop the site
//! from starting, naming the key.
//!
//! # Level
//!
//! **Optional.** If pgvector is missing on the server, the memory database cannot be provisioned,
//! the password file is wrong, or the embedding provider or the coordinator lease is absent, the
//! site serves and readiness says `degraded: agent_memory (<what and the fix>)`; no memory port is
//! usable and nothing is written. A missing `vector` extension says to install the pgvector package
//! (creating an extension is a privileged step).
//!
//! # What this module does not do
//!
//! It calls no model: embeddings come from the provider port (the LLM service). It never runs as an
//! administrator for an agent's call: recall and write run as the calling agent's tier. It reads a
//! business record only as the platform, only to compare values for the write filter, and stores
//! none of it.

#[path = "agent_memory/facade.rs"]
pub mod facade;
#[path = "agent_memory/filter.rs"]
pub mod filter;
#[path = "agent_memory/host.rs"]
pub mod host;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use spot_agent_memory::{Embedder, RetentionPolicy, Sanitiser};
use spot_storage_contract::Value;
use tokio::sync::watch;

use self::facade::{AgentMemory, Limits, MemoryFault, Parts};
use self::filter::{Fut, RecordValues, WriteFilter};
use crate::services::ai_audit::AiAuditPort;
use crate::services::ports::{ConsumerLease, OutboxLease, SystemReader};
use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the AI audit chain (1060), the agent tier service (1070) and the LLM service
/// (1260), whose ports it takes.
pub const ORDER: u32 = 1280;

/// The job that runs one retention pass on demand.
pub const RETENTION_JOB: &str = "platform:agent_memory.retention";

/// The coordinator lease the scheduled retention pass holds.
pub const RETENTION_LEASE: &str = "agent_memory_retention";

/// How long a standby node waits before asking for the lease again.
const STANDBY: Duration = Duration::from_secs(10);

fn default_interval() -> u64 {
    3_600
}
fn default_max_k() -> usize {
    20
}

/// `[services.agent_memory]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The master switch. Off by default: nothing is created.
    pub enabled: bool,
    /// The sites (tenants) agent memory is on for.
    pub tenants: Vec<String>,
    /// The file holding the memory role's password.
    pub password_file: Option<PathBuf>,
    /// Seconds between scheduled retention passes, 60 to 86,400.
    #[serde(default = "default_interval")]
    pub retention_interval_seconds: u64,
    /// The legal-hold file (`spot-data-policy` holds TOML), read at every pass.
    pub holds_file: Option<PathBuf>,
    /// The most hits one recall returns, 1 to 100.
    #[serde(default = "default_max_k")]
    pub max_recall_k: usize,
    /// The per-kind TTLs and caps.
    pub retention: RetentionPolicy,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            tenants: Vec::new(),
            password_file: None,
            retention_interval_seconds: default_interval(),
            holds_file: None,
            max_recall_k: default_max_k(),
            retention: RetentionPolicy::default(),
        }
    }
}

impl Settings {
    /// `Err(key)` for a value out of range. Only checked when the module is enabled.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(60..=86_400).contains(&self.retention_interval_seconds) {
            return Err("retention_interval_seconds");
        }
        if !(1..=100).contains(&self.max_recall_k) {
            return Err("max_recall_k");
        }
        if self.tenants.iter().any(|tenant| tenant.trim().is_empty()) {
            return Err("tenants");
        }
        Ok(())
    }

    /// Whether agent memory is on for `site`.
    #[must_use]
    pub fn covers(&self, site: &str) -> bool {
        self.enabled && self.tenants.iter().any(|tenant| tenant == site)
    }
}

/// The port other modules take: the tenant's memory once the service has provisioned it.
#[derive(Default)]
pub struct MemoryHandle {
    cell: OnceLock<Arc<AgentMemory>>,
}

impl MemoryHandle {
    /// The memory, or `None` until the start-up check provisioned it (and for ever when it could
    /// not).
    #[must_use]
    pub fn get(&self) -> Option<Arc<AgentMemory>> {
        self.cell.get().cloned()
    }

    fn set(&self, memory: Arc<AgentMemory>) {
        let _ = self.cell.set(memory);
    }
}

/// [`RecordValues`] over the platform's document reader: the text fields of the record an entry is
/// about, only ever compared and dropped.
struct ReaderValues(Arc<dyn SystemReader>);

impl RecordValues for ReaderValues {
    fn values<'a>(&'a self, doctype: &'a str, id: &'a str) -> Fut<'a, Vec<String>> {
        Box::pin(async move {
            match self.0.read(doctype, id).await {
                Ok(Some(document)) => document
                    .fields()
                    .values()
                    .filter_map(|value| match value {
                        Value::Text(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            }
        })
    }
}

/// What the service is built from.
struct ServiceParts {
    site: spot_core::SiteId,
    admin: spot_postgres::PgConfig,
    collation: String,
    password_file: PathBuf,
    settings: Settings,
    embedder: Arc<dyn Embedder>,
    lease: Arc<dyn OutboxLease>,
    reader: Option<Arc<dyn SystemReader>>,
    audit: Option<Arc<dyn AiAuditPort>>,
    clock: Arc<dyn spot_core::Clock>,
    handle: Arc<MemoryHandle>,
}

/// The service: the start-up check (provision, open, prepare) and the scheduled retention.
struct MemoryService {
    parts: ServiceParts,
}

impl MemoryService {
    fn now_ms(&self) -> i64 {
        self.parts
            .clock
            .now_utc()
            .unix_timestamp()
            .saturating_mul(1_000)
    }

    /// One scheduled pass on a blocking thread; logs counts and a word, never an entry.
    async fn pass(&self, memory: Arc<AgentMemory>) {
        let outcome = tokio::task::spawn_blocking(move || memory.run_retention_now()).await;
        match outcome {
            Ok(Ok(report)) => {
                if !report.is_empty() {
                    tracing::info!(
                        target: "spot::agent_memory",
                        site = %self.parts.site.as_str(),
                        removed = facade::removed(&report),
                        facts = report.facts.len(),
                        "agent memory retention pass"
                    );
                }
            }
            Ok(Err(fault)) => tracing::warn!(
                target: "spot::agent_memory",
                site = %self.parts.site.as_str(),
                fault = fault.word(),
                "an agent memory retention pass failed; the next pass retries"
            ),
            Err(_) => tracing::warn!(
                target: "spot::agent_memory",
                "an agent memory retention pass did not finish"
            ),
        }
    }

    /// Runs the schedule as the lease holder; `true` means the node was stopped.
    async fn lead(&self, stop: &mut watch::Receiver<bool>, mut lease: ConsumerLease) -> bool {
        let interval = Duration::from_secs(self.parts.settings.retention_interval_seconds);
        let tick = interval.min(Duration::from_secs(10));
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last = tokio::time::Instant::now();
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = ticker.tick() => {
                    match self.parts.lease.renew(&lease).await {
                        Ok(renewed) => lease = renewed,
                        Err(_) => break false,
                    }
                    if !matches!(self.parts.lease.still_held(&lease).await, Ok(true)) {
                        break false;
                    }
                    if last.elapsed() >= interval {
                        last = tokio::time::Instant::now();
                        if let Some(memory) = self.parts.handle.get() {
                            self.pass(memory).await;
                        }
                    }
                }
            }
        };
        let _ = self.parts.lease.release(&lease).await;
        stopped
    }
}

impl Service for MemoryService {
    fn name(&self) -> &'static str {
        "agent_memory"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let parts = &self.parts;
            let tenant = host::tenant_of(&parts.site).map_err(|f| f.degraded_text())?;
            let password =
                host::read_password(&parts.password_file).map_err(|f| f.degraded_text())?;
            let runtime = host::runtime().map_err(|f| f.degraded_text())?;
            let now_ms = self.now_ms();
            let (admin, collation) = (parts.admin.clone(), parts.collation.clone());
            let opened = tokio::task::block_in_place(|| {
                host::provision_and_open(&admin, &collation, &tenant, &password, now_ms, runtime)
            })
            .map_err(|failure| failure.degraded_text())?;
            let sanitiser = Sanitiser::standard()
                .map_err(|_| "agent_memory: the sanitiser could not be built".to_owned())?;
            let filter = WriteFilter::new(
                parts.site.as_str(),
                sanitiser,
                parts.reader.as_ref().map(|reader| {
                    Arc::new(ReaderValues(Arc::clone(reader))) as Arc<dyn RecordValues>
                }),
                parts.audit.clone(),
                Arc::clone(&parts.clock),
            );
            parts.handle.set(Arc::new(AgentMemory::new(Parts {
                tenant,
                store: opened.store,
                embedder: Arc::clone(&parts.embedder),
                filter,
                clock: Arc::clone(&parts.clock),
                retention: parts.settings.retention.clone(),
                holds_file: parts.settings.holds_file.clone(),
                limits: Limits {
                    max_k: parts.settings.max_recall_k,
                },
            })));
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                if let Ok(lease) = self.parts.lease.acquire(RETENTION_LEASE).await
                    && self.lead(&mut stop, lease).await
                {
                    return Ok(());
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(STANDBY) => {}
                }
            }
        })
    }
}

/// A degraded entry: the module is on for this site but cannot serve, for `reason`.
fn degraded(ctx: &mut SiteCtx<'_>, reason: &'static str) {
    ctx.add(Idle::new("agent_memory", Level::Optional, move || {
        Box::pin(async move { Err(reason.to_owned()) })
    }));
}

/// Registers agent memory for a site that turned it on.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("agent_memory")?;
    if !settings.enabled {
        return Ok(());
    }
    settings
        .validate()
        .map_err(|key| Error(format!("services.agent_memory.{key}: out of range")))?;
    let env = *ctx.env()?;
    let site = spot_core::SiteId::new(&env.site.site).map_err(crate::error)?;
    if !settings.covers(site.as_str()) || !ctx.assembling() {
        return Ok(());
    }
    let Some(password_file) = settings.password_file.clone() else {
        degraded(
            ctx,
            "agent_memory: `password_file` is not set; it names the file holding the memory role's password",
        );
        return Ok(());
    };
    let Some(embedder) = ctx.port::<dyn Embedder>() else {
        degraded(
            ctx,
            "agent_memory: no embedding provider is available (turn the LLM service on and configure an embedding provider), so memory cannot embed and stays off",
        );
        return Ok(());
    };
    let Some(lease) = ctx.port::<dyn OutboxLease>() else {
        degraded(
            ctx,
            "agent_memory: the coordinator lease port is not available, so the retention schedule cannot run",
        );
        return Ok(());
    };
    let handle = Arc::new(MemoryHandle::default());
    let job_handle = Arc::clone(&handle);
    let (reader, audit, clock) = (
        ctx.port::<dyn SystemReader>(),
        ctx.port::<dyn AiAuditPort>(),
        Arc::clone(ctx.clock()),
    );
    ctx.add(Arc::new(MemoryService {
        parts: ServiceParts {
            site,
            admin: env.loaded.admin.clone(),
            collation: env.site.collation.clone(),
            password_file,
            settings,
            embedder,
            lease,
            reader,
            audit,
            clock,
            handle: Arc::clone(&handle),
        },
    }));
    ctx.job_handlers().register(RETENTION_JOB, move |_job| {
        let handle = Arc::clone(&job_handle);
        Box::pin(async move {
            let Some(memory) = handle.get() else {
                return Ok(spot_jobs::worker::HandlerResult::Retry(
                    "agent memory is not provisioned on this node yet".to_owned(),
                ));
            };
            // Idempotent, so any node may run it; a hold file that cannot be read retries.
            let done = tokio::task::spawn_blocking(move || memory.run_retention_now()).await;
            Ok(match done {
                Ok(Ok(_)) => spot_jobs::worker::HandlerResult::Ok,
                Ok(Err(MemoryFault::Holds)) => spot_jobs::worker::HandlerResult::Retry(
                    "the legal-hold file could not be read".to_owned(),
                ),
                _ => {
                    spot_jobs::worker::HandlerResult::Retry("the retention pass failed".to_owned())
                }
            })
        })
    });
    ctx.provide(handle)
        .map_err(|e| Error(format!("services.agent_memory: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn agent_memory_is_off_by_default_and_per_tenant() {
        let parsed: Settings = toml::from_str("").unwrap();
        assert!(!parsed.enabled && parsed.tenants.is_empty() && parsed.password_file.is_none());
        let on: Settings = toml::from_str("enabled = true\ntenants = [\"acme\"]\n").unwrap();
        assert!(on.covers("acme") && !on.covers("globex"));
        assert!(toml::from_str::<Settings>("enabled = true\nverify = 1\n").is_err());
        assert!(toml::from_str::<Settings>("[retention]\nsurprise = 1\n").is_err());
    }

    #[test]
    fn out_of_range_settings_name_their_key() {
        let ok = Settings {
            enabled: true,
            tenants: vec!["acme".to_owned()],
            ..Settings::default()
        };
        assert_eq!(ok.validate(), Ok(()));
        for (settings, key) in [
            (
                Settings {
                    retention_interval_seconds: 5,
                    ..ok.clone()
                },
                "retention_interval_seconds",
            ),
            (
                Settings {
                    max_recall_k: 0,
                    ..ok.clone()
                },
                "max_recall_k",
            ),
            (
                Settings {
                    tenants: vec![" ".to_owned()],
                    ..ok.clone()
                },
                "tenants",
            ),
        ] {
            assert_eq!(settings.validate(), Err(key));
        }
    }

    #[test]
    fn the_retention_table_takes_the_crates_own_keys() {
        let parsed: Settings =
            toml::from_str("[retention.episode]\nttl_days = 30\nmax_entries = 100\n").unwrap();
        assert_eq!(parsed.retention.episode.ttl_days, Some(30));
        assert_eq!(parsed.retention.episode.max_entries, 100);
    }
}
