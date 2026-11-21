//! The fix pipeline, hosted (WP-AI-08.T4; blueprint WS13, ADR-26): the single path by which an
//! agent action above T1 changes a site. Plan, rehearse on a snapshot, apply, verify and roll back
//! automatically, with the change journal and the undo window in PostgreSQL and the side-effect
//! hold on while a change is unverified. Operator guide: `docs/running-spot/fix-pipeline.md`.
//!
//! # What it does
//!
//! `spot-fix-pipeline` is the engine (a write-ahead journal, a state machine, a residue check) and
//! `spot-fix-pipeline-pg` its durable journal. This module hosts them:
//!
//! - **It is the remedy host's apply path.** The remedy host (`services/remedies`) refuses to
//!   apply any remedy that is not a reversible T1 setting until a pipeline is installed. This
//!   module installs [`host::PipelinePath`] on it, so a T2 or T3 remedy an agent plans is applied
//!   only through [`host::FixHost::apply_remedy`].
//! - **The journal is durable.** Every transition and every step is written to
//!   `spot_control.spot_fix_journal` (append-only: no update, no delete, not even for the owner)
//!   *before* it happens, and every transition is on the AI audit chain. Pre-images are state
//!   fingerprints of every affected target.
//! - **A rehearsal never touches the live site.** The remedy is applied first on the rehearsal
//!   snapshot ([`WorldKind::Rehearsal`](crate::services::remedies::world::WorldKind)); only a clean
//!   rehearsal goes on.
//! - **External effects are held while a change is unverified.** The effect hold is engaged before
//!   the first live change; a verified fix releases it (held effects replay in order with their
//!   original keys), a rolled-back or failed one discards it.
//! - **One run per site at a time**: a second is refused, not queued.
//! - **A failed verification rolls back**, and only an empty residue (the pre-state fingerprint
//!   against the post-rollback one) is `rolled_back`; anything else is `failed`, never a false claim.
//! - **A verified fix stays undoable for `undo_window_secs`**, conflict-checked, journaled and
//!   audited.
//!
//! # The durable job
//!
//! The journal *is* the durable record of a run. [`host::FixHost::maintain`] folds it: at start
//! (the readiness check) and every `maintenance_seconds` it recovers fixes the journal shows
//! unfinished (a finished apply is verified, a rehearsal resumed) and closes the undo window of
//! verified fixes whose time is up. One limit is stated plainly: the remedy framework keeps its
//! undo log in memory, so a fix that was *applying* when the process ended cannot be reversed by
//! its remedy and ends `failed` with evidence for an operator (see the report's open question).
//!
//! # Level
//!
//! **Optional.** The pipeline needs the remedy host, a world to run remedies on, the effect hold
//! (unless `require_effect_hold = false`) and the PostgreSQL journal; when one is missing the module
//! is present but degraded (naming which) and does **not** install itself, so the remedy host keeps
//! refusing T2 and T3 applies. A failed recovery at start, or a hold that could not be settled,
//! degrades the site and says so.
//!
//! # Ports
//!
//! Provides [`host::FixHost`]; takes `RemedyHost` (required), `HoldService`, `dyn host::Snapshots`
//! and `dyn ports::FixChecks` (optional).

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use spot_effect_hold::HoldService;
use spot_fix_pipeline::{FixJournal, FixPolicy};
use spot_fix_pipeline_pg::PgFixJournal;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use tokio::sync::watch;

use crate::services::mcp::seams::audit_sink;
use crate::services::remedies::host::RemedyHost;
use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "fix_pipeline/host.rs"]
pub mod host;
#[path = "fix_pipeline/link.rs"]
pub mod link;
#[path = "fix_pipeline/ports.rs"]
pub mod ports;

use host::{Deps, EffectHold, FixHost, HoldAdapter, NoSnapshots, PipelinePath, Snapshots};
use ports::{FixChecks, NoChecks};

/// Start order: after the remedy host (1270) it installs itself on.
pub const ORDER: u32 = 1280;

/// Catalogue codes that head the sentences of settings and readiness reports.
const SETTINGS_INVALID: &str = "fix_pipeline_host.settings_invalid";
const RECOVERY_FAILED: &str = "fix_pipeline_host.recovery_failed";
const REMEDIES_UNAVAILABLE: &str = "fix_pipeline_host.remedies_unavailable";
const WORLD_UNAVAILABLE: &str = "fix_pipeline_host.world_unavailable";
const HOLD_UNAVAILABLE: &str = "fix_pipeline_host.hold_unavailable";

fn default_true() -> bool {
    true
}

fn default_undo_window() -> i64 {
    86_400
}

fn default_epoch() -> u64 {
    1
}

fn default_maintenance() -> u64 {
    60
}

/// `[services.fix_pipeline]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Host the pipeline. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How long a verified fix stays undoable, in seconds (60 to 30 days). Default one day.
    #[serde(default = "default_undo_window")]
    pub undo_window_secs: i64,
    /// Refuse to apply without the side-effect hold. Default true.
    #[serde(default = "default_true")]
    pub require_effect_hold: bool,
    /// The deployment epoch the audit records carry.
    #[serde(default = "default_epoch")]
    pub deployment_epoch: u64,
    /// Seconds between recovery and undo-window sweeps (10 to 3600).
    #[serde(default = "default_maintenance")]
    pub maintenance_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            undo_window_secs: default_undo_window(),
            require_effect_hold: true,
            deployment_epoch: default_epoch(),
            maintenance_seconds: default_maintenance(),
        }
    }
}

impl Settings {
    /// Checks the numbers.
    ///
    /// # Errors
    ///
    /// A refusal naming the key.
    pub fn check(&self) -> Result<()> {
        let refuse = |text: &str| Error(format!("{SETTINGS_INVALID}: {text}"));
        if !(60..=2_592_000).contains(&self.undo_window_secs) {
            return Err(refuse("undo_window_secs must be 60 to 2592000"));
        }
        if !(10..=3_600).contains(&self.maintenance_seconds) {
            return Err(refuse("maintenance_seconds must be 10 to 3600"));
        }
        Ok(())
    }

    /// The engine policy these map to.
    #[must_use]
    pub fn policy(&self) -> FixPolicy {
        FixPolicy::default().with_undo_window(self.undo_window_secs)
    }
}

/// The service: recovery at start, then the maintenance sweep.
struct FixService {
    host: Arc<FixHost>,
    interval: Duration,
}

impl Service for FixService {
    fn name(&self) -> &'static str {
        "fix_pipeline"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let report = self.host.maintain().await.map_err(|e| e.to_string())?;
            if report.failed > 0 {
                return Err(format!(
                    "{RECOVERY_FAILED}: {} unfinished fixes could not be recovered and ended failed",
                    report.failed
                ));
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut tick = tokio::time::interval(self.interval);
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = tick.tick() => {
                        if self.host.maintain().await.is_err() {
                            tracing::warn!("fix pipeline: the maintenance sweep failed; it runs again");
                        }
                    }
                }
            }
            Ok(())
        })
    }

    fn degraded(&self) -> Option<String> {
        self.host.degraded()
    }
}

/// A degraded entry: the module is present but cannot serve, for `reason`.
fn degraded(ctx: &mut SiteCtx<'_>, reason: impl Into<String>) {
    let reason: String = reason.into();
    ctx.add(Idle::new("fix_pipeline", Level::Optional, move || {
        let reason = reason.clone();
        Box::pin(async move { Err(reason) })
    }));
}

/// Builds the journal on a plain thread (the bridge starts its own runtime and refuses a thread
/// that belongs to one) and creates its schema.
fn build_journal(backend: &PgBackend, pool_size: usize) -> Result<Arc<dyn FixJournal>> {
    let largest = BridgeConfig::largest_in_flight(pool_size);
    if largest < 1 {
        return Err(Error(format!(
            "services.fix_pipeline needs a PostgreSQL pool of at least 3 connections \
             (postgres.pool_max_size is {pool_size})"
        )));
    }
    let config = BridgeConfig {
        name: "fix_pipeline".to_owned(),
        max_in_flight: largest.min(2),
        workers: 2,
        queue_capacity: 4,
        default_deadline: Duration::from_secs(10),
        max_deadline: Duration::from_secs(60),
    };
    let backend = backend.clone();
    std::thread::spawn(move || -> Result<Arc<dyn FixJournal>> {
        let bridge = Arc::new(
            Bridge::for_backend(config, &backend)
                .map_err(|e| Error(format!("services.fix_pipeline: {e}")))?,
        );
        let journal = PgFixJournal::new(bridge, &backend.pool())
            .map_err(|e| Error(format!("services.fix_pipeline: {}", e.code())))?;
        journal
            .ensure_schema()
            .map_err(|e| Error(format!("services.fix_pipeline: {}", e.code())))?;
        Ok(Arc::new(journal))
    })
    .join()
    .map_err(|_| Error("the fix pipeline journal thread failed".to_owned()))?
}

/// Registers the fix pipeline: its journal, its readiness entry, the [`FixHost`] port, and its
/// apply path on the remedy host. Inert when `services.fix_pipeline.enabled = false`.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("fix_pipeline")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    settings.check()?;
    let Some(remedies) = ctx.port::<RemedyHost>() else {
        degraded(
            ctx,
            format!(
                "{REMEDIES_UNAVAILABLE}: the remedy host is not running, so there is nothing to apply"
            ),
        );
        return Ok(());
    };
    if !remedies.has_world() {
        degraded(
            ctx,
            format!(
                "{WORLD_UNAVAILABLE}: no world is provided for remedies, so a fix can be neither rehearsed nor verified"
            ),
        );
        return Ok(());
    }
    let hold: Option<Arc<dyn EffectHold>> = ctx
        .port::<HoldService>()
        .map(|service| Arc::new(HoldAdapter(service)) as Arc<dyn EffectHold>);
    if settings.require_effect_hold && hold.is_none() {
        degraded(
            ctx,
            format!(
                "{HOLD_UNAVAILABLE}: the effect hold is not running and require_effect_hold is on"
            ),
        );
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let journal = build_journal(env.backend, env.loaded.common.postgres.pool_max_size)?;
    let snapshots: Arc<dyn Snapshots> = ctx
        .port::<dyn Snapshots>()
        .unwrap_or_else(|| Arc::new(NoSnapshots));
    let checks: Arc<dyn FixChecks> = ctx
        .port::<dyn FixChecks>()
        .unwrap_or_else(|| Arc::new(NoChecks));
    let host = Arc::new(FixHost::new(Deps {
        site: site.clone(),
        remedies: Arc::clone(&remedies),
        journal,
        sink: audit_sink(env.backend, &site),
        hold,
        require_hold: settings.require_effect_hold,
        snapshots,
        checks,
        policy: settings.policy(),
        epoch: settings.deployment_epoch,
        clock: Arc::clone(ctx.clock()),
    }));
    remedies.install_apply_path(Arc::new(PipelinePath(Arc::clone(&host))));
    ctx.add(Arc::new(FixService {
        host: Arc::clone(&host),
        interval: Duration::from_secs(settings.maintenance_seconds),
    }));
    ctx.provide(host)
        .map_err(|e| Error(format!("services.fix_pipeline: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on_and_valid() {
        let s = Settings::default();
        assert!(s.enabled && s.require_effect_hold);
        assert!(s.check().is_ok());
        assert_eq!(s.policy().undo_window_secs, 86_400);
        let parsed: Settings = toml::from_str("").unwrap();
        assert_eq!(parsed.maintenance_seconds, 60);
        assert!(toml::from_str::<Settings>("nope = 1").is_err());
    }

    #[test]
    fn numbers_out_of_range_are_refused() {
        for (undo, every) in [(59, 60), (86_400, 9), (86_400, 3_601), (2_592_001, 60)] {
            let s = Settings {
                undo_window_secs: undo,
                maintenance_seconds: every,
                ..Settings::default()
            };
            assert!(s.check().is_err(), "{undo} {every}");
        }
    }
}
