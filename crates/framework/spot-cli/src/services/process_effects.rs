//! Request effects and commit points, hosted for one site (WP-BPM-15.T4; ADR-80 section 6,
//! ADR-45).
//!
//! `spot-bpm-effects` (the request record file, rewind and undo), `spot-effect-hold` (the
//! side-effect hold), `spot-bpm-commit` (commit points) and `spot-bpm-effect-checks` (design-time
//! checks) are libraries: none ran in the server. This module hosts the first three for the
//! process engine:
//!
//! - **The request record file.** Every operation that leaves the system writes a receipt, keyed
//!   by `(request, node, attempt, operation)`, to an append-only, site-scoped log before it is
//!   recorded in memory; a receipt holds ids, counts and digests only (no secret, no payload body,
//!   no document value). After a restart the file is rebuilt by replaying the log.
//! - **Undo and rewind with compensation.** Cancelling a Run or taking an undo edge runs the
//!   declared compensation as a resumable job: each reversal is idempotent per receipt (the
//!   request key is journaled before and after the lifecycle call), and one that cannot be
//!   reversed stops the sequence with the Run in the stable state `needs_attention` and a
//!   supervisor notice, never silently skipped. The lease holder resumes interrupted undos.
//!   Reversals run as the acting principal, never as a widened or system identity; who may start
//!   an undo of somebody else's request is the `Supervisors` seam, and a source that cannot answer
//!   denies.
//! - **Effect hold and commit points.** External effects of the nodes before a commit point are
//!   held in the request's scope and released, in order and exactly once per key, at the commit
//!   point; a request cancelled first discards them and records that.
//! - **Graph.** `GraphDependants` uses the hosted business graph when a module provides it; a
//!   graph that errors denies the undo (`bpm_effects.graph_unavailable`). With no graph, only the
//!   links inside the file count and readiness says so.
//! - **Partner.** Compensation messages to another organisation go through the conversations
//!   module when it provides a `PartnerPort`; otherwise they are refused with a stable code. This
//!   module does not implement them.
//!
//! # Threads
//!
//! The undo library is synchronous. An undo is planned and executed on a blocking thread
//! (`spawn_blocking`); the reversal port and the journal reach back to the runtime through its
//! handle, so no database call runs on a runtime worker thread.
//!
//! # Fail closed
//!
//! A permission source that cannot answer denies; a graph that cannot answer denies the undo; a
//! dispatcher that is not registered leaves a held effect held; an unreachable journal leaves the
//! undo resumable instead of reversing twice. Every refusal is audited with who, what and which
//! rule.
//!
//! # Configuration
//!
//! `[services.process_effects]`: `enabled`, `tick_secs`, `stale_secs`, `max_resume_per_tick` and
//! `supervisor_roles`. See `docs/running-spot/process_effects.md`.
//!
//! # Audit and logs
//!
//! Rows of `spot_bpm_eff_audit`; log lines carry ids and stable code tokens, never a payload, a
//! receipt, a reason or a message body; counters carry no label.

#[path = "process_effects/error.rs"]
mod error;
#[path = "process_effects/granted.rs"]
mod granted;
#[path = "process_effects/hold.rs"]
mod hold;
#[path = "process_effects/host.rs"]
mod host;
#[path = "process_effects/ledger.rs"]
mod ledger;
#[path = "process_effects/reversal.rs"]
mod reversal;
#[path = "process_effects/settings.rs"]
mod settings;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use spot_app_api::BoxFuture;
use spot_bpm_effects::{
    Declarations, GraphDependants, NoGraph, PartnerPort, ReversalPort, StaticDeclarations,
};
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_effect_hold::{
    AuditEvent, AuditSink, Dispatcher, EffectRecord, Error as HoldError, LedgerStore, Notice,
    Notifier, Result as HoldResult,
};
use spot_effect_hold_pg::PgLedger;
use tokio::sync::watch;

pub use error::EffError;
pub use granted::{GrantedReversal, LiveProbe, UNCONFIRMED, VersionProbe, governed_doctypes};
pub use hold::{HoldHost, HoldParts, NoPlans, PlanSource};
pub use host::{
    AttentionNotice, EffectNotice, EffectsHost, Recorded, Supervisors, UndoReport, Wiring, micros,
};
pub use ledger::{EffLedger, LedgerError, LogRow, Resumable, SCHEMA, UndoRow};
pub use reversal::{DurableReversal, LiveReversal};
pub use settings::{DEFAULT_SUPERVISOR_ROLES, Settings};

use crate::live::SiteLive;
use crate::services::ports::{ConsumerLease, OutboxLease};
use crate::services::process_governance::{GrantHost, Settings as GovernanceSettings};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the process engine (1100), the Document node module (1110), the task inbox
/// (1120) and governance (1140), and before the modules that consume the effects host.
pub const ORDER: u32 = 1170;

/// The consumer name the module's lease is held under (`outbox:<site>:process_effects`).
const LEASE_CONSUMER: &str = "process_effects";

/// The hosts of the sites built so far, by site name: a reload builds the site again without
/// building the host again.
fn built() -> &'static Mutex<HashMap<String, Arc<EffectsHost>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<EffectsHost>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------------------------
// The production seams.
// ---------------------------------------------------------------------------------------------

/// Who may supervise, from the role assignments of the live site. A user the site cannot resolve
/// (or a site with no identity yet) is `None`: denied.
pub struct LiveSupervisors {
    live: Arc<SiteLive>,
    roles: Vec<String>,
}

impl LiveSupervisors {
    /// The supervisors of `live`: a person holding one of `roles`.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, roles: Vec<String>) -> Self {
        Self { live, roles }
    }
}

impl Supervisors for LiveSupervisors {
    fn may_supervise<'a>(&'a self, actor: &'a str) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move {
            let id = actor
                .strip_prefix("user:")
                .and_then(spot_bpm_documents::ids::parse_record_id)?;
            let principals = self.live.principals()?;
            let principal = principals.resolve(&id).await;
            Some(
                principal
                    .assignments
                    .iter()
                    .flat_map(|assignment| assignment.roles.iter())
                    .any(|role| self.roles.iter().any(|name| name == role.as_str())),
            )
        })
    }
}

/// The supervisor notice when nothing better is provided: a counter and a log line with the
/// request id. Delivery to a person is the notice feed's job.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogNotice;

/// Counter: requests whose reversal was refused and so need a person's attention.
const NEEDS_ATTENTION_TOTAL: &str = "spot_bpm_effects_needs_attention_total";

impl EffectNotice for LogNotice {
    fn needs_attention<'a>(&'a self, notice: &'a AttentionNotice) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            metrics::counter!(NEEDS_ATTENTION_TOTAL).increment(1);
            tracing::warn!(
                request = %notice.request,
                entry = notice.entry,
                index = notice.index,
                "a request needs attention: a reversal was refused"
            );
        })
    }
}

/// No conversations module: every partner message is refused with a stable code, and the undo
/// stays resumable.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPartner;

impl PartnerPort for NoPartner {
    fn send_cancellation(
        &self,
        _call: &spot_bpm_effects::CancellationCall<'_>,
    ) -> std::result::Result<(), String> {
        Err("partner_unavailable".to_owned())
    }
}

/// No dispatcher is registered: a release stops at the first effect and it stays held.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDispatcher;

impl Dispatcher for NoDispatcher {
    fn dispatch<'a>(&'a self, effect: &'a EffectRecord) -> BoxFuture<'a, HoldResult<()>> {
        Box::pin(async move {
            Err(HoldError::DispatchFailed {
                key: effect.key.clone(),
                class: effect.class.key(),
                reason: "no dispatcher is registered for this site".to_owned(),
            })
        })
    }
}

/// Discard notices as a log line (class, kind and key: ids, never a payload).
#[derive(Clone, Copy, Debug, Default)]
pub struct LogNotifier;

impl Notifier for LogNotifier {
    fn notify<'a>(&'a self, notice: Notice) -> BoxFuture<'a, HoldResult<()>> {
        Box::pin(async move {
            tracing::warn!(
                class = %notice.class,
                kind = %notice.kind,
                key = %notice.key,
                "a held effect was discarded and needs a person"
            );
            Ok(())
        })
    }
}

/// Hold transitions into the module's audit table.
pub struct HoldAudit {
    ledger: EffLedger,
}

impl HoldAudit {
    /// The sink over `ledger`.
    #[must_use]
    pub fn new(ledger: EffLedger) -> Self {
        Self { ledger }
    }
}

impl AuditSink for HoldAudit {
    fn record<'a>(&'a self, event: AuditEvent) -> BoxFuture<'a, HoldResult<()>> {
        Box::pin(async move {
            let request = event
                .migration_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            let action = format!("hold_{}", event.kind.name());
            self.ledger
                .audit(
                    "system:effects",
                    &request,
                    &action,
                    "recorded",
                    "hold",
                    event.at,
                )
                .await;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The service.
// ---------------------------------------------------------------------------------------------

/// Resumes interrupted undos on the holder of the lease.
struct EffectsService {
    host: Arc<EffectsHost>,
    outbox: Arc<dyn OutboxLease>,
    interval: Duration,
}

impl Service for EffectsService {
    fn name(&self) -> &'static str {
        "process_effects"
    }

    fn level(&self) -> Level {
        // A broken effects ledger is not a reason to take document serving down.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.host.prepare().await.map_err(|error| {
                format!(
                    "the effects tables are not usable ({}); check the database",
                    error.code()
                )
            })
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut lease: Option<ConsumerLease> = None;
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        let held = match &lease {
                            Some(held) => matches!(self.outbox.still_held(held).await, Ok(true)),
                            None => false,
                        };
                        if !held {
                            lease = self.outbox.acquire(LEASE_CONSUMER).await.ok();
                        }
                        if lease.is_some() {
                            let resumed = self.host.resume_pending().await;
                            if resumed > 0 {
                                tracing::debug!(resumed, "interrupted undos resumed");
                            }
                        }
                    }
                }
            }
            if let Some(held) = lease {
                let _ = self.outbox.release(&held).await;
            }
            Ok(())
        })
    }
}

/// Says in readiness that no business graph is hosted: an undo then sees only the links inside the
/// request's own file. It checks as failed on purpose, like the task inbox's calendar notice.
struct GraphNotice;

impl Service for GraphNotice {
    fn name(&self) -> &'static str {
        "process_effects_graph"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async {
            Err(
                "no business graph is hosted: an undo sees only the links inside the request's \
                 own record file"
                    .to_owned(),
            )
        })
    }

    fn run(&self, _stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// Registers request effects: the ledger, the hold, the undo and the service. Inert when
/// `services.process_effects.enabled = false`.
///
/// # Errors
/// A setting that does not decode or is out of range; a table that cannot be prepared; a port
/// another module already provided.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_effects")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.process_effects.{key}: out of range")))?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let host = if ctx.assembling() {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error("process effects need the async runtime".into()))?;
        let pool = env.backend.pool();
        let outbox = ctx.require_port::<dyn OutboxLease>()?;
        let ledger = EffLedger::new(pool.clone(), &site);
        let hold_ledger = PgLedger::new(pool.clone());
        hold_ledger
            .prepare()
            .await
            .map_err(|e| Error(format!("cli.process_effects_hold_tables: {}", e.code)))?;
        let declarations: Arc<dyn Declarations> = match ctx.port::<spot_seams::Registry>() {
            Some(registry) => registry,
            // With no seam registry nothing is declared, so every effect is a point of no
            // return and an undo of a receipt with effects stops for a person (fail closed).
            None => Arc::new(StaticDeclarations::new()),
        };
        let graph_known = ctx.port::<dyn GraphDependants>();
        if graph_known.is_none() {
            ctx.add(Arc::new(GraphNotice));
        }
        let graph: Arc<dyn GraphDependants> = graph_known.unwrap_or_else(|| Arc::new(NoGraph));
        let partner: Arc<dyn PartnerPort> = ctx
            .port::<dyn PartnerPort>()
            .unwrap_or_else(|| Arc::new(NoPartner));
        let dispatcher: Arc<dyn Dispatcher> = ctx
            .port::<dyn Dispatcher>()
            .unwrap_or_else(|| Arc::new(NoDispatcher));
        let plans: Arc<dyn PlanSource> = ctx
            .port::<dyn PlanSource>()
            .unwrap_or_else(|| Arc::new(NoPlans));
        let live = Arc::clone(env.live);
        let site_id = spot_app_api::prelude::SiteId::new(&site).map_err(crate::error)?;
        let live_reversal: Arc<dyn ReversalPort> = Arc::new(LiveReversal::new(
            Arc::clone(&live),
            site_id.clone(),
            runtime.clone(),
        ));
        // With the governance module on, an undo of a governed record runs under a request grant
        // (WP-BPM-15.T5); with it off, or for any other record, nothing changes.
        let reversal: Arc<dyn ReversalPort> = match ctx.port::<GrantHost>() {
            Some(grants) => {
                let governance: GovernanceSettings =
                    ctx.settings_or_default("process_governance")?;
                let entries: Vec<(String, String)> = governance
                    .governed
                    .iter()
                    .map(|entry| (entry.doctype.clone(), entry.mode.clone()))
                    .collect();
                Arc::new(GrantedReversal::new(
                    live_reversal,
                    grants,
                    governed_doctypes(&entries),
                    EffLedger::new(pool.clone(), &site),
                    Arc::new(LiveProbe::new(Arc::clone(&live), site_id, runtime.clone())),
                    runtime.clone(),
                ))
            }
            None => live_reversal,
        };
        let interval = Duration::from_secs(settings.tick_secs);
        let roles = settings.supervisor_roles.clone();
        let wiring = Wiring {
            site: site.clone(),
            clock: Arc::clone(ctx.clock()),
            pool,
            settings,
            runtime,
            declarations,
            graph,
            reversal,
            partner,
            supervisors: Arc::new(LiveSupervisors::new(live, roles)),
            notice: Arc::new(LogNotice),
            hold: HoldParts {
                ledger: Arc::new(hold_ledger) as Arc<dyn LedgerStore>,
                dispatcher,
                notifier: Arc::new(LogNotifier),
                audit: Arc::new(HoldAudit::new(ledger)),
                plans,
            },
        };
        let host = Arc::new(
            wiring
                .build()
                .map_err(|key| Error(format!("services.process_effects.{key}: not valid")))?,
        );
        host.ledger()
            .prepare()
            .await
            .map_err(|error| Error(format!("cli.process_effects_tables: {error}")))?;
        ctx.add(Arc::new(EffectsService {
            host: Arc::clone(&host),
            outbox,
            interval,
        }));
        lock(built()).insert(site, Arc::clone(&host));
        host
    } else {
        match lock(built()).get(&site).cloned() {
            Some(host) => host,
            None => return Ok(()),
        }
    };
    ctx.provide::<EffectsHost>(host)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_module_starts_after_governance() {
        const { assert!(super::ORDER > 1140) };
    }

    #[test]
    fn the_default_supervisors_are_named() {
        assert!(super::DEFAULT_SUPERVISOR_ROLES.contains(&"Process Supervisor"));
    }
}
