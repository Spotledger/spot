//! A firing the tick reaches on time runs under every missed-run policy (WP-PLT-07.T2).
//!
//! `skip` means "a run missed while nobody was ticking is not replayed", not "nothing runs": the
//! regression tests here tick a `skip` schedule through a day and see every firing delivered.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{clock_at, schedule, ts};
use jiff::Timestamp;
use spot_scheduler::policy::{plan_origins_with_grace, plan_with_grace};
use spot_scheduler::scheduler::sink;
use spot_scheduler::{
    DEFAULT_SKIP_GRACE, InMemoryHighWater, InMemoryLeaseStore, MissedRunPolicy, RunSlot, Scheduler,
    SchedulerConfig, SlotOrigin, SlotRef,
};

type Node = Scheduler<InMemoryLeaseStore, Arc<InMemoryHighWater>>;

fn node_for(cron: &str, policy: MissedRunPolicy) -> Node {
    let config = SchedulerConfig {
        site: "s".into(),
        node: "n1".into(),
        ttl: Duration::from_secs(30),
        schedules: vec![(schedule("sched", "UTC", cron), policy)],
        app: "billing".into(),
    };
    Scheduler::new(
        config,
        Arc::new(InMemoryLeaseStore::new()),
        Arc::new(InMemoryHighWater::new()),
    )
}

fn tick(n: &mut Node, clock: &spot_core::ManualClock) -> Vec<RunSlot> {
    let s = sink(|_| true);
    n.tick(clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"))
}

fn plan_at(
    policy: MissedRunPolicy,
    last: &str,
    now: &str,
    grace: Duration,
) -> Vec<(RunSlot, SlotOrigin)> {
    let s = schedule("hourly", "UTC", "0 0 * * * *");
    plan_origins_with_grace(
        &s,
        "billing",
        Some(ts(last)),
        ts(now),
        policy,
        SlotRef::new("billing", "hourly"),
        grace,
    )
}

fn instants(planned: &[(RunSlot, SlotOrigin)]) -> Vec<Timestamp> {
    planned.iter().map(|(s, _)| s.scheduled_for).collect()
}

#[test]
fn an_hourly_skip_schedule_ticked_every_30_seconds_fires_every_hour_for_26_hours() {
    let mut n = node_for("0 0 * * * *", MissedRunPolicy::Skip);
    let clock = clock_at(ts("2026-10-07T00:30:00Z"));
    let mut handed = tick(&mut n, &clock);
    for _ in 0..(26 * 120) {
        clock.advance(Duration::from_secs(30));
        handed.extend(tick(&mut n, &clock));
    }
    assert_eq!(handed.len(), 26, "one slot per hour firing, none dropped");
    let keys: BTreeSet<_> = handed.iter().map(|s| s.key().to_owned()).collect();
    assert_eq!(keys.len(), 26, "each firing exactly once, distinct keys");
    let first = ts("2026-10-07T01:00:00Z");
    for (hour, slot) in (0_i64..).zip(&handed) {
        assert_eq!(slot.scheduled_for, first + jiff::Span::new().hours(hour));
    }
    assert_eq!(handed[0].key(), "billing:sched:2026-10-07T01:00:00Z");
}

#[test]
fn on_time_slots_are_due_and_missed_ones_follow_the_policy() {
    // A node was silent from 00:30 to 03:30:10, so 01:00, 02:00 and 03:00 are all older than
    // the grace.
    let (last, now) = ("2026-10-07T00:30:00Z", "2026-10-07T03:30:10Z");
    let g = DEFAULT_SKIP_GRACE;
    assert!(plan_at(MissedRunPolicy::Skip, last, now, g).is_empty());

    let once = plan_at(MissedRunPolicy::RunOnce, last, now, g);
    assert_eq!(instants(&once), vec![ts("2026-10-07T03:00:00Z")]);
    assert_eq!(once[0].1, SlotOrigin::Missed);

    let catch = plan_at(MissedRunPolicy::CatchUp(2), last, now, g);
    assert_eq!(
        instants(&catch),
        vec![ts("2026-10-07T02:00:00Z"), ts("2026-10-07T03:00:00Z")]
    );
    assert_eq!(catch[0].1, SlotOrigin::Missed);
    assert_eq!(catch[1].1, SlotOrigin::CatchUp);

    // The tick that reaches 04:00 on time then runs it under every policy, as `Due`.
    for policy in [
        MissedRunPolicy::Skip,
        MissedRunPolicy::RunOnce,
        MissedRunPolicy::CatchUp(2),
    ] {
        let after = plan_at(policy, "2026-10-07T03:30:10Z", "2026-10-07T04:00:00Z", g);
        assert_eq!(instants(&after), vec![ts("2026-10-07T04:00:00Z")]);
        assert_eq!(after[0].1, SlotOrigin::Due);
    }
}

#[test]
fn a_node_silent_for_three_hours_then_ticking_follows_each_policy() {
    for (policy, expected) in [
        (MissedRunPolicy::Skip, vec![]),
        (MissedRunPolicy::RunOnce, vec!["2026-10-07T03:00:00Z"]),
        (
            MissedRunPolicy::CatchUp(2),
            vec!["2026-10-07T02:00:00Z", "2026-10-07T03:00:00Z"],
        ),
    ] {
        let mut n = node_for("0 0 * * * *", policy);
        let clock = clock_at(ts("2026-10-07T00:30:00Z"));
        assert!(tick(&mut n, &clock).is_empty());
        // Silent for three hours (the lease lapses too), then one tick at 03:30.
        clock.advance(Duration::from_secs(3 * 3600));
        let gap: Vec<_> = tick(&mut n, &clock)
            .iter()
            .map(|s| s.scheduled_for)
            .collect();
        let expected: Vec<_> = expected.into_iter().map(ts).collect();
        assert_eq!(gap, expected, "{policy:?}");
        // The next on-time firing runs under every policy.
        clock.advance(Duration::from_secs(30 * 60));
        let next = tick(&mut n, &clock);
        assert_eq!(next.len(), 1, "{policy:?}");
        assert_eq!(next[0].scheduled_for, ts("2026-10-07T04:00:00Z"));
    }
}

#[test]
fn the_grace_boundary_is_inclusive_and_the_grace_is_configurable() {
    let last = "2026-10-07T00:30:00Z";
    let g = DEFAULT_SKIP_GRACE;
    assert_eq!(g, Duration::from_secs(60));
    // 59 s after 01:00: on time. Exactly 60 s: still on time. 61 s: missed, `skip` drops it.
    for (now, fires) in [
        ("2026-10-07T01:00:59Z", true),
        ("2026-10-07T01:01:00Z", true),
        ("2026-10-07T01:01:01Z", false),
    ] {
        let planned = plan_at(MissedRunPolicy::Skip, last, now, g);
        assert_eq!(planned.len(), usize::from(fires), "now {now}");
        if fires {
            assert_eq!(planned[0].1, SlotOrigin::Due);
        }
    }
    // A five minute grace moves the boundary.
    let five = Duration::from_secs(300);
    assert_eq!(
        plan_at(MissedRunPolicy::Skip, last, "2026-10-07T01:05:00Z", five).len(),
        1
    );
    assert!(plan_at(MissedRunPolicy::Skip, last, "2026-10-07T01:05:01Z", five).is_empty());
    // `plan_with_grace` agrees with the origin-carrying variant.
    let s = schedule("hourly", "UTC", "0 0 * * * *");
    let slots = plan_with_grace(
        &s,
        "billing",
        Some(ts(last)),
        ts("2026-10-07T01:05:00Z"),
        MissedRunPolicy::Skip,
        SlotRef::new("billing", "hourly"),
        five,
    );
    assert_eq!(slots.len(), 1);
}

#[test]
fn with_skip_grace_sets_the_schedulers_boundary() {
    let late = |grace: Option<Duration>| {
        let mut n = node_for("0 0 * * * *", MissedRunPolicy::Skip);
        if let Some(g) = grace {
            n = n.with_skip_grace(g);
        }
        let clock = clock_at(ts("2026-10-07T00:30:00Z"));
        tick(&mut n, &clock);
        // The next tick comes 3 minutes after the 01:00 firing.
        clock.advance(Duration::from_secs(33 * 60));
        tick(&mut n, &clock).len()
    };
    assert_eq!(late(None), 0, "3 minutes late is missed under the default");
    assert_eq!(late(Some(Duration::from_secs(300))), 1);
}

#[test]
fn a_schedule_firing_every_second_ticked_every_ten_seconds_has_no_gaps() {
    let mut n = node_for("* * * * * *", MissedRunPolicy::Skip);
    let clock = clock_at(ts("2026-10-07T00:00:00Z"));
    tick(&mut n, &clock);
    let mut previous = ts("2026-10-07T00:00:00Z");
    for _ in 0..30 {
        clock.advance(Duration::from_secs(10));
        let handed = tick(&mut n, &clock);
        assert_eq!(handed.len(), 10);
        for slot in &handed {
            previous += jiff::Span::new().seconds(1);
            assert_eq!(
                slot.scheduled_for, previous,
                "in order, no gap across ticks"
            );
        }
    }
}

#[test]
fn a_refused_on_time_slot_keeps_the_mark_and_is_replanned_under_the_same_key() {
    let mut n = node_for("0 0 * * * *", MissedRunPolicy::Skip);
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
    n.tick(&clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"));
    // 01:00:10: the 01:00 slot is on time and refused, so nothing is handed over.
    clock.advance(Duration::from_secs(30 * 60 + 10));
    assert!(
        n.tick(&clock, s.as_ref())
            .unwrap_or_else(|e| panic!("{e}"))
            .is_empty()
    );
    if let Ok(mut g) = accept.lock() {
        *g = true;
    }
    // 01:00:20: still within the grace, the same slot is planned again and accepted.
    clock.advance(Duration::from_secs(10));
    let handed = n.tick(&clock, s.as_ref()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(handed.len(), 1);
    let keys = seen.lock().map(|v| v.clone()).unwrap_or_default();
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0], keys[1], "the retry carries the same key");
    assert_eq!(keys[0], "billing:sched:2026-10-07T01:00:00Z");
}

#[test]
fn run_once_and_catch_up_keep_the_on_time_slots_beside_the_missed_ones() {
    // 02:00 and 03:00 are missed, 04:00 is 10 s old: on time.
    let (last, now) = ("2026-10-07T01:30:00Z", "2026-10-07T04:00:10Z");
    let g = DEFAULT_SKIP_GRACE;
    let once = plan_at(MissedRunPolicy::RunOnce, last, now, g);
    assert_eq!(
        instants(&once),
        vec![ts("2026-10-07T03:00:00Z"), ts("2026-10-07T04:00:00Z")]
    );
    assert_eq!(
        (once[0].1, once[1].1),
        (SlotOrigin::Missed, SlotOrigin::Due)
    );
    // `max` bounds the missed ones, never the on-time one.
    let catch = plan_at(MissedRunPolicy::CatchUp(1), last, now, g);
    assert_eq!(
        instants(&catch),
        vec![ts("2026-10-07T03:00:00Z"), ts("2026-10-07T04:00:00Z")]
    );
    assert_eq!(catch[0].1, SlotOrigin::Missed);
    assert_eq!(catch[1].1, SlotOrigin::Due);
    assert_eq!(
        plan_at(MissedRunPolicy::Skip, last, now, g)
            .iter()
            .map(|(_, o)| *o)
            .collect::<Vec<_>>(),
        vec![SlotOrigin::Due]
    );
}
