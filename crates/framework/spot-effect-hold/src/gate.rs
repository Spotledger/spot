//! The gate every external dispatcher passes through before the irrevocable act.
//!
//! [`Gate::admit`] is cheap: it reads an in-memory snapshot of the hold state. The snapshot errs
//! towards **Held**: an unknown or unreadable state, and a snapshot that may be stale relative to
//! a `begin` in flight, both yield [`Admission::Held`], never [`Admission::Proceed`].

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use spot_core::SiteId;

use crate::class::{EffectClass, PolicyTable};
use crate::effect::{EffectId, EffectRef, NewEffect};
use crate::hold::{HoldState, MigrationId};
use crate::store::LedgerStore;

/// How long a snapshot stays fresh when a gate does not say otherwise: five seconds.
pub const DEFAULT_MAX_STALENESS_MICROS: i64 = 5_000_000;

/// What the gate saw when it decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HoldSnapshot {
    /// The site is open; held-class effects proceed.
    Open,
    /// The site is held under a migration.
    Held {
        /// The migration being verified.
        migration_id: MigrationId,
    },
    /// The state could not be read; the site is treated as held (fail closed).
    Unknown,
}

/// A held effect's ticket: what the dispatcher reports as *deferred*, not failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HoldTicket {
    /// The site.
    pub site: SiteId,
    /// The ledger id, when the effect was recorded durably.
    pub effect_id: Option<EffectId>,
    /// The migration the effect is held under; `unverified` when the state was unknown.
    pub migration_id: MigrationId,
    /// Whether the effect is in the durable ledger. `false` is the fail-closed placeholder for an
    /// unavailable ledger: the effect was *not* sent, and must be re-driven once the ledger
    /// answers again.
    pub durable: bool,
}

/// What [`Gate::admit`] decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The effect may go out now: the site is open, or its class is never held.
    Proceed,
    /// The effect was recorded and deferred; do not count a retry or dead-letter it.
    Held(HoldTicket),
}

/// The shared in-memory snapshot of one site's hold state.
pub(crate) struct Snapshot {
    held: AtomicBool,
    known: AtomicBool,
    generation: AtomicU64,
    migration: RwLock<Option<MigrationId>>,
    inflight: AtomicUsize,
    loaded_at: AtomicI64,
}

impl Snapshot {
    fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
            known: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            migration: RwLock::new(None),
            inflight: AtomicUsize::new(0),
            loaded_at: AtomicI64::new(i64::MIN),
        }
    }

    fn state(&self) -> HoldSnapshot {
        if !self.known.load(Ordering::SeqCst) {
            return HoldSnapshot::Unknown;
        }
        if self.held.load(Ordering::SeqCst) {
            let migration = self
                .migration
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .unwrap_or_else(MigrationId::unverified);
            return HoldSnapshot::Held {
                migration_id: migration,
            };
        }
        HoldSnapshot::Open
    }

    fn is_stale(&self, now: i64, max_staleness: i64) -> bool {
        let loaded_at = self.loaded_at.load(Ordering::SeqCst);
        loaded_at != i64::MIN && now.saturating_sub(loaded_at) > max_staleness
    }

    fn mark_held(&self, migration_id: MigrationId, now: i64) {
        *self
            .migration
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(migration_id);
        self.loaded_at.store(now, Ordering::SeqCst);
        self.known.store(true, Ordering::SeqCst);
        self.held.store(true, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn mark_open(&self, now: i64) {
        *self
            .migration
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.loaded_at.store(now, Ordering::SeqCst);
        self.known.store(true, Ordering::SeqCst);
        self.held.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn mark_unknown(&self) {
        self.known.store(false, Ordering::SeqCst);
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

/// The gate: one per site, shared by every dispatcher and by the hold service.
pub struct Gate {
    site: SiteId,
    ledger: Arc<dyn LedgerStore>,
    policies: Arc<PolicyTable>,
    snapshot: Arc<Snapshot>,
    max_staleness: i64,
}

impl Gate {
    /// A gate for `site` over `ledger` and `policies`. It starts **unknown** (fail closed); call
    /// [`Gate::load`] to read the durable state.
    #[must_use]
    pub fn new(site: SiteId, ledger: Arc<dyn LedgerStore>, policies: Arc<PolicyTable>) -> Self {
        Self {
            site,
            ledger,
            policies,
            snapshot: Arc::new(Snapshot::new()),
            max_staleness: DEFAULT_MAX_STALENESS_MICROS,
        }
    }

    /// The same gate with a snapshot lifetime of `max_staleness` microseconds. A snapshot older
    /// than this is treated as held, so a node that has not refreshed since another node began a
    /// hold fails closed.
    #[must_use]
    pub fn with_max_staleness(mut self, max_staleness: i64) -> Self {
        self.max_staleness = max_staleness;
        self
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The policy table.
    #[must_use]
    pub fn policies(&self) -> &PolicyTable {
        &self.policies
    }

    /// Reads the durable hold state into the snapshot, stamping it with `now`.
    ///
    /// An unreadable ledger leaves the snapshot **unknown**, which the gate treats as held.
    pub async fn load(&self, now: i64) {
        match self.ledger.get_hold(&self.site).await {
            Ok(state) => match state {
                HoldState::Held { migration_id, .. } => self.snapshot.mark_held(migration_id, now),
                _ => self.snapshot.mark_open(now),
            },
            Err(_) => self.snapshot.mark_unknown(),
        }
    }

    /// The current snapshot.
    #[must_use]
    pub fn snapshot(&self) -> HoldSnapshot {
        self.snapshot.state()
    }

    /// Marks the site held and waits until every in-flight `admit` that saw an open snapshot has
    /// finished, so that after this returns no held-class effect can be admitted to proceed.
    ///
    /// The wait is not a lock a dispatcher can block on: the in-flight window is a few atomic
    /// operations long. It yields to the runtime, so a single-threaded executor still makes
    /// progress.
    pub(crate) async fn begin_hold(&self, migration_id: MigrationId, now: i64) {
        self.snapshot.mark_held(migration_id, now);
        // Bounded: an admit's in-flight window contains no await, so this always drains.
        let mut spins = 0u32;
        while self.snapshot.inflight.load(Ordering::SeqCst) != 0 && spins < 10_000 {
            tokio::time::sleep(Duration::ZERO).await;
            spins += 1;
        }
        // A few settling yields so an admit that had just left its critical section is scheduled
        // through its post-admit work before this returns. New admits already see `Held`.
        for _ in 0..4 {
            tokio::time::sleep(Duration::ZERO).await;
        }
    }

    /// Marks the site open.
    pub(crate) fn open(&self, now: i64) {
        self.snapshot.mark_open(now);
    }

    /// The gate every dispatcher calls immediately before the irrevocable act.
    ///
    /// Returns [`Admission::Proceed`] when the site is open and the snapshot is fresh or the class
    /// is never held; otherwise records the effect in the ledger and returns [`Admission::Held`].
    /// Never returns `Proceed` while the state is held, unknown, unreadable or stale.
    pub async fn admit(&self, effect: &EffectRef, now: i64) -> Admission {
        let policy = self.policies.policy_for(&effect.class);
        if policy.never_hold {
            return Admission::Proceed;
        }
        if !self.snapshot.known.load(Ordering::SeqCst)
            || self.snapshot.held.load(Ordering::SeqCst)
            || self.snapshot.is_stale(now, self.max_staleness)
        {
            return self.hold(effect).await;
        }
        // A seqlock-style open read: register in flight, then confirm no `begin` intervened. A
        // stale observation falls through to `Held`, never to `Proceed`.
        let generation = self.snapshot.generation.load(Ordering::SeqCst);
        self.snapshot.inflight.fetch_add(1, Ordering::SeqCst);
        let still_open = self.snapshot.known.load(Ordering::SeqCst)
            && !self.snapshot.held.load(Ordering::SeqCst)
            && self.snapshot.generation.load(Ordering::SeqCst) == generation
            && !self.snapshot.is_stale(now, self.max_staleness);
        self.snapshot.inflight.fetch_sub(1, Ordering::SeqCst);
        if still_open {
            Admission::Proceed
        } else {
            self.hold(effect).await
        }
    }

    async fn hold(&self, effect: &EffectRef) -> Admission {
        let migration_id = match self.snapshot.state() {
            HoldSnapshot::Held { migration_id } => migration_id,
            // Open or unknown: the effect arrived as the site was opening or while the state was
            // unreadable. Record it under the fail-closed placeholder.
            _ => MigrationId::unverified(),
        };
        let new = NewEffect {
            effect: effect.clone(),
            migration_id: migration_id.clone(),
        };
        match self.ledger.append_effect(new).await {
            Ok(id) => Admission::Held(HoldTicket {
                site: self.site.clone(),
                effect_id: Some(id),
                migration_id,
                durable: true,
            }),
            Err(_) => Admission::Held(HoldTicket {
                site: self.site.clone(),
                effect_id: None,
                migration_id,
                durable: false,
            }),
        }
    }
}

/// The job worker's view of the gate: which queues are claimable.
#[derive(Clone)]
pub struct QueueGate {
    gate: Arc<Gate>,
}

impl QueueGate {
    /// A queue gate over `gate`.
    #[must_use]
    pub fn new(gate: Arc<Gate>) -> Self {
        Self { gate }
    }

    /// Whether the worker may claim from `queue_name`. `false` while held, except for queues whose
    /// class is never held. Held jobs keep their position; release restores their order.
    #[must_use]
    pub fn is_claimable(&self, queue_name: &str) -> bool {
        let class = EffectClass::JobQueue(queue_name.to_owned());
        if self.gate.policies.policy_for(&class).never_hold {
            return true;
        }
        matches!(self.gate.snapshot(), HoldSnapshot::Open)
    }
}

/// The edge-sync view of the gate (EDG-02 will consult it).
#[derive(Clone)]
pub struct EdgeSyncGate {
    gate: Arc<Gate>,
}

impl EdgeSyncGate {
    /// An edge-sync gate over `gate`.
    #[must_use]
    pub fn new(gate: Arc<Gate>) -> Self {
        Self { gate }
    }

    /// Whether an edge may **send** its held-class changes. Local reads and writes continue.
    #[must_use]
    pub fn may_send(&self, _node: &str) -> bool {
        matches!(self.gate.snapshot(), HoldSnapshot::Open)
    }

    /// Whether an edge may **apply** inbound changes. `false` while held.
    #[must_use]
    pub fn may_apply(&self, _node: &str) -> bool {
        matches!(self.gate.snapshot(), HoldSnapshot::Open)
    }
}
