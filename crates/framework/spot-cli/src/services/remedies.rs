//! The remedy catalogue, hosted (WP-AI-07.T2; blueprint WS13, ADR-26): the shipped remedies
//! loaded into the site, exposed to the agent call path through the tier authoriser, dry-run and
//! undo as audited operations, and every remedy's self-test run against a rehearsal world.
//! Operator guide: `docs/running-spot/remedies.md`.
//!
//! # What it does
//!
//! `spot-remedies` is the framework (typed remedies with a precondition, a mandatory dry-run, an
//! undo and a self-test; a registry that gates by tier). This module hosts it on a site:
//!
//! - **The registry is loaded from the shipped catalogue** ([`world::ShippedCatalogue`], or a
//!   larger one another module provides as `dyn Catalogue`). A catalogue that breaks a registry
//!   rule (a duplicate id, an irreversible remedy above T3) stops the site from starting.
//! - **Listing is tier-filtered; dry-run changes nothing; apply and undo are audited**: see
//!   [`host`] for the rules. The agent's tier comes from the agent tier service's stored data
//!   (its effective tier is T0 while `services.agent_tiers.unattended_policy_open` is false, so no
//!   remedy above T0 runs until that gate is opened).
//! - **Apply goes through the fix pipeline** ([`host::ApplyPath`], installed by
//!   `services/fix_pipeline`) for every remedy above T1 or not reversible; until it is installed
//!   those applies are refused with `remedies_host.apply_closed` and only dry-run and reversible
//!   T1 settings are enabled.
//! - **Every remedy's self-test runs** on the in-memory rehearsal world at start (set
//!   `self_test_at_start = false` to skip) and in CI (`tests/remedies_service.rs`): a remedy
//!   whose own scenario fails (precondition, dry-run, apply, invariants, undo) degrades the site
//!   and names the remedy.
//!
//! # The world
//!
//! A remedy changes the site through the typed ports of the framework (jobs, cache, retrieval
//! index, edge sync, quarantine, BPM cases, documents), never a raw handle. The implementation of
//! those ports is a seam ([`world::WorldAccess`], a port the owning modules provide): none is
//! provided by this task, so on a real site the host lists and self-tests but cannot run a remedy
//! (`remedies_host.no_world`). That is deliberate: a remedy must never run on a stand-in.
//!
//! # Level
//!
//! **Optional.** The module needs the agent tier service (the tier source): without it the
//! module is present but degraded (`remedies_host.tiers_unavailable`) and no host is provided, so
//! an agent path with no tier source is closed rather than unguarded. A failed self-test or a
//! failing check degrades the site; it never stops it.
//!
//! # Ports
//!
//! Provides [`host::RemedyHost`]; takes `TierService` (required), `dyn world::WorldAccess`,
//! `dyn world::Catalogue` and `dyn host::ApplyPath` (optional). Ports flow forward only and the fix
//! pipeline registers after this module, so it installs its apply path with
//! [`host::RemedyHost::install_apply_path`].

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain};
use spot_core::Clock;
use spot_remedies::RemedySettings;
use tokio::sync::watch;

use crate::services::agent_tiers::TierService;
use crate::services::mcp::McpHandle;
use crate::services::mcp::seams::{AgentDirectory, audit_sink};
use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "remedies/host.rs"]
pub mod host;
#[path = "remedies/tools.rs"]
pub mod tools;
#[path = "remedies/undo_store.rs"]
pub mod undo_store;
#[path = "remedies/world.rs"]
pub mod world;

use host::{AgentSource, Deps, Fut, RemedyHost, UNDO_RESTORE_FAILED, UNDO_STORE_UNAVAILABLE};
use tools::RemedyTool;
use undo_store::PgUndoStore;
use world::{Catalogue, ShippedCatalogue, WorldAccess};

/// Start order: after the agent tier service (1070) and the AI audit modules, before the fix
/// pipeline (1280) that installs the apply path.
pub const ORDER: u32 = 1270;

/// Catalogue codes that head the sentences of settings and readiness reports.
const SETTINGS_INVALID: &str = "remedies_host.settings_invalid";
const SELF_TEST_FAILED: &str = "remedies_host.self_test_failed";
const TIERS_UNAVAILABLE: &str = "remedies_host.tiers_unavailable";
/// The code of the readiness text when the MCP service refused one of the four tools (no code of
/// its own: it is an internal failure of the wiring).
const TOOLS_REFUSED: &str = "remedies_host.internal";

/// How often the service sweeps the undo records whose window has closed.
const SWEEP_EVERY: Duration = Duration::from_secs(300);

fn default_true() -> bool {
    true
}

fn default_undo_window() -> i64 {
    86_400
}

/// `[services.remedies]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Host the remedy registry. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How long an undo record stays usable, in seconds (60 to 30 days). Default one day.
    #[serde(default = "default_undo_window")]
    pub undo_window_secs: i64,
    /// Run every remedy's self-test when the site starts. Default true.
    #[serde(default = "default_true")]
    pub self_test_at_start: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            undo_window_secs: default_undo_window(),
            self_test_at_start: true,
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
        if !(60..=2_592_000).contains(&self.undo_window_secs) {
            return Err(Error(format!(
                "{SETTINGS_INVALID}: undo_window_secs must be 60 to 2592000"
            )));
        }
        Ok(())
    }

    /// The framework settings these map to (support domain).
    #[must_use]
    pub fn framework(&self) -> RemedySettings {
        RemedySettings {
            undo_window_secs: self.undo_window_secs,
            domain: Domain::Support,
        }
    }
}

/// The agent source over the agent tier service's stored registrations.
pub struct TierSource {
    service: Arc<TierService>,
    clock: Arc<dyn Clock>,
}

impl TierSource {
    /// A source over `service`.
    #[must_use]
    pub fn new(service: Arc<TierService>, clock: Arc<dyn Clock>) -> Self {
        Self { service, clock }
    }
}

impl AgentSource for TierSource {
    fn identity<'a>(
        &'a self,
        tenant: &'a str,
        agent_id: &'a str,
    ) -> Fut<'a, Option<AgentIdentity>> {
        Box::pin(async move {
            // The directory answers with the effective tier: T0 while the unattended policy is closed.
            let resolved = self.service.resolve(tenant, agent_id).await?;
            let reg = self.service.store().agent(agent_id).await.ok()??;
            let identity = AgentIdentity::new(&reg.agent_id, &reg.owner, reg.domain, resolved.tier)
                .with_status(reg.status);
            let mut registry = AgentRegistry::new();
            registry.enroll(identity).ok()?;
            if reg.deep && resolved.tier == reg.max_tier {
                let at = self.clock.now_utc().unix_timestamp();
                // A failure leaves the agent not deep, which only narrows what it may run.
                let _ = registry.register_deep(agent_id, "remedy host", "stored registration", at);
            }
            registry.get(agent_id).cloned()
        })
    }
}

/// What the service needs to put its four tools on the MCP service and take them off again.
struct ToolWiring {
    mcp: Arc<McpHandle>,
    site: String,
    clock: Arc<dyn Clock>,
}

/// The service: the readiness entry and the start-up self-test.
struct RemediesService {
    host: Arc<RemedyHost>,
    self_test: bool,
    /// The MCP service, when it runs: the tools are on it while this service is healthy.
    tools: Option<ToolWiring>,
}

impl Service for RemediesService {
    fn name(&self) -> &'static str {
        "remedies"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            // Restore first: a token handed out before a restart must be undoable even when the
            // self-test is switched off.
            let mut problems = Vec::new();
            match self.host.restore_undo_records().await {
                Ok(report) if report.failed > 0 => problems.push(format!(
                    "{UNDO_RESTORE_FAILED}: {} undo records could not be restored",
                    report.failed
                )),
                Ok(_) => {}
                Err(error) => problems.push(error.to_string()),
            }
            if self.self_test {
                let results = self.host.self_test_all().map_err(|e| e.sentence())?;
                let failed: Vec<String> = results
                    .iter()
                    .filter(|r| r.outcome.is_err())
                    .map(|r| r.remedy.as_str().to_owned())
                    .collect();
                if !failed.is_empty() {
                    problems.push(format!(
                        "{SELF_TEST_FAILED}: {} remedies fail their own scenario ({})",
                        failed.len(),
                        failed.join(", ")
                    ));
                }
            }
            // The tools are offered while the service is healthy and taken off while it is not
            // (an extension tool is absent while its service is degraded).
            if let Some(wiring) = &self.tools {
                if problems.is_empty() {
                    RemedyTool::install(&wiring.mcp, &self.host, &wiring.site, &wiring.clock)
                        .map_err(|reason| format!("{TOOLS_REFUSED}: {reason}"))?;
                } else {
                    RemedyTool::uninstall(&wiring.mcp);
                }
            }
            if problems.is_empty() {
                Ok(())
            } else {
                Err(problems.join("; "))
            }
        })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            if let Some(wiring) = &self.tools {
                RemedyTool::uninstall(&wiring.mcp);
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut tick = tokio::time::interval(SWEEP_EVERY);
            loop {
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = tick.tick() => {
                        if self.host.sweep_undo().await.is_err() {
                            tracing::warn!(
                                code = UNDO_STORE_UNAVAILABLE,
                                "remedies: the undo sweep failed; it runs again"
                            );
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

/// Registers the remedy host: its readiness entry and the [`RemedyHost`] port. Inert when
/// `services.remedies.enabled = false`; degraded when the agent tier service is not running.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("remedies")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    settings.check()?;
    let Some(tiers) = ctx.port::<TierService>() else {
        ctx.add(Idle::new("remedies", Level::Optional, || {
            Box::pin(async {
                Err(format!("{TIERS_UNAVAILABLE}: the agent tier service is not running, so there is no tier source and the remedy host stays closed"))
            })
        }));
        return Ok(());
    };
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let catalogue: Arc<dyn Catalogue> = ctx
        .port::<dyn Catalogue>()
        .unwrap_or_else(|| Arc::new(ShippedCatalogue));
    let world: Option<Arc<dyn WorldAccess>> = ctx.port::<dyn WorldAccess>();
    let apply_path = ctx.port::<dyn host::ApplyPath>();
    let clock = Arc::clone(ctx.clock());
    let host = RemedyHost::new(Deps {
        site: site.clone(),
        catalogue,
        settings: settings.framework(),
        world,
        agents: Arc::new(TierSource::new(tiers, Arc::clone(&clock))),
        apply_path,
        sink: audit_sink(env.backend, &site),
        clock,
    })
    .map_err(|e| Error(format!("services.remedies: {}", e.sentence())))?
    .with_undo_store(Arc::new(PgUndoStore::new(env.backend.pool())));
    if host.prepare_undo_store().await.is_err() {
        // A remedy applied with no way back is never left standing, so a host that cannot keep
        // undo records stays closed.
        ctx.add(Idle::new("remedies", Level::Optional, || {
            Box::pin(async {
                Err(format!("{UNDO_STORE_UNAVAILABLE}: the undo table could not be created, so the remedy host stays closed"))
            })
        }));
        return Ok(());
    }
    let host = Arc::new(host);
    // The MCP service is absent when it is off: no tools to offer, not an error. The tools are
    // installed here and again by the check each time the service is healthy.
    let tools = ctx.port::<McpHandle>().map(|mcp| ToolWiring {
        mcp,
        site: site.clone(),
        clock: Arc::clone(ctx.clock()),
    });
    if let Some(wiring) = &tools
        && let Err(reason) = RemedyTool::install(&wiring.mcp, &host, &wiring.site, &wiring.clock)
    {
        let sentence = format!("{TOOLS_REFUSED}: {reason}");
        ctx.add(Idle::new("remedies", Level::Optional, move || {
            let sentence = sentence.clone();
            Box::pin(async move { Err(sentence) })
        }));
        return Ok(());
    }
    ctx.add(Arc::new(RemediesService {
        host: Arc::clone(&host),
        self_test: settings.self_test_at_start,
        tools,
    }));
    ctx.provide(host)
        .map_err(|e| Error(format!("services.remedies: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on_and_valid() {
        let s = Settings::default();
        assert!(s.enabled && s.self_test_at_start);
        assert!(s.check().is_ok());
        assert_eq!(s.framework().undo_window_secs, 86_400);
        let parsed: Settings = toml::from_str("").unwrap();
        assert_eq!(parsed.undo_window_secs, 86_400);
        assert!(toml::from_str::<Settings>("nope = 1").is_err());
    }

    #[test]
    fn the_undo_window_is_bounded() {
        for bad in [0, 59, 2_592_001] {
            let s = Settings {
                undo_window_secs: bad,
                ..Settings::default()
            };
            assert!(s.check().is_err(), "{bad}");
        }
    }
}
