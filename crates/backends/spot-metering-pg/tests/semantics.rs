//! Behaviour beyond the contract suite: concurrency, atomicity, compaction with replay, the
//! retention horizon, tenant binding, reconciliation against an independent truth, and parity with
//! the in-memory store.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{FEBRUARY, Rng, february, january, resolver, store, window};
use spot_metering::{
    CompactionReport, MAX_BATCH_EVENTS, MemberDigest, MemoryMeterStore, Meter, MeterEvent,
    MeterStore, MeteringError, RetentionPolicy, TallyTruth, reconcile,
};
use spot_metering_pg::PgMeterStore;
use spot_postgres::testing::TestDb;

fn tx(id: &str, tenant: &str, amount: u64, at: i64) -> MeterEvent {
    MeterEvent::amount(id, tenant, Meter::Transactions, amount, at)
}

async fn event_rows(db: &TestDb, filter: &str) -> i64 {
    let raw = db.raw_client().await;
    let sql = format!("SELECT count(*) FROM spot_meter_event WHERE {filter}");
    raw.query_one(&sql, &[]).await.unwrap().get(0)
}

/// The first second of the period after `period`, in UTC calendar months.
fn next_period(period: &spot_metering::PeriodKey) -> spot_metering::PeriodKey {
    window(period.end_exclusive)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_adding_the_same_batch_count_it_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(store(&db, 16, "acme").await);
    let mut batch = Vec::new();
    for i in 0..200u64 {
        batch.push(tx(&format!("e{i}"), "acme", i + 1, 0));
        batch.push(MeterEvent::amount(
            format!("g{i}"),
            "acme",
            Meter::StorageBytes,
            i * 3,
            10,
        ));
        batch.push(MeterEvent::member(
            format!("m{i}"),
            "acme",
            Meter::ActiveUsers,
            MemberDigest::new(format!("u{}", i % 50)),
            20,
        ));
    }
    let mut tasks = Vec::new();
    for worker in 0..8u64 {
        let store = Arc::clone(&store);
        let mut mine = batch.clone();
        Rng(0x9E37_79B9 + worker).shuffle(&mut mine);
        tasks.push(tokio::spawn(async move {
            store.add_events(mine, &resolver()).await.unwrap()
        }));
    }
    let mut recorded = 0;
    for task in tasks {
        recorded += task.await.unwrap();
    }
    assert_eq!(
        recorded,
        batch.len(),
        "every event recorded by exactly one caller"
    );
    let usage = store
        .period_totals("acme", &january(), &resolver())
        .await
        .unwrap();
    assert_eq!(usage.total(&Meter::Transactions), (1..=200u64).sum::<u64>());
    assert_eq!(usage.total(&Meter::StorageBytes), 199 * 3);
    assert_eq!(usage.total(&Meter::ActiveUsers), 50);
    assert_eq!(usage.members_of(&Meter::ActiveUsers).len(), 50);
    assert_eq!(event_rows(&db, "true").await, batch.len() as i64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlapping_batches_from_many_tasks_never_double_count() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(store(&db, 16, "acme").await);
    let all: Vec<MeterEvent> = (0..300u64)
        .map(|i| tx(&format!("o{i}"), "acme", 2, 0))
        .collect();
    let mut tasks = Vec::new();
    for worker in 0..6usize {
        let store = Arc::clone(&store);
        // Each task sends a different overlapping window of the same events, in its own order.
        let mut mine: Vec<MeterEvent> = all[worker * 40..(worker * 40 + 140).min(300)].to_vec();
        Rng(7 + worker as u64).shuffle(&mut mine);
        tasks.push(tokio::spawn(async move {
            store.add_events(mine, &resolver()).await.unwrap()
        }));
    }
    let mut recorded = 0;
    for task in tasks {
        recorded += task.await.unwrap();
    }
    assert_eq!(recorded, 300);
    let usage = store
        .period_totals("acme", &january(), &resolver())
        .await
        .unwrap();
    assert_eq!(usage.total(&Meter::Transactions), 600);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_that_fails_midway_leaves_nothing_visible() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "acme").await;
    let res = resolver();
    store
        .add_events(vec![tx("base", "acme", u64::MAX - 10, 0)], &res)
        .await
        .unwrap();
    // Three fine events, a gauge, a member, then a counter that overflows the stored sum.
    let batch = vec![
        tx("a", "acme", 1, 0),
        tx("b", "acme", 2, 0),
        MeterEvent::amount("g", "acme", Meter::StorageBytes, 77, 0),
        MeterEvent::member("m", "acme", Meter::ActiveUsers, MemberDigest::new("u1"), 0),
        tx("boom", "acme", 100, 0),
    ];
    let error = store.add_events(batch.clone(), &res).await.unwrap_err();
    assert_eq!(error, MeteringError::CounterOverflow);
    assert_eq!(
        event_rows(&db, "event_id <> 'base'").await,
        0,
        "no event row survived"
    );
    let usage = store.period_totals("acme", &january(), &res).await.unwrap();
    assert_eq!(usage.total(&Meter::Transactions), u64::MAX - 10);
    assert_eq!(usage.total(&Meter::StorageBytes), 0);
    assert_eq!(usage.total(&Meter::ActiveUsers), 0);
    assert!(usage.members_of(&Meter::ActiveUsers).is_empty());
    // Ids of the failed batch were not burnt: a corrected retry records them all.
    let mut fixed = batch;
    fixed[4] = tx("boom", "acme", 5, 0);
    assert_eq!(store.add_events(fixed, &res).await.unwrap(), 5);
    let usage = store.period_totals("acme", &january(), &res).await.unwrap();
    assert_eq!(usage.total(&Meter::Transactions), u64::MAX - 10 + 1 + 2 + 5);
    assert_eq!(usage.total(&Meter::StorageBytes), 77);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn counters_are_exact_to_u64_max_and_overflow_is_refused() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "acme").await;
    let other = common::store(&db, 4, "other").await;
    let res = resolver();
    store
        .add_events(vec![tx("big", "acme", u64::MAX, 0)], &res)
        .await
        .unwrap();
    let usage = store.period_totals("acme", &january(), &res).await.unwrap();
    assert_eq!(usage.total(&Meter::Transactions), u64::MAX);
    let error = store
        .add_events(vec![tx("one", "acme", 1, 0)], &res)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "metering.counter_overflow");
    // Two events in one batch that together pass u64 are refused too — in another tenant, so the
    // oversized sum is proved to be per tenant and not clamped at a shared boundary.
    let error = other
        .add_events(
            vec![tx("x", "other", u64::MAX, 0), tx("y", "other", 1, 0)],
            &res,
        )
        .await
        .unwrap_err();
    assert_eq!(error, MeteringError::CounterOverflow);
    assert_eq!(
        other
            .period_totals("other", &january(), &res)
            .await
            .unwrap()
            .total(&Meter::Transactions),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_or_unresolvable_event_refuses_the_whole_batch() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "acme").await;
    let res = resolver();
    let bad = MeterEvent::amount("bad", "acme", Meter::ActiveUsers, 1, 0);
    let error = store
        .add_events(vec![tx("ok", "acme", 1, 0), bad], &res)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "metering.malformed_event");
    assert_eq!(event_rows(&db, "true").await, 0);
    let error = store
        .add_events(vec![tx("far", "acme", 1, i64::MAX)], &res)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "metering.period_unresolvable");
    assert_eq!(event_rows(&db, "true").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_batch_of_another_tenant_is_refused_before_any_write() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "acme").await;
    let res = resolver();
    // One foreign event in an otherwise valid batch refuses the whole batch.
    let error = store
        .add_events(
            vec![tx("own", "acme", 1, 0), tx("theirs", "globex", 99, 0)],
            &res,
        )
        .await
        .unwrap_err();
    assert_eq!(error, MeteringError::TenantMismatch);
    assert_eq!(event_rows(&db, "true").await, 0);
    assert!(
        store
            .period_totals("globex", &january(), &res)
            .await
            .is_err()
    );
    // The same event id in two tenants is two facts, each in its own store.
    let globex = common::store(&db, 4, "globex").await;
    assert_eq!(
        store
            .add_events(vec![tx("shared", "acme", 5, 0)], &res)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        globex
            .add_events(vec![tx("shared", "globex", 9, 0)], &res)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .period_totals("acme", &january(), &res)
            .await
            .unwrap()
            .total(&Meter::Transactions),
        5
    );
    assert_eq!(
        globex
            .period_totals("globex", &january(), &res)
            .await
            .unwrap()
            .total(&Meter::Transactions),
        9
    );
    assert_eq!(
        event_rows(&db, "tenant = 'acme'").await,
        1,
        "one tombstone per tenant key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_keeps_tombstones_totals_and_members_and_replays_are_no_ops() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "acme").await;
    let res = resolver();
    let old: Vec<MeterEvent> = vec![
        tx("j1", "acme", 3, 0),
        tx("j2", "acme", 4, 100),
        MeterEvent::amount("jg", "acme", Meter::StorageBytes, 500, 50),
        MeterEvent::member(
            "jm1",
            "acme",
            Meter::ActiveUsers,
            MemberDigest::new("u1"),
            5,
        ),
        MeterEvent::member(
            "jm2",
            "acme",
            Meter::ActiveUsers,
            MemberDigest::new("u2"),
            6,
        ),
    ];
    let new = vec![tx("f1", "acme", 10, FEBRUARY + 5)];
    store.add_events(old.clone(), &res).await.unwrap();
    store.add_events(new.clone(), &res).await.unwrap();
    let jan_before = store.period_totals("acme", &january(), &res).await.unwrap();

    let report: CompactionReport = store
        .compact(&february(), &RetentionPolicy::default(), &res)
        .await
        .unwrap();
    assert_eq!(report.events_dropped, old.len() as u64);
    assert_eq!(
        report.tombstones_dropped, 0,
        "the default horizon keeps ids"
    );
    assert_eq!(
        event_rows(&db, "meter IS NULL").await,
        old.len() as i64,
        "raw metadata dropped"
    );
    assert_eq!(
        event_rows(&db, "meter IS NOT NULL").await,
        1,
        "the February event is untouched"
    );
    assert_eq!(
        event_rows(&db, "true").await,
        (old.len() + 1) as i64,
        "every id still has its tombstone"
    );
    assert_eq!(
        event_rows(
            &db,
            "meter IS NULL AND (amount IS NOT NULL OR member_digest IS NOT NULL)"
        )
        .await,
        0
    );
    // The period end survives compaction: it is what dates the tombstone for retention.
    assert_eq!(
        event_rows(&db, "meter IS NULL AND period_end IS NOT NULL").await,
        old.len() as i64
    );
    // The floor is recorded, far enough back that January is still inside it.
    assert!(report.tombstone_floor.is_some());

    // Totals and members are unchanged.
    assert_eq!(
        store.period_totals("acme", &january(), &res).await.unwrap(),
        jan_before
    );
    assert_eq!(
        store
            .distinct_members("acme", &january(), &Meter::ActiveUsers, &res)
            .await
            .unwrap(),
        vec![MemberDigest::new("u1"), MemberDigest::new("u2")]
    );
    // Replaying every old id, even with different amounts, changes nothing.
    let mut replay = old.clone();
    for event in &mut replay {
        event.amount += 1000;
    }
    assert_eq!(store.add_events(replay, &res).await.unwrap(), 0);
    assert_eq!(store.add_events(old, &res).await.unwrap(), 0);
    assert_eq!(
        store.period_totals("acme", &january(), &res).await.unwrap(),
        jan_before
    );
    assert_eq!(
        store
            .period_totals("acme", &february(), &res)
            .await
            .unwrap()
            .total(&Meter::Transactions),
        10
    );
    // A second compaction has nothing left; the February event is not touched by it.
    let again = store
        .compact(&february(), &RetentionPolicy::default(), &res)
        .await
        .unwrap();
    assert!(again.is_empty());
    assert_eq!(store.add_events(new, &res).await.unwrap(), 0);
}

/// Recorded security finding on PostgreSQL: past the horizon the tombstone rows are gone, the totals are not, and
/// the period is refused — so a replay can never be counted twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tombstone_horizon_purges_ids_and_refuses_their_period() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "acme").await;
    let res = resolver();
    store
        .add_events(
            vec![
                tx("jan", "acme", 3, 0),
                MeterEvent::member("jm", "acme", Meter::ActiveUsers, MemberDigest::new("u1"), 5),
                tx("feb", "acme", 4, FEBRUARY),
            ],
            &res,
        )
        .await
        .unwrap();
    let jan_before = store.period_totals("acme", &january(), &res).await.unwrap();
    // April, so January and February are both final periods; the two-period minimum
    // horizon puts the cut at February.
    let march = next_period(&february());
    let april = next_period(&march);
    // A horizon below the minimum is refused and purges nothing.
    let too_short = store
        .compact(&april, &RetentionPolicy::new(1), &res)
        .await
        .unwrap_err();
    assert_eq!(too_short.code(), "metering.retention_horizon_too_short");
    assert_eq!(event_rows(&db, "true").await, 3);
    let report = store
        .compact(&april, &RetentionPolicy::new(2), &res)
        .await
        .unwrap();
    assert_eq!(report.tombstones_dropped, 2, "January's two ids");
    assert_eq!(
        report.tombstone_floor.as_ref().map(|floor| floor.start),
        Some(february().start)
    );
    assert_eq!(
        event_rows(&db, "period_end <= 2678400").await,
        0,
        "no January tombstone row survives"
    );
    assert_eq!(
        event_rows(&db, "true").await,
        1,
        "February's tombstone is inside the horizon"
    );
    // The totals are untouched by the purge.
    assert_eq!(
        store.period_totals("acme", &january(), &res).await.unwrap(),
        jan_before
    );
    // Inside the horizon a replay is still a no-op.
    assert_eq!(
        store
            .add_events(vec![tx("feb", "acme", 4, FEBRUARY)], &res)
            .await
            .unwrap(),
        0
    );
    // Beyond it the id is refused, and so is an id never seen before.
    for event in [tx("jan", "acme", 1_000, 0), tx("never-seen", "acme", 7, 0)] {
        let error = store.add_events(vec![event], &res).await.unwrap_err();
        assert_eq!(error.code(), "metering.late_beyond_horizon");
    }
    assert_eq!(
        store.period_totals("acme", &january(), &res).await.unwrap(),
        jan_before
    );
    // The floor is durable and only moves forward: an earlier cut cannot re-open January.
    let again = store
        .compact(&february(), &RetentionPolicy::new(3), &res)
        .await
        .unwrap();
    assert_eq!(again.tombstones_dropped, 0);
    assert_eq!(
        again.tombstone_floor.as_ref().map(|floor| floor.start),
        Some(february().start)
    );
    assert_eq!(
        store
            .add_events(vec![tx("jan", "acme", 1, 0)], &res)
            .await
            .unwrap_err()
            .code(),
        "metering.late_beyond_horizon"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn compaction_runs_in_bounded_batches() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "acme").await;
    let res = resolver();
    let events: Vec<MeterEvent> = (0..12_500u64)
        .map(|i| tx(&format!("b{i}"), "acme", 1, 0))
        .collect();
    let mut recorded = 0;
    for chunk in events.chunks(MAX_BATCH_EVENTS) {
        recorded += store.add_events(chunk.to_vec(), &res).await.unwrap();
    }
    assert_eq!(recorded, 12_500);
    let report = store
        .compact(&february(), &RetentionPolicy::default(), &res)
        .await
        .unwrap();
    assert_eq!(report.events_dropped, 12_500);
    assert_eq!(
        store
            .period_totals("acme", &january(), &res)
            .await
            .unwrap()
            .total(&Meter::Transactions),
        12_500
    );
}

/// The ground truth of a delivery: facts folded directly, by different code from the store.
struct Facts {
    events: Vec<MeterEvent>,
}

impl Facts {
    fn generate(rng: &mut Rng) -> Self {
        let mut events = Vec::new();
        let tenants = ["acme", "globex", "initech"];
        for i in 0..360usize {
            let tenant = tenants[rng.below(3)];
            let at = if rng.below(2) == 0 {
                10 + rng.below(1000) as i64
            } else {
                FEBRUARY + rng.below(1000) as i64
            };
            let event = match rng.below(4) {
                0 => MeterEvent::amount(
                    format!("t{i}"),
                    tenant,
                    Meter::Transactions,
                    1 + rng.below(50) as u64,
                    at,
                ),
                1 => MeterEvent::amount(
                    format!("a{i}"),
                    tenant,
                    Meter::AiTokens,
                    rng.below(5000) as u64,
                    at,
                ),
                2 => MeterEvent::amount(
                    format!("s{i}"),
                    tenant,
                    Meter::StorageBytes,
                    rng.below(100_000) as u64,
                    at,
                ),
                _ => MeterEvent::member(
                    format!("u{i}"),
                    tenant,
                    Meter::ActiveUsers,
                    MemberDigest::new(format!("user{}", rng.below(40))),
                    at,
                ),
            };
            events.push(event);
        }
        Self { events }
    }

    fn truth(&self) -> TallyTruth {
        let mut sums: BTreeMap<(String, i64, Meter), u64> = BTreeMap::new();
        let mut members: BTreeMap<(String, i64, Meter), Vec<MemberDigest>> = BTreeMap::new();
        for event in &self.events {
            let start = window(event.at).start;
            let key = (event.tenant.clone(), start, event.meter.clone());
            match &event.member {
                Some(member) => {
                    let set = members.entry(key).or_default();
                    if !set.contains(member) {
                        set.push(member.clone());
                    }
                }
                None if event.meter == Meter::StorageBytes => {
                    let max = sums.entry(key).or_default();
                    *max = (*max).max(event.amount);
                }
                None => *sums.entry(key).or_default() += event.amount,
            }
        }
        let mut truth = TallyTruth::new();
        for ((tenant, start, meter), total) in sums {
            truth = truth.with_total(tenant, &window(start), meter, total);
        }
        for ((tenant, start, meter), set) in members {
            truth = truth.with_members(tenant, &window(start), meter, set);
        }
        truth
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconciliation_lands_exactly_on_the_truth_after_a_messy_delivery() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let res = resolver();
    let tenants = ["acme", "globex", "initech"];
    let mut stores: BTreeMap<&str, Arc<PgMeterStore>> = BTreeMap::new();
    for tenant in tenants {
        stores.insert(tenant, Arc::new(store(&db, 8, tenant).await));
    }
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    let facts = Facts::generate(&mut rng);
    let truth = facts.truth();

    // Delivery, per tenant: duplicated (a third of the facts twice), shuffled, cut into batches,
    // with some batches dropped on the first pass and retried afterwards.
    for tenant in tenants {
        let store = Arc::clone(&stores[tenant]);
        let mine: Vec<MeterEvent> = facts
            .events
            .iter()
            .filter(|event| event.tenant == tenant)
            .cloned()
            .collect();
        let mut delivery = mine.clone();
        for event in &mine {
            if rng.below(3) == 0 {
                delivery.push(event.clone());
            }
        }
        rng.shuffle(&mut delivery);
        let batches: Vec<Vec<MeterEvent>> =
            delivery.chunks(37).map(<[MeterEvent]>::to_vec).collect();
        let mut dropped = Vec::new();
        for (index, batch) in batches.iter().enumerate() {
            if index % 4 == 1 {
                dropped.push(batch.clone());
            } else {
                store.add_events(batch.clone(), &res).await.unwrap();
            }
        }
        // Mid-way the store is behind the truth: reconcile says so and counts the dropped batches.
        let report = reconcile(
            store.as_ref(),
            &truth,
            tenant,
            &january(),
            &res,
            dropped.len() as u64,
        )
        .await
        .unwrap();
        assert_eq!(report.dropped, dropped.len() as u64);
        assert!(!report.is_clean() || dropped.is_empty());
        // The retries (and a second, redundant pass) bring it to the truth.
        for batch in dropped.iter().chain(batches.iter()) {
            store.add_events(batch.clone(), &res).await.unwrap();
        }
    }
    for tenant in tenants {
        for period in [january(), february()] {
            let report = reconcile(stores[tenant].as_ref(), &truth, tenant, &period, &res, 0)
                .await
                .unwrap();
            assert!(
                report.is_clean(),
                "{tenant} {}: {:?}",
                period.label,
                report.disagreements()
            );
            assert!(
                !report.rows.is_empty(),
                "{tenant} {} had facts",
                period.label
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn totals_match_the_in_memory_store_across_a_random_history() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let res = resolver();
    let tenants = ["acme", "globex"];
    let mut pg: BTreeMap<&str, PgMeterStore> = BTreeMap::new();
    let mut memory: BTreeMap<&str, MemoryMeterStore> = BTreeMap::new();
    for tenant in tenants {
        pg.insert(tenant, store(&db, 8, tenant).await);
        memory.insert(tenant, MemoryMeterStore::new(tenant));
    }
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let mut next_id = 0usize;
    let mut pool: Vec<MeterEvent> = Vec::new();
    for round in 0..40 {
        let mut batch = Vec::new();
        for _ in 0..rng.below(30) {
            // Sometimes reuse an earlier event (a retry), sometimes make a new one.
            if !pool.is_empty() && rng.below(3) == 0 {
                batch.push(pool[rng.below(pool.len())].clone());
                continue;
            }
            next_id += 1;
            let tenant = ["acme", "globex"][rng.below(2)];
            let at = [0, 500, FEBRUARY, FEBRUARY + 9][rng.below(4)];
            let id = format!("r{next_id}");
            let event = match rng.below(5) {
                0 => {
                    MeterEvent::amount(id, tenant, Meter::Transactions, rng.below(1000) as u64, at)
                }
                1 => MeterEvent::amount(
                    id,
                    tenant,
                    Meter::Custom("transactions".into()),
                    rng.below(1000) as u64,
                    at,
                ),
                2 => MeterEvent::amount(
                    id,
                    tenant,
                    Meter::Custom("api_calls".into()),
                    rng.below(1000) as u64,
                    at,
                ),
                3 => {
                    MeterEvent::amount(id, tenant, Meter::StorageBytes, rng.below(1000) as u64, at)
                }
                _ => MeterEvent::member(
                    id,
                    tenant,
                    Meter::ActiveUsers,
                    MemberDigest::new(format!("d{}", rng.below(25))),
                    at,
                ),
            };
            pool.push(event.clone());
            batch.push(event);
        }
        rng.shuffle(&mut batch);
        // One batch may hold both tenants' events, so it is split the way a caller would: one
        // call per store, since a store refuses another tenant's event.
        for tenant in tenants {
            let mine: Vec<MeterEvent> = batch
                .iter()
                .filter(|event| event.tenant == tenant)
                .cloned()
                .collect();
            let a = pg[tenant].add_events(mine.clone(), &res).await.unwrap();
            let b = memory[tenant].add_events(mine, &res).await.unwrap();
            assert_eq!(a, b, "round {round}: newly recorded count for {tenant}");
        }
        if round % 13 == 12 {
            for tenant in tenants {
                let a = pg[tenant]
                    .compact(&february(), &RetentionPolicy::default(), &res)
                    .await
                    .unwrap();
                let b = memory[tenant]
                    .compact(&february(), &RetentionPolicy::default(), &res)
                    .await
                    .unwrap();
                assert_eq!(a, b, "round {round}: compacted report for {tenant}");
            }
        }
    }
    // A tenant nothing was ever recorded for reads the same from both stores.
    let empty_pg = store(&db, 4, "nobody").await;
    let empty_memory = MemoryMeterStore::new("nobody");
    for tenant in ["acme", "globex"] {
        for period in [january(), february()] {
            let a = pg[tenant]
                .period_totals(tenant, &period, &res)
                .await
                .unwrap();
            let b = memory[tenant]
                .period_totals(tenant, &period, &res)
                .await
                .unwrap();
            assert_eq!(a, b, "{tenant} {}", period.label);
            for meter in [Meter::ActiveUsers, Meter::Transactions] {
                assert_eq!(
                    pg[tenant]
                        .distinct_members(tenant, &period, &meter, &res)
                        .await
                        .unwrap(),
                    memory[tenant]
                        .distinct_members(tenant, &period, &meter, &res)
                        .await
                        .unwrap(),
                );
            }
        }
    }
    for period in [january(), february()] {
        assert_eq!(
            empty_pg
                .period_totals("nobody", &period, &res)
                .await
                .unwrap(),
            empty_memory
                .period_totals("nobody", &period, &res)
                .await
                .unwrap()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tenants_share_neither_ids_nor_members_nor_totals() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let acme = store(&db, 4, "acme").await;
    let globex = common::store(&db, 4, "globex").await;
    let initech = store(&db, 4, "initech").await;
    let res = resolver();
    let member = |tenant: &str| {
        MeterEvent::member(
            "same-id",
            tenant,
            Meter::ActiveUsers,
            MemberDigest::new("same"),
            0,
        )
    };
    assert_eq!(
        acme.add_events(vec![member("acme")], &res).await.unwrap(),
        1
    );
    assert_eq!(
        globex
            .add_events(vec![member("globex")], &res)
            .await
            .unwrap(),
        1
    );
    for (store, tenant) in [(&acme, "acme"), (&globex, "globex")] {
        let usage = store.period_totals(tenant, &january(), &res).await.unwrap();
        assert_eq!(usage.total(&Meter::ActiveUsers), 1);
        assert_eq!(
            usage.members_of(&Meter::ActiveUsers),
            vec![MemberDigest::new("same")]
        );
    }
    assert!(
        initech
            .period_totals("initech", &january(), &res)
            .await
            .unwrap()
            .by_meter
            .is_empty()
    );
    assert!(
        initech
            .distinct_members("initech", &january(), &Meter::ActiveUsers, &res)
            .await
            .unwrap()
            .is_empty()
    );
    // No store answers for a tenant it is not bound to.
    assert_eq!(
        acme.period_totals("globex", &january(), &res)
            .await
            .unwrap_err(),
        MeteringError::TenantMismatch
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ids_and_digests_are_data_not_sql() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let evil = "x'; DROP TABLE spot_meter_event; --";
    let store = store(&db, 4, evil).await;
    let res = resolver();
    let events = vec![
        tx(evil, evil, 5, 0),
        MeterEvent::member(
            "m'1",
            evil,
            Meter::ActiveUsers,
            MemberDigest::new("d'; --"),
            0,
        ),
        MeterEvent::amount("c1", evil, Meter::Custom("a'b".into()), 2, 0),
    ];
    assert_eq!(store.add_events(events.clone(), &res).await.unwrap(), 3);
    assert_eq!(store.add_events(events, &res).await.unwrap(), 0);
    let usage = store.period_totals(evil, &january(), &res).await.unwrap();
    assert_eq!(usage.total(&Meter::Transactions), 5);
    assert_eq!(usage.total(&Meter::Custom("a'b".into())), 2);
    assert_eq!(
        usage.members_of(&Meter::ActiveUsers),
        vec![MemberDigest::new("d'; --")]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bigger_backlog_is_several_bounded_calls() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, 4, "acme").await;
    let res = resolver();
    let events: Vec<MeterEvent> = (0..12_000u64)
        .map(|i| tx(&format!("big{i}"), "acme", 1, 0))
        .collect();
    // One call cannot carry the whole backlog, and the refusal writes nothing.
    let error = store.add_events(events.clone(), &res).await.unwrap_err();
    assert_eq!(
        error,
        MeteringError::BatchTooLarge {
            size: 12_000,
            max: MAX_BATCH_EVENTS,
        }
    );
    assert_eq!(event_rows(&db, "true").await, 0);
    // In bounded calls it is still several statements in one transaction each, and the totals are
    // identical whether or not a batch is repeated.
    let mut recorded = 0;
    for chunk in events.chunks(6_000) {
        recorded += store.add_events(chunk.to_vec(), &res).await.unwrap();
    }
    assert_eq!(recorded, 12_000);
    assert_eq!(
        store
            .add_events(events[..6_000].to_vec(), &res)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        store
            .period_totals("acme", &january(), &res)
            .await
            .unwrap()
            .total(&Meter::Transactions),
        12_000
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent_and_safe_to_run_concurrently() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = common::pool(&db, 8, "metering_prepare");
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let store = PgMeterStore::new(pool.clone(), "acme");
        tasks.push(tokio::spawn(async move { store.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    PgMeterStore::new(pool, "acme").prepare().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreachable_database_is_an_error_naming_no_secret() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some("spot_test_does_not_exist".to_owned());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    let broken = PgMeterStore::new(
        cfg.create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap(),
        "acme",
    );
    let res = resolver();
    let error = broken
        .add_events(vec![tx("e", "acme", 1, 0)], &res)
        .await
        .unwrap_err();
    let text = format!("{error} {error:?}");
    assert!(
        !text.contains(&c.password) && !text.contains("acme"),
        "{text}"
    );
    assert!(
        broken
            .period_totals("acme", &january(), &res)
            .await
            .is_err()
    );
    assert!(
        broken
            .compact(&february(), &RetentionPolicy::default(), &res)
            .await
            .is_err()
    );
    assert!(broken.prepare().await.is_err());
    // An empty batch needs no database.
    assert_eq!(broken.add_events(Vec::new(), &res).await.unwrap(), 0);
}
