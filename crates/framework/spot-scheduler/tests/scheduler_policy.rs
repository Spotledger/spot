//! Missed-run policies, slot keys, and the tick's mark rules.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{clock_at, schedule, ts};
use spot_scheduler::policy::plan;
use spot_scheduler::scheduler::sink;
use spot_scheduler::{
    InMemoryHighWater, InMemoryLeaseStore, MissedRunPolicy, RunSlot, Scheduler, SchedulerConfig,
    SlotRef,
};

fn slots(policy: MissedRunPolicy, last: &str, now: &str) -> Vec<RunSlot> {
    let s = schedule("hourly", "UTC", "0 0 * * * *");
    plan(
        &s,
        "billing",
        Some(ts(last)),
        ts(now),
        policy,
        SlotRef::new("billing", "hourly"),
    )
}

#[test]
fn skip_plans_nothing() {
    assert!(
        slots(
            MissedRunPolicy::Skip,
            "2026-10-07T00:00:00Z",
            "2026-10-07T05:30:00Z"
        )
        .is_empty()
    );
}

#[test]
fn run_once_collapses_the_missed_runs_into_the_last_one() {
    let planned = slots(
        MissedRunPolicy::RunOnce,
        "2026-10-07T00:00:00Z",
        "2026-10-07T05:30:00Z",
    );
    assert_eq!(planned.len(), 1);
    assert_eq!(planned[0].scheduled_for, ts("2026-10-07T05:00:00Z"));
}

#[test]
fn catch_up_runs_each_missed_firing_up_to_the_limit_keeping_the_latest() {
    let all = slots(
        MissedRunPolicy::CatchUp(10),
        "2026-10-07T00:00:00Z",
        "2026-10-07T05:30:00Z",
    );
    assert_eq!(all.len(), 5);
    let capped = slots(
        MissedRunPolicy::CatchUp(2),
        "2026-10-07T00:00:00Z",
        "2026-10-07T05:30:00Z",
    );
    let at: Vec<_> = capped.iter().map(|s| s.scheduled_for).collect();
    assert_eq!(
        at,
        vec![ts("2026-10-07T04:00:00Z"), ts("2026-10-07T05:00:00Z")]
    );
}

#[test]
fn nothing_missed_plans_nothing_under_every_policy() {
    for policy in [
        MissedRunPolicy::Skip,
        MissedRunPolicy::RunOnce,
        MissedRunPolicy::CatchUp(3),
    ] {
        assert!(slots(policy, "2026-10-07T05:00:00Z", "2026-10-07T05:59:59Z").is_empty());
    }
}

#[test]
fn the_deduplication_key_is_app_schedule_and_the_scheduled_instant() {
    let planned = slots(
        MissedRunPolicy::RunOnce,
        "2026-10-07T00:00:00Z",
        "2026-10-07T01:00:00Z",
    );
    assert_eq!(planned[0].key(), "billing:hourly:2026-10-07T01:00:00Z");
    // Re-planning the same window gives the same key, so a second enqueue is harmless.
    let again = slots(
        MissedRunPolicy::RunOnce,
        "2026-10-07T00:00:00Z",
        "2026-10-07T01:30:00Z",
    );
    assert_eq!(planned[0].key(), again[0].key());
}

fn node(
    policy: MissedRunPolicy,
    leases: Arc<InMemoryLeaseStore>,
    marks: Arc<InMemoryHighWater>,
) -> Scheduler<InMemoryLeaseStore, Arc<InMemoryHighWater>> {
    let config = SchedulerConfig {
        site: "s".into(),
        node: "n1".into(),
        ttl: Duration::from_secs(30),
        schedules: vec![(schedule("hourly", "UTC", "0 0 * * * *"), policy)],
        app: "billing".into(),
    };
    Scheduler::new(config, leases, marks)
}

#[test]
fn a_first_tick_has_no_history_and_the_next_firing_is_planned_when_reached() {
    let leases = Arc::new(InMemoryLeaseStore::new());
    let mut n = node(
        MissedRunPolicy::CatchUp(100),
        leases,
        Arc::new(InMemoryHighWater::new()),
    );
    let clock = clock_at(ts("2026-10-07T00:30:00Z"));
    let s = sink(|_| true);
    assert!(
        n.tick(&clock, s.as_ref())
            .unwrap_or_else(|e| panic!("{e}"))
            .is_empty()
    );
    clock.advance(Duration::from_secs(30 * 60));
    let handed = n.tick(&clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(handed.len(), 1);
    assert_eq!(handed[0].scheduled_for, ts("2026-10-07T01:00:00Z"));
}

#[test]
fn a_refused_slot_keeps_the_mark_and_is_replanned_under_the_same_key() {
    let leases = Arc::new(InMemoryLeaseStore::new());
    let mut n = node(
        MissedRunPolicy::CatchUp(100),
        leases,
        Arc::new(InMemoryHighWater::new()),
    );
    let clock = clock_at(ts("2026-10-07T00:30:00Z"));
    let accept = Arc::new(Mutex::new(false));
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let (a2, s2) = (accept.clone(), seen.clone());
    let s = sink(move |slot| {
        if let Ok(mut v) = s2.lock() {
            v.push(slot.key().to_owned());
        }
        a2.lock().map(|g| *g).unwrap_or(false)
    });
    assert!(
        n.tick(&clock, s.as_ref())
            .unwrap_or_else(|e| panic!("{e}"))
            .is_empty()
    );
    clock.advance(Duration::from_secs(40 * 60));
    // 01:10: the 01:00 slot is refused, so nothing is handed over and the mark stays.
    assert!(
        n.tick(&clock, s.as_ref())
            .unwrap_or_else(|e| panic!("{e}"))
            .is_empty()
    );
    if let Ok(mut g) = accept.lock() {
        *g = true;
    }
    clock.advance(Duration::from_secs(20));
    let handed = n.tick(&clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(handed.len(), 1);
    let keys = seen.lock().map(|v| v.clone()).unwrap_or_default();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1], "the retry carries the same key");
}

#[test]
fn a_lost_lease_abandons_the_tick() {
    let leases = Arc::new(InMemoryLeaseStore::new());
    let marks = Arc::new(InMemoryHighWater::new());
    let mut a = node(MissedRunPolicy::RunOnce, leases.clone(), marks.clone());
    let clock = clock_at(ts("2026-10-07T00:30:00Z"));
    let s = sink(|_| true);
    a.tick(&clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"));
    // `a` stays silent past the ttl and `b` takes over.
    clock.advance(Duration::from_secs(45));
    let mut b = node(MissedRunPolicy::RunOnce, leases, marks);
    b.tick(&clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"));
    let err = a.tick(&clock, s.as_ref()).err().map(|e| e.code);
    assert_eq!(err, Some("scheduler.lease_lost"));
}

fn short_schedule() -> Arc<spot_scheduler::Schedule> {
    let cron = spot_manifest::Cron::parse("0 0 * * * *").unwrap_or_else(|e| panic!("{e}"));
    let step = spot_app_api::StepName::new("billing:close").unwrap_or_else(|e| panic!("{e}"));
    let zone = spot_scheduler::TimeZone::parse("UTC").unwrap_or_else(|e| panic!("{e}"));
    Arc::new(
        spot_scheduler::Schedule::build(
            "hourly".into(),
            cron,
            step,
            spot_manifest::Queue::Short,
            Duration::from_millis(2_500),
            zone,
        )
        .unwrap_or_else(|e| panic!("{e}")),
    )
}

#[test]
fn a_new_slot_ref_carries_the_manifest_defaults() {
    let r = SlotRef::new("billing", "hourly");
    assert_eq!(r.queue, "default");
    assert_eq!(r.step, "");
    assert_eq!(r.budget_ms, 600_000);
}

#[test]
fn a_ticked_slot_carries_the_schedules_queue_step_and_budget_and_the_key_is_unchanged() {
    let config = SchedulerConfig {
        site: "s".into(),
        node: "n1".into(),
        ttl: Duration::from_secs(30),
        schedules: vec![(short_schedule(), MissedRunPolicy::RunOnce)],
        app: "billing".into(),
    };
    let mut n = Scheduler::new(
        config,
        Arc::new(InMemoryLeaseStore::new()),
        Arc::new(InMemoryHighWater::new()),
    );
    let clock = clock_at(ts("2026-10-07T00:30:00Z"));
    let s = sink(|_| true);
    n.tick(&clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"));
    clock.advance(Duration::from_secs(30 * 60));
    let handed = n.tick(&clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(handed.len(), 1);
    let r = &handed[0].slot_ref;
    assert_eq!(
        (r.queue.as_str(), r.step.as_str(), r.budget_ms),
        ("short", "billing:close", 2_500)
    );
    assert_eq!(handed[0].key(), "billing:hourly:2026-10-07T01:00:00Z");
}
