//! Hosting the side-effect hold (WP-MIG-08.T4, ADR-22).
//!
//! `spot-effect-hold` is the gate every external effect asks before it leaves the process, and
//! `spot-effect-hold-pg` is its durable ledger. This module builds both for the site, **provides**
//! them to the modules that start after it, and runs the one task that keeps the gate's view of
//! the ledger fresh.
//!
//! # Ports provided
//!
//! * [`EffectGate`]: `try_pass(class, key)` answers [`Pass::Pass`] or [`Pass::Held`]. With the
//!   hold inactive it is a read of a cached flag (the gate refreshes every
//!   [`DEFAULT_REFRESH_MILLIS`] ms). With the hold active, or the state unknown, the effect is
//!   recorded in the ledger (deduplicated on `(site, key)`) and not performed. **Fail closed:**
//!   when the ledger cannot be read the gate holds every non-fiscal effect; a fiscal
//!   transmission of an issued document is never held (ADR-43), which the hold crate enforces.
//! * [`PerformerRegistry`]: a producer registers an [`EffectPerformer`] for its effect class
//!   (`mail`, `webhook`, `job:<queue>`, ...); a release replays held effects through it, in the
//!   original order, each with its original idempotency key, at most
//!   `replay_per_class_per_second` per class. A class with no performer fails the release (the
//!   effect stays held), never a silent drop.
//! * The `HoldService` (begin, release, discard), for the lifecycle operations.
//!
//! # Settings (`[services.effect_hold]`)
//!
//! | key | default | meaning |
//! | --- | --- | --- |
//! | `enabled` | `true` | `false` provides no port; consumers behave as before |
//! | `refresh_millis` | 1000 | how often the gate re-reads the ledger (100 to 60000) |
//! | `replay_per_class_per_second` | 20 | the release's per-class rate limit (1 to 10000) |
//!
//! # Readiness
//!
//! The service is [`Level::Optional`]: a ledger that cannot be prepared or read degrades the site
//! (`effect_hold (...)`), the gate stays closed to external effects and the site keeps serving.
//!
//! # Audit
//!
//! Hold transitions go to the `spot::audit::hold` tracing target (counts and ids only, never a
//! payload). Appending them to the site's hash-chained audit store is an open question of the
//! task; until then this is the record, and a failure to log cannot fail a transition.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use spot_core::{Clock, SiteId};
use spot_effect_hold::{
    Admission, AuditEvent, AuditSink, Dispatcher, EffectClass, EffectRecord, EffectRef, Gate,
    HoldService, HoldState, LedgerStore, Notice, Notifier, PolicyTable,
};
use spot_effect_hold_pg::PgLedger;
use spot_migration_apply::{HoldServiceBridge, InMemoryAdmissionGate, NoFaults};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::hold::{HoldInspector, HoldView};
use spot_migration_ops::{
    Actor, Channel, LifecycleContext, LifecycleError, LifecyclePorts, PlanHash, PlanState,
    PlanStore, PlanSummary, SharedAuditSink, StoredPlan,
};
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Starts below the job worker (600), mail (700) and realtime (800), after participants (500); a legacy-ordered module (`LEGACY_ORDER`).
pub const ORDER: u32 = 550;

/// The module name and the `[services.<name>]` table it reads.
pub const NAME: &str = "effect_hold";

/// How often the gate re-reads the ledger by default.
pub const DEFAULT_REFRESH_MILLIS: u64 = 1_000;

/// The replay rate limit per class by default.
pub const DEFAULT_REPLAY_PER_CLASS_PER_SECOND: u32 = 20;

/// `[services.effect_hold]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// `false` provides nothing and consumers behave as before.
    pub enabled: bool,
    /// Gate refresh period, in milliseconds (100 to 60000).
    pub refresh_millis: u64,
    /// Release rate limit per effect class (1 to 10000).
    pub replay_per_class_per_second: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_millis: DEFAULT_REFRESH_MILLIS,
            replay_per_class_per_second: DEFAULT_REPLAY_PER_CLASS_PER_SECOND,
        }
    }
}

impl Settings {
    /// Refuses a value outside its range.
    ///
    /// # Errors
    /// [`Error`] naming the key, never a value.
    pub fn validate(&self) -> Result<()> {
        if !(100..=60_000).contains(&self.refresh_millis) {
            return Err(Error(
                "services.effect_hold.refresh_millis must be 100 to 60000".to_owned(),
            ));
        }
        if !(1..=10_000).contains(&self.replay_per_class_per_second) {
            return Err(Error(
                "services.effect_hold.replay_per_class_per_second must be 1 to 10000".to_owned(),
            ));
        }
        Ok(())
    }
}

/// What the gate decided for one effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    /// Perform the effect now.
    Pass,
    /// The effect was recorded and deferred. Do not perform it, do not count a retry, do not
    /// dead-letter it: it is parked by the ledger.
    Held,
}

/// A boxed future, the shape the hold crate's object-safe traits use.
pub type GateFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The port every external-effect producer asks immediately before the irrevocable act.
pub trait EffectGate: Send + Sync {
    /// Whether the effect of `class` with idempotency key `key` may be performed now. An error
    /// asking the gate is `Held` for every external class: this method has no error case.
    fn try_pass<'a>(&'a self, class: &'a EffectClass, key: &'a str) -> GateFuture<'a, Pass>;
}

/// One class' replay implementation: perform a held effect with its original key.
pub trait EffectPerformer: Send + Sync {
    /// Performs `effect`. The transport must treat `effect.key` as an idempotency key.
    ///
    /// # Errors
    /// A short, payload-free reason; the effect then stays held.
    fn perform<'a>(
        &'a self,
        effect: &'a EffectRecord,
    ) -> GateFuture<'a, std::result::Result<(), String>>;
}

/// The performers by effect class key, filled by the producers at registration.
#[derive(Default)]
pub struct PerformerRegistry {
    by_class: Mutex<BTreeMap<String, Arc<dyn EffectPerformer>>>,
}

impl PerformerRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Registers (or replaces) the performer of `class_key` (`mail`, `webhook`, `job`, ...).
    /// A class key `job:<queue>` falls back to the performer of `job`.
    pub fn register(&self, class_key: &str, performer: Arc<dyn EffectPerformer>) {
        if let Ok(mut map) = self.by_class.lock() {
            map.insert(class_key.to_owned(), performer);
        }
    }

    fn find(&self, class_key: &str) -> Option<Arc<dyn EffectPerformer>> {
        let map = self.by_class.lock().ok()?;
        map.get(class_key)
            .or_else(|| class_key.split(':').next().and_then(|head| map.get(head)))
            .cloned()
    }
}

/// The unix microseconds of `clock`'s now.
#[must_use]
pub fn micros(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
}

/// The gate port over the hold crate's [`Gate`].
pub struct HoldEffectGate {
    gate: Arc<Gate>,
    site: SiteId,
    clock: Arc<dyn Clock>,
}

impl EffectGate for HoldEffectGate {
    fn try_pass<'a>(&'a self, class: &'a EffectClass, key: &'a str) -> GateFuture<'a, Pass> {
        Box::pin(async move {
            let now = micros(self.clock.as_ref());
            let effect = EffectRef::new(self.site.clone(), class.clone(), class.key(), key, now);
            match self.gate.admit(&effect, now).await {
                Admission::Proceed => Pass::Pass,
                Admission::Held(_) => Pass::Held,
            }
        })
    }
}

/// Dispatches a released effect through the registered performer, rate limited per class.
struct RegistryDispatcher {
    performers: Arc<PerformerRegistry>,
    interval: Duration,
    next_slot: Mutex<BTreeMap<String, Instant>>,
}

impl RegistryDispatcher {
    fn wait_for(&self, class: &str) -> Duration {
        let now = Instant::now();
        let Ok(mut slots) = self.next_slot.lock() else {
            return self.interval;
        };
        let slot = slots.get(class).copied().unwrap_or(now).max(now);
        slots.insert(class.to_owned(), slot + self.interval);
        slot.saturating_duration_since(now)
    }
}

impl Dispatcher for RegistryDispatcher {
    fn dispatch<'a>(
        &'a self,
        effect: &'a EffectRecord,
    ) -> GateFuture<'a, spot_effect_hold::Result<()>> {
        Box::pin(async move {
            let class = effect.class.key();
            let failed = |reason: &str| spot_effect_hold::Error::DispatchFailed {
                key: effect.key.clone(),
                class: class.clone(),
                reason: reason.to_owned(),
            };
            let Some(performer) = self.performers.find(&class) else {
                return Err(failed("no performer is registered for this class"));
            };
            let wait = self.wait_for(&class);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            performer.perform(effect).await.map_err(|why| failed(&why))
        })
    }
}

/// Records hold transitions as tracing events (target `spot::audit::hold`): kind, site,
/// migration id and counts, never a payload.
pub struct TracingHoldAudit;

impl AuditSink for TracingHoldAudit {
    fn record<'a>(&'a self, event: AuditEvent) -> GateFuture<'a, spot_effect_hold::Result<()>> {
        Box::pin(async move {
            tracing::info!(
                target: "spot::audit::hold",
                kind = event.kind.name(),
                site = %event.site,
                migration = event.migration_id.as_ref().map_or("", |m| m.as_str()),
                at = event.at,
                counts = ?event.counts,
                "side-effect hold transition"
            );
            Ok(())
        })
    }
}

/// Raises the discard notice as a tracing event; a task for a person is the job module's to
/// create once it provides a port for it.
struct TracingNotifier;

impl Notifier for TracingNotifier {
    fn notify<'a>(&'a self, notice: Notice) -> GateFuture<'a, spot_effect_hold::Result<()>> {
        Box::pin(async move {
            tracing::warn!(
                target: "spot::audit::hold",
                site = %notice.site,
                class = %notice.class,
                kind = %notice.kind,
                "a discarded effect needs a person"
            );
            Ok(())
        })
    }
}

/// Reads the hold state from the ledger for `spot migrate hold --status`.
pub struct LedgerInspector {
    ledger: Arc<dyn LedgerStore>,
    site: SiteId,
}

impl HoldInspector for LedgerInspector {
    fn state(&self) -> GateFuture<'_, std::result::Result<HoldView, LifecycleError>> {
        Box::pin(async move {
            match self.ledger.get_hold(&self.site).await {
                Ok(HoldState::Held {
                    migration_id,
                    reason,
                    since,
                    ..
                }) => Ok(HoldView {
                    held: true,
                    migration_id: Some(migration_id.to_string()),
                    reason: Some(reason),
                    since: Some(since),
                }),
                Ok(_) => Ok(HoldView {
                    held: false,
                    migration_id: None,
                    reason: None,
                    since: None,
                }),
                Err(_) => Err(LifecycleError::Store),
            }
        })
    }
}

/// Everything built for one site over one ledger.
pub struct HoldParts {
    /// The site.
    pub site: SiteId,
    /// The ledger.
    pub ledger: Arc<dyn LedgerStore>,
    /// Begin, release and discard.
    pub service: Arc<HoldService>,
    /// The hold crate's gate (the job worker's `QueueGate` is built from it).
    pub gate: Arc<Gate>,
    /// The port the producers use.
    pub effect_gate: Arc<dyn EffectGate>,
    /// The performers a release replays through.
    pub performers: Arc<PerformerRegistry>,
    /// The state reader.
    pub inspector: Arc<dyn HoldInspector>,
}

/// Builds the parts over `ledger`. Nothing is read or created.
#[must_use]
pub fn build(
    site: SiteId,
    ledger: Arc<dyn LedgerStore>,
    clock: Arc<dyn Clock>,
    settings: &Settings,
    audit: Arc<dyn AuditSink>,
) -> HoldParts {
    let performers = PerformerRegistry::new();
    let per_second = u64::from(settings.replay_per_class_per_second.max(1));
    let dispatcher = Arc::new(RegistryDispatcher {
        performers: Arc::clone(&performers),
        interval: Duration::from_micros(1_000_000 / per_second),
        next_slot: Mutex::new(BTreeMap::new()),
    });
    let service = Arc::new(HoldService::new(
        site.clone(),
        Arc::clone(&ledger),
        Arc::new(PolicyTable::new()),
        audit,
        dispatcher,
        Arc::new(TracingNotifier),
    ));
    let gate = service.gate();
    let effect_gate: Arc<dyn EffectGate> = Arc::new(HoldEffectGate {
        gate: Arc::clone(&gate),
        site: site.clone(),
        clock,
    });
    let inspector: Arc<dyn HoldInspector> = Arc::new(LedgerInspector {
        ledger: Arc::clone(&ledger),
        site: site.clone(),
    });
    HoldParts {
        site,
        ledger,
        service,
        gate,
        effect_gate,
        performers,
        inspector,
    }
}

/// The ledger of a site's database. It does not touch the database until used.
#[must_use]
pub fn pg_ledger(backend: &PgBackend) -> Arc<PgLedger> {
    Arc::new(PgLedger::new(backend.pool()))
}

/// The site clock as the lifecycle operations' time source.
struct OpsClock(Arc<dyn Clock>);

impl spot_migration_ops::Clock for OpsClock {
    fn now(&self) -> i64 {
        micros(self.0.as_ref())
    }
}

/// No plan is stored by the hold operations.
struct UnwiredPlans;

impl PlanStore for UnwiredPlans {
    fn save(&self, _plan: StoredPlan) -> std::result::Result<(), LifecycleError> {
        Err(LifecycleError::PortUnavailable { port: "plans" })
    }
    fn load(&self, _hash: &PlanHash) -> std::result::Result<Option<StoredPlan>, LifecycleError> {
        Err(LifecycleError::PortUnavailable { port: "plans" })
    }
    fn list(&self) -> std::result::Result<Vec<PlanSummary>, LifecycleError> {
        Err(LifecycleError::PortUnavailable { port: "plans" })
    }
    fn mark_state(
        &self,
        _hash: &PlanHash,
        _expected: PlanState,
        _expected_version: u64,
        _next: PlanState,
    ) -> std::result::Result<u64, LifecycleError> {
        Err(LifecycleError::PortUnavailable { port: "plans" })
    }
}

/// A lifecycle context that can run the hold operations for `parts`' site as `actor`. The ports
/// the hold does not use are inert: no plan store, no run store, no snapshot provider, and an
/// in-memory approval service nobody reads, so any other verb refuses with
/// `migration.port_unavailable` instead of touching state.
///
/// # Errors
/// [`Error`] when the actor is anonymous or the approval service cannot be built.
pub fn lifecycle_context(
    parts: &HoldParts,
    clock: Arc<dyn Clock>,
    actor: Principal,
    channel: Channel,
) -> Result<LifecycleContext> {
    let actor = Actor::new(actor, channel).map_err(|e| Error(e.to_string()))?;
    let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
    let approvals = ApprovalService::new(
        store,
        SharedAuditSink(Arc::new(MemoryAuditSink::new())),
        ApprovalPolicy::default(),
    )
    .map_err(|e| Error(e.to_string()))?;
    let ports = LifecyclePorts {
        clock: Arc::new(OpsClock(clock)),
        approvals: Arc::new(approvals),
        runs: None,
        snapshots: None,
        gate: Arc::new(InMemoryAdmissionGate::new()),
        hold: Arc::new(HoldServiceBridge::new(Arc::clone(&parts.service))),
        faults: Arc::new(NoFaults),
        plans: Arc::new(UnwiredPlans),
        access: Arc::new(RoleAccess::new()),
    };
    Ok(LifecycleContext::new(parts.site.clone(), actor, ports))
}

/// The service that keeps the gate fresh and reports the ledger's health.
pub struct HoldKeeper {
    parts_gate: Arc<Gate>,
    ledger: Arc<dyn LedgerStore>,
    site: SiteId,
    clock: Arc<dyn Clock>,
    refresh: Duration,
    prepare_error: Option<String>,
}

impl Service for HoldKeeper {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if let Some(reason) = &self.prepare_error {
                return Err(format!(
                    "the hold ledger could not be prepared ({reason}); external effects are held"
                ));
            }
            // The flag must be readable: when it is not, the gate stays closed to external
            // effects and the site says so.
            self.parts_gate.load(micros(self.clock.as_ref())).await;
            self.ledger
                .get_hold(&self.site)
                .await
                .map(|_| ())
                .map_err(|_| "the hold state cannot be read; external effects are held".to_owned())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut tick = tokio::time::interval(self.refresh);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    _ = tick.tick() => {
                        self.parts_gate.load(micros(self.clock.as_ref())).await;
                    }
                }
            }
        })
    }
}

/// Registers the hold for the site: builds it, provides the ports, adds the keeper.
///
/// # Errors
/// [`Error`] for a setting out of range or a site name that is not a valid site id.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    let env = ctx.env()?;
    let site = SiteId::new(&env.site.site)
        .map_err(|_| Error("effect_hold: the site name is not a valid site id".to_owned()))?;
    let ledger = pg_ledger(env.backend);
    let prepare_error = match ledger.prepare().await {
        Ok(()) => None,
        Err(_) => Some("the ledger tables could not be created".to_owned()),
    };
    let clock = Arc::clone(ctx.clock());
    let parts = build(
        site.clone(),
        Arc::clone(&ledger) as Arc<dyn LedgerStore>,
        Arc::clone(&clock),
        &settings,
        Arc::new(TracingHoldAudit),
    );
    ctx.provide::<dyn EffectGate>(Arc::clone(&parts.effect_gate))?;
    ctx.provide::<PerformerRegistry>(Arc::clone(&parts.performers))?;
    ctx.provide::<HoldService>(Arc::clone(&parts.service))?;
    ctx.add(Arc::new(HoldKeeper {
        parts_gate: Arc::clone(&parts.gate),
        ledger: parts.ledger,
        site,
        clock,
        refresh: Duration::from_millis(settings.refresh_millis),
        prepare_error,
    }));
    Ok(())
}
