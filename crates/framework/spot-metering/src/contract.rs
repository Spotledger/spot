//! The backend-independent store contract: the properties every [`MeterStore`] must
//! satisfy, runnable against any implementation.
//!
//! A later task writes a PostgreSQL store; this module is what makes "it is a
//! meter store" a checkable claim rather than an assertion. [`run_store_contract`]
//! exercises an implementation with the same cases the in-memory store passes, and
//! [`StoreContractCase`] names each so a failure says which property broke.
//!
//! Every case gets a **fresh store** from the [`StoreFactory`], bound to a tenant of
//! its own, because a store refuses events for another tenant: the suite cannot share
//! one instance across cases any more than a deployment can share one instance across
//! customers.

use std::sync::Arc;

use crate::error::MeteringError;
use crate::event::MeterEvent;
use crate::meter::{MemberDigest, Meter};
use crate::period::{PeriodKey, PeriodResolver, PeriodSpec, TenantPeriods};
use crate::retention::RetentionPolicy;
use crate::store::{MAX_BATCH_EVENTS, MeterStore};

/// Creates an empty store bound to one tenant.
///
/// A backend's test module implements this with its own constructor (a pool plus a
/// tenant for PostgreSQL; a plain `MemoryMeterStore::new` for the reference store).
/// A closure `Fn(&str) -> Arc<dyn MeterStore>` is a factory by the blanket impl below.
pub trait StoreFactory: Send + Sync {
    /// An empty store bound to `tenant` and to nothing else.
    fn for_tenant(&self, tenant: &str) -> Arc<dyn MeterStore>;
}

impl<F> StoreFactory for F
where
    F: Fn(&str) -> Arc<dyn MeterStore> + Send + Sync,
{
    fn for_tenant(&self, tenant: &str) -> Arc<dyn MeterStore> {
        self(tenant)
    }
}

/// One property the store contract checks; every implementation must pass all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StoreContractCase {
    /// The same event id added twice counts once.
    IdempotentByEventId,
    /// A duplicated batch (a retry) adds nothing new.
    DuplicatedBatchIsANoOp,
    /// Events may arrive in any order and the totals are the same.
    OrderIndependent,
    /// A counter is summed.
    CounterSums,
    /// A gauge keeps the maximum, never the sum.
    GaugeKeepsMaximum,
    /// A distinct set counts a repeated member once.
    DistinctSetCountsOnce,
    /// Events in different periods are bucketed separately.
    PeriodsAreSeparateBuckets,
    /// Totals are per tenant and never leak between tenants; the same event id in two
    /// tenants is two separate facts.
    TenantsAreIsolated,
    /// Compaction drops old raw events and keeps the totals and the tombstones.
    CompactionKeepsTotals,
    /// Compaction past the retention horizon removes tombstones and keeps the totals.
    TombstonesExpireBeyondTheHorizon,
    /// A replay of an id whose tombstone survives is a no-op.
    ReplayInsideTheHorizonIsANoOp,
    /// An event past the horizon is refused, whether or not its id was ever seen.
    LateArrivalBeyondTheHorizonIsRefused,
    /// A store writes and reads only its own tenant, refused before any write.
    TenantIsBoundAtConstruction,
    /// One call cannot carry more than [`MAX_BATCH_EVENTS`] events.
    BatchSizeIsBounded,
    /// A retention horizon below [`RetentionPolicy::MIN_HORIZON_PERIODS`] is refused by
    /// compaction and purges nothing.
    HorizonBelowTheMinimumIsRefused,
}

impl StoreContractCase {
    /// Every case, for a caller that wants to report coverage.
    #[must_use]
    pub fn all() -> [StoreContractCase; 15] {
        [
            Self::IdempotentByEventId,
            Self::DuplicatedBatchIsANoOp,
            Self::OrderIndependent,
            Self::CounterSums,
            Self::GaugeKeepsMaximum,
            Self::DistinctSetCountsOnce,
            Self::PeriodsAreSeparateBuckets,
            Self::TenantsAreIsolated,
            Self::CompactionKeepsTotals,
            Self::TombstonesExpireBeyondTheHorizon,
            Self::ReplayInsideTheHorizonIsANoOp,
            Self::LateArrivalBeyondTheHorizonIsRefused,
            Self::TenantIsBoundAtConstruction,
            Self::BatchSizeIsBounded,
            Self::HorizonBelowTheMinimumIsRefused,
        ]
    }
}

/// The instant of the first second of the second calendar month after the epoch
/// (1970-02-01T00:00:00Z), the "recent period" of the contract cases.
const FEBRUARY: i64 = 2_678_400;

/// Runs every contract case against stores `factory` builds, returning the cases that
/// passed.
///
/// A caller (a backend store's test module) asserts the returned list is
/// exactly [`StoreContractCase::all`]. A failing case returns its
/// [`MeteringError`] (or panics on a pure assertion), so the caller can print which
/// property and why.
///
/// # Errors
/// The first case that failed, with the store's error.
pub async fn run_store_contract(
    factory: &dyn StoreFactory,
) -> Result<Vec<StoreContractCase>, (StoreContractCase, MeteringError)> {
    let mut passed = Vec::new();
    for (ordinal, case) in StoreContractCase::all().into_iter().enumerate() {
        run_case(factory, case, ordinal)
            .await
            .map_err(|error| (case, error))?;
        passed.push(case);
    }
    Ok(passed)
}

async fn run_case(
    factory: &dyn StoreFactory,
    case: StoreContractCase,
    ordinal: usize,
) -> Result<(), MeteringError> {
    let resolver = TenantPeriods::new(PeriodSpec::calendar_month("UTC")?);
    let tenant = format!("contract-{ordinal}");
    let store = factory.for_tenant(&tenant);
    let store = store.as_ref();
    // Every event carries the store's own tenant: an event for another tenant is
    // refused, and that refusal has its own case below.
    let tenant = store.tenant().to_owned();
    let tenant = tenant.as_str();
    match case {
        StoreContractCase::IdempotentByEventId => {
            let event = MeterEvent::amount("e1", tenant, Meter::Transactions, 5, 0);
            assert_eq!(store.add_events(vec![event.clone()], &resolver).await?, 1);
            assert_eq!(store.add_events(vec![event], &resolver).await?, 0);
            Ok(())
        }
        StoreContractCase::DuplicatedBatchIsANoOp => {
            let batch = vec![
                MeterEvent::amount("a", tenant, Meter::Transactions, 1, 0),
                MeterEvent::amount("b", tenant, Meter::Transactions, 2, 0),
            ];
            store.add_events(batch.clone(), &resolver).await?;
            assert_eq!(store.add_events(batch, &resolver).await?, 0);
            Ok(())
        }
        StoreContractCase::OrderIndependent => {
            let forward = [
                MeterEvent::amount("o1", tenant, Meter::Transactions, 3, 0),
                MeterEvent::amount("o2", tenant, Meter::Transactions, 4, 0),
                MeterEvent::amount("o3", tenant, Meter::Transactions, 5, 0),
            ];
            let mut reversed = forward.to_vec();
            reversed.reverse();
            store.add_events(reversed.to_vec(), &resolver).await?;
            let period = a_period(&resolver, tenant)?;
            let usage = store.period_totals(tenant, &period, &resolver).await?;
            assert_eq!(usage.total(&Meter::Transactions), 12);
            Ok(())
        }
        StoreContractCase::CounterSums => {
            store
                .add_events(
                    vec![
                        MeterEvent::amount("c1", tenant, Meter::AiTokens, 100, 0),
                        MeterEvent::amount("c2", tenant, Meter::AiTokens, 250, 0),
                    ],
                    &resolver,
                )
                .await?;
            let period = a_period(&resolver, tenant)?;
            let usage = store.period_totals(tenant, &period, &resolver).await?;
            assert_eq!(usage.total(&Meter::AiTokens), 350);
            Ok(())
        }
        StoreContractCase::GaugeKeepsMaximum => {
            store
                .add_events(
                    vec![
                        MeterEvent::amount("g1", tenant, Meter::StorageBytes, 10, 0),
                        MeterEvent::amount("g2", tenant, Meter::StorageBytes, 900, 0),
                        MeterEvent::amount("g3", tenant, Meter::StorageBytes, 40, 0),
                    ],
                    &resolver,
                )
                .await?;
            let period = a_period(&resolver, tenant)?;
            let usage = store.period_totals(tenant, &period, &resolver).await?;
            assert_eq!(usage.total(&Meter::StorageBytes), 900);
            Ok(())
        }
        StoreContractCase::DistinctSetCountsOnce => {
            let member = |id: &str, digest: &str| {
                MeterEvent::member(id, tenant, Meter::ActiveUsers, MemberDigest::new(digest), 0)
            };
            store
                .add_events(
                    vec![member("m1", "u1"), member("m2", "u1"), member("m3", "u2")],
                    &resolver,
                )
                .await?;
            let period = a_period(&resolver, tenant)?;
            let usage = store.period_totals(tenant, &period, &resolver).await?;
            assert_eq!(usage.total(&Meter::ActiveUsers), 2);
            Ok(())
        }
        StoreContractCase::PeriodsAreSeparateBuckets => {
            // Two instants one month apart (January and February, UTC).
            store
                .add_events(
                    vec![
                        MeterEvent::amount("p1", tenant, Meter::Transactions, 1, 0),
                        MeterEvent::amount("p2", tenant, Meter::Transactions, 2, FEBRUARY),
                    ],
                    &resolver,
                )
                .await?;
            let january = crate::period::period_of(0, resolver.spec_for(tenant))?;
            let february = crate::period::period_of(FEBRUARY, resolver.spec_for(tenant))?;
            assert_eq!(
                store
                    .period_totals(tenant, &january, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                1
            );
            assert_eq!(
                store
                    .period_totals(tenant, &february, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                2
            );
            Ok(())
        }
        StoreContractCase::TenantsAreIsolated => {
            let other_tenant = format!("{tenant}-other");
            let other = factory.for_tenant(&other_tenant);
            let other = other.as_ref();
            // One event id, two tenants: two facts, two totals.
            let event = |tenant: &str, amount: u64| {
                MeterEvent::amount("shared", tenant, Meter::Transactions, amount, 0)
            };
            assert_eq!(
                store.add_events(vec![event(tenant, 1)], &resolver).await?,
                1
            );
            assert_eq!(
                other
                    .add_events(vec![event(&other_tenant, 99)], &resolver)
                    .await?,
                1
            );
            let period = a_period(&resolver, tenant)?;
            assert_eq!(
                store
                    .period_totals(tenant, &period, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                1
            );
            assert_eq!(
                other
                    .period_totals(&other_tenant, &period, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                99
            );
            // Replaying one tenant's id leaves the other tenant's total alone.
            assert_eq!(
                other
                    .add_events(vec![event(&other_tenant, 50)], &resolver)
                    .await?,
                0
            );
            assert_eq!(
                store
                    .period_totals(tenant, &period, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                1
            );
            Ok(())
        }
        StoreContractCase::CompactionKeepsTotals => {
            store
                .add_events(
                    vec![
                        MeterEvent::amount("k1", tenant, Meter::Transactions, 3, 0),
                        MeterEvent::amount("k2", tenant, Meter::Transactions, 4, FEBRUARY),
                    ],
                    &resolver,
                )
                .await?;
            let february = crate::period::period_of(FEBRUARY, resolver.spec_for(tenant))?;
            let report = store
                .compact(&february, &RetentionPolicy::default(), &resolver)
                .await?;
            assert_eq!(report.events_dropped, 1);
            // The default horizon keeps every tombstone, so a replay is still a no-op.
            assert_eq!(report.tombstones_dropped, 0);
            assert_eq!(store.tombstone_count().await?, 2);
            let january = resolver.resolve(tenant, 0)?;
            assert_eq!(
                store
                    .period_totals(tenant, &january, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                3
            );
            assert_eq!(
                store
                    .add_events(
                        vec![MeterEvent::amount("k1", tenant, Meter::Transactions, 3, 0)],
                        &resolver
                    )
                    .await?,
                0
            );
            assert_eq!(
                store
                    .period_totals(tenant, &january, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                3
            );
            assert_eq!(
                store
                    .period_totals(tenant, &february, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                4
            );
            Ok(())
        }
        StoreContractCase::TombstonesExpireBeyondTheHorizon => {
            let (january, february, totals) = seed_two_periods(store, tenant, &resolver).await?;
            // Two months after the newest recorded one: both periods are final and the
            // two-period minimum horizon puts the cut at February.
            let march = resolver.resolve(tenant, february.end_exclusive)?;
            let april = resolver.resolve(tenant, march.end_exclusive)?;
            assert_eq!(store.tombstone_count().await?, 3);
            let report = store
                .compact(&april, &RetentionPolicy::new(2), &resolver)
                .await?;
            // January's two ids are past the horizon and gone; February's survives.
            assert_eq!(report.tombstones_dropped, 2);
            assert_eq!(store.tombstone_count().await?, 1);
            assert_eq!(
                report.tombstone_floor.as_ref().map(|floor| floor.start),
                Some(february.start)
            );
            // The totals did not move.
            assert_eq!(
                store.period_totals(tenant, &january, &resolver).await?,
                totals
            );
            assert_eq!(
                store
                    .distinct_members(tenant, &january, &Meter::ActiveUsers, &resolver)
                    .await?,
                vec![MemberDigest::new("u1")]
            );
            assert_eq!(
                store
                    .period_totals(tenant, &february, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                4
            );
            Ok(())
        }
        StoreContractCase::ReplayInsideTheHorizonIsANoOp => {
            let (_january, february, _totals) = seed_two_periods(store, tenant, &resolver).await?;
            let march = resolver.resolve(tenant, february.end_exclusive)?;
            let april = resolver.resolve(tenant, march.end_exclusive)?;
            store
                .compact(&april, &RetentionPolicy::new(2), &resolver)
                .await?;
            let inside = MeterEvent::amount("feb", tenant, Meter::Transactions, 4, FEBRUARY);
            assert_eq!(store.add_events(vec![inside], &resolver).await?, 0);
            assert_eq!(
                store
                    .period_totals(tenant, &february, &resolver)
                    .await?
                    .total(&Meter::Transactions),
                4
            );
            Ok(())
        }
        StoreContractCase::LateArrivalBeyondTheHorizonIsRefused => {
            let (january, february, totals) = seed_two_periods(store, tenant, &resolver).await?;
            let march = resolver.resolve(tenant, february.end_exclusive)?;
            let april = resolver.resolve(tenant, march.end_exclusive)?;
            store
                .compact(&april, &RetentionPolicy::new(2), &resolver)
                .await?;
            // A replay of the purged id, with a different amount: refused, not counted.
            let replay = MeterEvent::amount("jan", tenant, Meter::Transactions, 1_000, 0);
            let Err(error) = store.add_events(vec![replay], &resolver).await else {
                panic!("a replay past the horizon must be refused");
            };
            assert_eq!(error.code(), "metering.late_beyond_horizon");
            // An id the store has never seen, that late: refused for the same reason.
            let fresh = MeterEvent::amount("never-seen", tenant, Meter::Transactions, 7, 0);
            let Err(error) = store.add_events(vec![fresh], &resolver).await else {
                panic!("an event past the horizon must be refused");
            };
            assert_eq!(error.code(), "metering.late_beyond_horizon");
            // Exactly the total from before the purge attempts.
            assert_eq!(
                store.period_totals(tenant, &january, &resolver).await?,
                totals
            );
            Ok(())
        }
        StoreContractCase::TenantIsBoundAtConstruction => {
            let other_tenant = format!("{tenant}-foreign");
            // An event naming another tenant is refused before any write.
            let foreign = MeterEvent::amount("shared", &other_tenant, Meter::Transactions, 5, 0);
            let Err(error) = store.add_events(vec![foreign], &resolver).await else {
                panic!("an event of another tenant must be refused");
            };
            assert_eq!(error, MeteringError::TenantMismatch);
            assert_eq!(error.code(), "metering.tenant_mismatch");
            assert_eq!(store.tombstone_count().await?, 0);
            let period = a_period(&resolver, tenant)?;
            assert!(
                store
                    .period_totals(tenant, &period, &resolver)
                    .await?
                    .by_meter
                    .is_empty()
            );
            // Nor may the store be used to read another tenant's totals.
            assert_eq!(
                store.period_totals(&other_tenant, &period, &resolver).await,
                Err(MeteringError::TenantMismatch)
            );
            // A mixed batch is refused whole: the good event does not land either.
            assert_eq!(
                store
                    .add_events(
                        vec![
                            MeterEvent::amount("own", tenant, Meter::Transactions, 1, 0),
                            MeterEvent::amount("theirs", &other_tenant, Meter::Transactions, 2, 0),
                        ],
                        &resolver,
                    )
                    .await,
                Err(MeteringError::TenantMismatch)
            );
            assert_eq!(store.tombstone_count().await?, 0);
            Ok(())
        }
        StoreContractCase::BatchSizeIsBounded => {
            let events: Vec<MeterEvent> = (0..=MAX_BATCH_EVENTS)
                .map(|i| MeterEvent::amount(format!("b{i}"), tenant, Meter::Transactions, 1, 0))
                .collect();
            let Err(error) = store.add_events(events, &resolver).await else {
                panic!("a batch past the bound must be refused");
            };
            assert_eq!(
                error,
                MeteringError::BatchTooLarge {
                    size: MAX_BATCH_EVENTS + 1,
                    max: MAX_BATCH_EVENTS,
                }
            );
            // Refused whole, before a single id was remembered.
            assert_eq!(store.tombstone_count().await?, 0);
            Ok(())
        }
        StoreContractCase::HorizonBelowTheMinimumIsRefused => {
            let (january, february, totals) = seed_two_periods(store, tenant, &resolver).await?;
            let march = resolver.resolve(tenant, february.end_exclusive)?;
            let april = resolver.resolve(tenant, march.end_exclusive)?;
            // A horizon inside a producer's retry window is a configuration error, not a
            // storage trade-off: nothing is purged and the floor does not move.
            let Err(error) = store
                .compact(&april, &RetentionPolicy::new(1), &resolver)
                .await
            else {
                panic!("a horizon below the minimum must be refused");
            };
            assert_eq!(error.code(), "metering.retention_horizon_too_short");
            assert_eq!(store.tombstone_count().await?, 3);
            assert_eq!(
                store.period_totals(tenant, &january, &resolver).await?,
                totals
            );
            Ok(())
        }
    }
}

/// Two periods of usage: January (a counter and a distinct member) and February (a
/// counter), returning both windows and January's totals as they stood before any
/// compaction.
async fn seed_two_periods(
    store: &dyn MeterStore,
    tenant: &str,
    resolver: &dyn PeriodResolver,
) -> Result<(PeriodKey, PeriodKey, crate::totals::TenantPeriodUsage), MeteringError> {
    store
        .add_events(
            vec![
                MeterEvent::amount("jan", tenant, Meter::Transactions, 3, 0),
                MeterEvent::member("jm", tenant, Meter::ActiveUsers, MemberDigest::new("u1"), 5),
                MeterEvent::amount("feb", tenant, Meter::Transactions, 4, FEBRUARY),
            ],
            resolver,
        )
        .await?;
    let january = resolver.resolve(tenant, 0)?;
    let february = resolver.resolve(tenant, FEBRUARY)?;
    let totals = store.period_totals(tenant, &january, resolver).await?;
    Ok((january, february, totals))
}

fn a_period(resolver: &dyn PeriodResolver, tenant: &str) -> Result<PeriodKey, MeteringError> {
    resolver.resolve(tenant, 0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::store::MemoryMeterStore;

    #[tokio::test]
    async fn the_in_memory_store_passes_every_contract_case() {
        let factory = |tenant: &str| -> Arc<dyn MeterStore> {
            Arc::new(MemoryMeterStore::new(tenant.to_owned()))
        };
        let passed = run_store_contract(&factory).await.unwrap();
        assert_eq!(passed, StoreContractCase::all());
    }
}
