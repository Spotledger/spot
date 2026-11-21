//! The persistence contract: idempotent, period-bucketed events behind an object-safe
//! async trait.
//!
//! [`MeterStore`] is deliberately small and object-safe (`Box<dyn MeterStore>` works),
//! so a later task can write a PostgreSQL store and every caller here keeps working.
//! The in-memory [`MemoryMeterStore`] is the reference implementation; the
//! backend-independent contract suite is [`crate::contract`].
//!
//! Bucketing is by [`PeriodKey`] computed from [`PeriodResolver`], so the store never
//! guesses a tenant's zone and the same contract holds for any backend.
//!
//! # Two boundaries this trait draws
//!
//! **A store belongs to one tenant.** It is bound at construction and its
//! [`MeterStore::tenant`] is what it writes: an event carrying another tenant is
//! refused with [`MeteringError::TenantMismatch`] before anything is resolved or
//! written, and a read for another tenant is refused the same way. A producer can no
//! longer name a victim tenant in an event and land in that tenant's totals;
//! the tenant is a property of the store the caller was handed, not of the payload.
//!
//! **A store bounds one call.** [`MAX_BATCH_EVENTS`] is the most events one
//! `add_events` call accepts, so no single call can hold a transaction open for longer
//! than that. A larger batch is refused whole with [`MeteringError::BatchTooLarge`]
//! before any write; the caller sends it in several calls.
//!
//! # Replay after a purge
//!
//! Compaction keeps the deduplication tombstone — the recorded id — until the period
//! is both final (the caller's `before` cut says so) and older than the
//! [`RetentionPolicy`]'s horizon. Past that point the tombstone is removed and the
//! period becomes the store's **late-arrival floor**: every event whose period ends at
//! or before the floor's start is refused with
//! [`MeteringError::LateBeyondHorizon`], so a purged id can never be counted twice.
//! [`crate::retention`] states the invariant and how to size the horizon; the
//! [`StoreContractCase`](crate::contract::StoreContractCase) cases prove it on every
//! implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::error::MeteringError;
use crate::event::MeterEvent;
use crate::meter::{MemberDigest, Meter, MeterKind};
use crate::period::{PeriodKey, PeriodResolver};
use crate::retention::RetentionPolicy;
use crate::totals::{PeriodTotals, TenantPeriodUsage};

/// The most events one `add_events` call accepts.
///
/// The bound exists so a single call cannot hold one transaction (or one mutex) for an
/// unbounded time: a caller with a bigger backlog sends it as several calls and gets
/// the same totals, because recording is idempotent by id. A batch larger than this is
/// refused with [`MeteringError::BatchTooLarge`] before anything is written.
pub const MAX_BATCH_EVENTS: usize = 10_000;

/// A boxed future, the object-safe async return shape.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What one compaction removed, and where the late-arrival floor now is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompactionReport {
    /// Events whose raw metadata was dropped (totals, members and tombstones kept).
    pub events_dropped: u64,
    /// Tombstones removed: ids of periods past the retention horizon.
    pub tombstones_dropped: u64,
    /// The first period still accepting events, when a floor has been set. Events in
    /// earlier periods are refused with
    /// [`MeteringError::LateBeyondHorizon`].
    pub tombstone_floor: Option<PeriodKey>,
}

impl CompactionReport {
    /// Whether this run changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events_dropped == 0 && self.tombstones_dropped == 0
    }
}

/// The object-safe persistence contract every meter store implements.
///
/// Writes are idempotent on `(tenant, event id)`: `add_events` keeps the first event
/// for an id and ignores later ones, so a retried batch never double-counts — including
/// after the tombstone is purged, because the purge also refuses the period.
pub trait MeterStore: Send + Sync {
    /// The one tenant this store is bound to.
    ///
    /// Every write lands in this tenant and nowhere else; a read naming another tenant
    /// is refused. See the module docs.
    fn tenant(&self) -> &str;

    /// Adds a batch of events, idempotently by id.
    ///
    /// Returns how many events were **newly** recorded; a re-added event is counted in
    /// neither the batch's new count nor the totals. The events may arrive in any
    /// order and may overlap with earlier batches.
    ///
    /// The whole batch is refused, before any write, when it is larger than
    /// [`MAX_BATCH_EVENTS`], when any event carries a tenant other than
    /// [`MeterStore::tenant`], when any event is malformed, or when any event belongs
    /// to a period past the late-arrival floor.
    fn add_events<'a>(
        &'a self,
        events: Vec<MeterEvent>,
        resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<usize, MeteringError>>;

    /// The combined totals of one tenant-period, per meter.
    fn period_totals<'a>(
        &'a self,
        tenant: &'a str,
        period: &'a PeriodKey,
        resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<TenantPeriodUsage, MeteringError>>;

    /// The distinct member digests of one distinct-set meter in one period, sorted.
    ///
    /// Non-distinct-set meters return an empty list. This is what reconciliation uses
    /// to name *which* members differ.
    fn distinct_members<'a>(
        &'a self,
        tenant: &'a str,
        period: &'a PeriodKey,
        meter: &'a Meter,
        resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<Vec<MemberDigest>, MeteringError>>;

    /// How many deduplication tombstones the store holds for its tenant.
    ///
    /// A tombstone is one remembered event id, whether or not its raw metadata has been
    /// compacted away. This is the number [`RetentionPolicy`] bounds and the number
    /// that must drop when compaction runs past the horizon.
    fn tombstone_count<'a>(&'a self) -> BoxFuture<'a, Result<u64, MeteringError>>;

    /// Drops raw event metadata of periods that ended at or before `before`, and
    /// removes the tombstones of periods past the retention horizon.
    ///
    /// `before` names the first period whose totals may still change; every earlier
    /// period is final (closed and reconciled), which is the caller's precondition for
    /// compaction. The tombstones removed are those of periods that are additionally
    /// older than `policy`'s horizon, and from then on every event in those periods is
    /// refused as [`MeteringError::LateBeyondHorizon`] — the floor advances, it never
    /// recedes. Periods at or after `before` are untouched.
    ///
    /// A `policy` whose horizon is below [`RetentionPolicy::MIN_HORIZON_PERIODS`] is
    /// refused with [`MeteringError::RetentionHorizonTooShort`] and purges nothing: a
    /// horizon inside a producer's retry window loses usage rather than storage.
    fn compact<'a>(
        &'a self,
        before: &'a PeriodKey,
        policy: &'a RetentionPolicy,
        resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<CompactionReport, MeteringError>>;
}

/// One recorded fact: the period it was bucketed into, and whether the raw metadata
/// (meter, amount, member, instant) is still held.
#[derive(Clone, Debug)]
struct StoredEvent {
    period: PeriodKey,
    raw: bool,
}

/// The in-memory store: events keyed by id for one tenant, first-write-wins, plus
/// per-period indices.
///
/// The layout is the one a real store would use: a keyed id map for idempotency and
/// retention, a per-(period, meter) index for totals, and a per-(period, meter) member
/// set for distinct counting. The tenant is not part of any key because an instance
/// serves exactly one tenant — the same rule the PostgreSQL store enforces with a
/// tenant column in every key and filter. Everything is behind one mutex, which is
/// enough for a library whose callers flush in batches.
#[derive(Debug)]
pub struct MemoryMeterStore {
    tenant: String,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// event id -> the stored event (first write wins); the key is the tombstone.
    events: BTreeMap<String, StoredEvent>,
    /// (period, meter) -> running totals.
    totals: BTreeMap<(PeriodKey, Meter), PeriodTotals>,
    /// (period, meter) -> distinct member digests.
    members: BTreeMap<(PeriodKey, Meter), BTreeSet<MemberDigest>>,
    /// The first period still accepting events, once a compaction has purged one.
    floor: Option<PeriodKey>,
}

impl MemoryMeterStore {
    /// An empty store bound to `tenant`.
    #[must_use]
    pub fn new(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// How many events still hold raw metadata (that is, before compaction).
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.lock()
            .events
            .values()
            .filter(|stored| stored.raw)
            .count()
    }

    /// How many deduplication tombstones the store holds.
    #[must_use]
    pub fn tombstones(&self) -> usize {
        self.lock().events.len()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn key(period: &PeriodKey, meter: &Meter) -> (PeriodKey, Meter) {
        (period.clone(), meter.clone())
    }
}

impl MeterStore for MemoryMeterStore {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn add_events<'a>(
        &'a self,
        events: Vec<MeterEvent>,
        resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<usize, MeteringError>> {
        Box::pin(async move {
            if events.len() > MAX_BATCH_EVENTS {
                return Err(MeteringError::BatchTooLarge {
                    size: events.len(),
                    max: MAX_BATCH_EVENTS,
                });
            }
            // Tenant binding first, before a single id is resolved or remembered: an
            // event for another tenant must not reach a total or a tombstone.
            if events.iter().any(|event| event.tenant != self.tenant) {
                return Err(MeteringError::TenantMismatch);
            }
            let mut inner = self.lock();
            let floor = inner.floor.clone();
            // Resolve and validate before touching totals.
            let prepared = events
                .into_iter()
                .map(|event| {
                    if !event.is_well_formed() {
                        return Err(MeteringError::MalformedEvent {
                            id: event.id,
                            meter: event.meter.to_string(),
                        });
                    }
                    let period = resolver.resolve(&event.tenant, event.at)?;
                    // Past the floor the id may have been purged, so the event is
                    // refused rather than risk counting it twice.
                    if let Some(floor) = &floor
                        && period.end_exclusive <= floor.start
                    {
                        return Err(MeteringError::LateBeyondHorizon {
                            id: event.id,
                            period: period.label,
                        });
                    }
                    Ok((event, period))
                })
                .collect::<Result<Vec<_>, MeteringError>>()?;
            // Preflight exact counter sums so an overflow cannot partly accept a batch.
            let mut seen = BTreeSet::new();
            let mut sums = BTreeMap::new();
            for (event, period) in &prepared {
                if inner.events.contains_key(&event.id) || !seen.insert(event.id.clone()) {
                    continue;
                }
                if event.meter.kind() == MeterKind::Counter {
                    let key = Self::key(period, &event.meter);
                    let previous = inner.totals.get(&key).map_or(0, |row| row.sum);
                    let total = sums.entry(key).or_insert(previous);
                    *total = total
                        .checked_add(event.amount)
                        .ok_or(MeteringError::CounterOverflow)?;
                }
            }
            let mut added = 0usize;
            for (event, period) in prepared {
                if inner.events.contains_key(&event.id) {
                    continue;
                }
                let key = Self::key(&period, &event.meter);
                match event.meter.kind() {
                    MeterKind::Counter => {
                        let totals = inner.totals.entry(key).or_default();
                        totals.sum = totals
                            .sum
                            .checked_add(event.amount)
                            .ok_or(MeteringError::CounterOverflow)?;
                    }
                    MeterKind::Gauge => {
                        let totals = inner.totals.entry(key).or_default();
                        totals.max = totals.max.max(event.amount);
                    }
                    MeterKind::DistinctSet => {
                        if let Some(member) = event.member {
                            let set = inner.members.entry(key.clone()).or_default();
                            set.insert(member);
                            let distinct = set.len() as u64;
                            inner.totals.entry(key).or_default().distinct = distinct;
                        }
                    }
                }
                inner
                    .events
                    .insert(event.id, StoredEvent { period, raw: true });
                added += 1;
            }
            Ok(added)
        })
    }

    fn period_totals<'a>(
        &'a self,
        tenant: &'a str,
        period: &'a PeriodKey,
        _resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<TenantPeriodUsage, MeteringError>> {
        Box::pin(async move {
            if tenant != self.tenant {
                return Err(MeteringError::TenantMismatch);
            }
            let inner = self.lock();
            let mut usage = TenantPeriodUsage::default();
            for ((row_period, meter), totals) in &inner.totals {
                if row_period != period {
                    continue;
                }
                usage.by_meter.insert(meter.clone(), totals.clone());
                if let Some(members) = inner.members.get(&(row_period.clone(), meter.clone())) {
                    usage
                        .member_sets
                        .insert(meter.clone(), members.iter().cloned().collect());
                }
            }
            Ok(usage)
        })
    }

    fn distinct_members<'a>(
        &'a self,
        tenant: &'a str,
        period: &'a PeriodKey,
        meter: &'a Meter,
        _resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<Vec<MemberDigest>, MeteringError>> {
        Box::pin(async move {
            if tenant != self.tenant {
                return Err(MeteringError::TenantMismatch);
            }
            let inner = self.lock();
            let key = Self::key(period, meter);
            Ok(inner
                .members
                .get(&key)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default())
        })
    }

    fn tombstone_count<'a>(&'a self) -> BoxFuture<'a, Result<u64, MeteringError>> {
        Box::pin(async move { Ok(self.lock().events.len() as u64) })
    }

    fn compact<'a>(
        &'a self,
        before: &'a PeriodKey,
        policy: &'a RetentionPolicy,
        resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<CompactionReport, MeteringError>> {
        Box::pin(async move {
            let policy = policy.validated()?;
            let cut = policy.tombstone_cut(before, &self.tenant, resolver)?;
            let mut inner = self.lock();
            let mut events_dropped = 0u64;
            for stored in inner.events.values_mut() {
                if stored.raw && stored.period.end_exclusive <= before.start {
                    stored.raw = false;
                    events_dropped += 1;
                }
            }
            let mut tombstones_dropped = 0u64;
            if let Some(cut) = cut {
                let before_len = inner.events.len();
                inner
                    .events
                    .retain(|_, stored| stored.period.end_exclusive > cut.start);
                tombstones_dropped = u64::try_from(before_len - inner.events.len()).unwrap_or(0);
                // The floor only moves forward: a compaction with an earlier cut can
                // never re-open a period whose tombstones are gone.
                let advances = inner
                    .floor
                    .as_ref()
                    .is_none_or(|floor| cut.start > floor.start);
                if advances {
                    inner.floor = Some(cut);
                }
            }
            Ok(CompactionReport {
                events_dropped,
                tombstones_dropped,
                tombstone_floor: inner.floor.clone(),
            })
        })
    }
}

/// A convenience for the common "totals right now" read: seeds a store read with a
/// resolver the caller already holds.
///
/// # Errors
/// Any store error, including [`MeteringError::TenantMismatch`] when `tenant` is not
/// the store's tenant.
pub async fn totals_for(
    store: &dyn MeterStore,
    tenant: &str,
    period: &PeriodKey,
    resolver: &dyn PeriodResolver,
) -> Result<TenantPeriodUsage, MeteringError> {
    store.period_totals(tenant, period, resolver).await
}

/// Compacts a store to keep only periods at or after `before`, under `policy`.
///
/// # Errors
/// Any store error.
pub async fn compact_up_to(
    store: &dyn MeterStore,
    before: &PeriodKey,
    policy: &RetentionPolicy,
    resolver: &dyn PeriodResolver,
) -> Result<CompactionReport, MeteringError> {
    store.compact(before, policy, resolver).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::period::{PeriodSpec, TenantPeriods};

    const FEBRUARY: i64 = 2_678_400;

    fn resolver() -> TenantPeriods {
        TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap())
    }

    fn store() -> MemoryMeterStore {
        MemoryMeterStore::new("acme")
    }

    #[tokio::test]
    async fn adding_the_same_event_twice_counts_once() {
        let store = store();
        let res = resolver();
        let event = MeterEvent::amount("doc-1", "acme", Meter::Transactions, 5, 0);
        assert_eq!(
            store.add_events(vec![event.clone()], &res).await.unwrap(),
            1
        );
        assert_eq!(store.add_events(vec![event], &res).await.unwrap(), 0);
        let period = crate::period::period_of(0, res.spec_for("acme")).unwrap();
        let usage = store.period_totals("acme", &period, &res).await.unwrap();
        assert_eq!(usage.total(&Meter::Transactions), 5);
    }

    #[tokio::test]
    async fn a_gauge_keeps_the_maximum_not_the_sum() {
        let store = store();
        let res = resolver();
        store
            .add_events(
                vec![
                    MeterEvent::amount("s1", "acme", Meter::StorageBytes, 100, 0),
                    MeterEvent::amount("s2", "acme", Meter::StorageBytes, 250, 10),
                    MeterEvent::amount("s3", "acme", Meter::StorageBytes, 175, 20),
                ],
                &res,
            )
            .await
            .unwrap();
        let period = crate::period::period_of(0, res.spec_for("acme")).unwrap();
        let usage = store.period_totals("acme", &period, &res).await.unwrap();
        assert_eq!(usage.total(&Meter::StorageBytes), 250);
    }

    #[tokio::test]
    async fn distinct_members_are_counted_once_and_by_digest() {
        let store = store();
        let res = resolver();
        let member = |id: &str, digest: &str| {
            MeterEvent::member(id, "acme", Meter::ActiveUsers, MemberDigest::new(digest), 0)
        };
        store
            .add_events(
                vec![member("s1", "u1"), member("s2", "u2"), member("s3", "u1")],
                &res,
            )
            .await
            .unwrap();
        let period = crate::period::period_of(0, res.spec_for("acme")).unwrap();
        let usage = store.period_totals("acme", &period, &res).await.unwrap();
        assert_eq!(usage.total(&Meter::ActiveUsers), 2);
        let members = store
            .distinct_members("acme", &period, &Meter::ActiveUsers, &res)
            .await
            .unwrap();
        assert_eq!(members.len(), 2);
    }

    #[tokio::test]
    async fn compaction_drops_old_events_but_keeps_totals() {
        let store = store();
        let res = resolver();
        // January event and a February event.
        store
            .add_events(
                vec![
                    MeterEvent::amount("jan", "acme", Meter::Transactions, 3, 0),
                    MeterEvent::amount("feb", "acme", Meter::Transactions, 4, FEBRUARY),
                ],
                &res,
            )
            .await
            .unwrap();
        let february = crate::period::period_of(FEBRUARY, res.spec_for("acme")).unwrap();
        let report = store
            .compact(&february, &RetentionPolicy::default(), &res)
            .await
            .unwrap();
        assert_eq!(report.events_dropped, 1);
        // The default horizon keeps every tombstone, and sets a floor far enough back
        // that January is still inside it.
        assert_eq!(report.tombstones_dropped, 0);
        assert!(
            report
                .tombstone_floor
                .as_ref()
                .is_some_and(|floor| floor.start < 0),
            "the default horizon reaches back past January 1970: {:?}",
            report.tombstone_floor
        );
        assert_eq!(store.event_count(), 1);
        assert_eq!(store.tombstones(), 2);
        // The February total survives compaction.
        let usage = store.period_totals("acme", &february, &res).await.unwrap();
        assert_eq!(usage.total(&Meter::Transactions), 4);
    }

    /// Recorded security finding: the invariant that lets compaction forget an id at all. Past the
    /// horizon the tombstone is gone, and the *same* cut refuses the period, so a
    /// replay is either a no-op or an error — never a second count.
    #[tokio::test]
    async fn a_purged_id_is_refused_beyond_the_horizon_and_never_counted_twice() {
        let store = store();
        let res = resolver();
        let january = crate::period::period_of(0, res.spec_for("acme")).unwrap();
        let february = crate::period::period_of(FEBRUARY, res.spec_for("acme")).unwrap();
        // The cut is the month after the newest recorded one: both are final.
        let march = res.resolve("acme", february.end_exclusive).unwrap();
        let jan = MeterEvent::amount("jan", "acme", Meter::Transactions, 3, 0);
        let feb = MeterEvent::amount("feb", "acme", Meter::Transactions, 4, FEBRUARY);
        store
            .add_events(vec![jan.clone(), feb.clone()], &res)
            .await
            .unwrap();
        let before = store.period_totals("acme", &january, &res).await.unwrap();

        // A horizon below the minimum is refused and purges nothing: it would turn a
        // still-retryable event into unbilled usage.
        let april = res.resolve("acme", march.end_exclusive).unwrap();
        let too_short = store
            .compact(&april, &RetentionPolicy::new(1), &res)
            .await
            .unwrap_err();
        assert_eq!(too_short.code(), "metering.retention_horizon_too_short");
        assert_eq!(store.tombstones(), 2);

        // A two-period horizon behind April: February keeps its tombstone, January's
        // is past the horizon and removed.
        let report = store
            .compact(&april, &RetentionPolicy::new(2), &res)
            .await
            .unwrap();
        assert_eq!(report.tombstones_dropped, 1);
        assert_eq!(
            report
                .tombstone_floor
                .as_ref()
                .map(|floor| floor.label.clone()),
            // The floor is the first period still accepting events: February.
            Some(february.label.clone())
        );
        assert_eq!(store.tombstones(), 1);
        // Totals are untouched by the purge.
        assert_eq!(
            store.period_totals("acme", &january, &res).await.unwrap(),
            before
        );

        // Inside the horizon the tombstone answers for the id: a no-op.
        assert_eq!(store.add_events(vec![feb.clone()], &res).await.unwrap(), 0);
        // Beyond it the tombstone is gone and the id is refused, not counted again.
        let refused = store.add_events(vec![jan], &res).await.unwrap_err();
        assert_eq!(refused.code(), "metering.late_beyond_horizon");
        // A first-time event that late is refused for the same reason.
        let fresh = MeterEvent::amount("never-seen", "acme", Meter::Transactions, 50, 0);
        assert_eq!(
            store
                .add_events(vec![fresh], &res)
                .await
                .unwrap_err()
                .code(),
            "metering.late_beyond_horizon"
        );
        // Every January total is exactly what it was before the replay attempts.
        assert_eq!(
            store.period_totals("acme", &january, &res).await.unwrap(),
            before
        );
        // The floor never recedes: a later compaction whose horizon reaches further
        // back cannot re-open January.
        let again = store
            .compact(&february, &RetentionPolicy::new(2), &res)
            .await
            .unwrap();
        assert_eq!(
            again.tombstone_floor.as_ref().map(|floor| floor.start),
            Some(february.start)
        );
        assert_eq!(again.tombstones_dropped, 0);
        assert_eq!(store.tombstones(), 1);
    }

    #[tokio::test]
    async fn an_event_of_another_tenant_is_refused_before_any_write() {
        let store = store();
        let res = resolver();
        let foreign = MeterEvent::amount("shared", "globex", Meter::Transactions, 9, 0);
        assert_eq!(
            store.add_events(vec![foreign], &res).await.unwrap_err(),
            MeteringError::TenantMismatch
        );
        assert_eq!(store.tombstones(), 0);
        assert_eq!(store.event_count(), 0);
        let january = crate::period::period_of(0, res.spec_for("acme")).unwrap();
        assert!(
            store
                .period_totals("acme", &january, &res)
                .await
                .unwrap()
                .by_meter
                .is_empty()
        );
        // A read for another tenant is refused too.
        assert_eq!(
            store
                .period_totals("globex", &january, &res)
                .await
                .unwrap_err(),
            MeteringError::TenantMismatch
        );
    }

    #[tokio::test]
    async fn a_batch_larger_than_the_bound_is_refused_whole() {
        let store = store();
        let res = resolver();
        let events: Vec<MeterEvent> = (0..=MAX_BATCH_EVENTS)
            .map(|i| MeterEvent::amount(format!("e{i}"), "acme", Meter::Transactions, 1, 0))
            .collect();
        assert_eq!(
            store.add_events(events, &res).await.unwrap_err(),
            MeteringError::BatchTooLarge {
                size: MAX_BATCH_EVENTS + 1,
                max: MAX_BATCH_EVENTS,
            }
        );
        assert_eq!(store.tombstones(), 0);
        // Exactly the bound is accepted.
        let events: Vec<MeterEvent> = (0..MAX_BATCH_EVENTS)
            .map(|i| MeterEvent::amount(format!("e{i}"), "acme", Meter::Transactions, 1, 0))
            .collect();
        assert_eq!(
            store.add_events(events, &res).await.unwrap(),
            MAX_BATCH_EVENTS
        );
    }
}
