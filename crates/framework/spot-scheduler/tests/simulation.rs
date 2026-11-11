//! The acceptance simulation: two or three scheduler nodes share a lease store and a mark
//! store, run a simulated day on fake clocks with random pauses, restarts and clock skew within
//! a stated bound, and the invariants hold on every seed:
//!
//! - no slot is handed over twice;
//! - no slot is missed under `catch_up` (the delivered set is exactly the firings), and under
//!   `run_once` the last firing is always delivered and nothing is delivered out of order;
//! - fencing tokens only increase.
//!
//! The failing seed is printed. Clock skew is at most `SKEW` seconds either way, a tick is
//! `STEP` seconds apart, and the lease ttl is `TTL`: `2 * SKEW + STEP < TTL`, so a node that
//! keeps renewing is never taken over by a node whose clock runs ahead.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{clock_at, schedule, ts};
use jiff::Timestamp;
use spot_core::ManualClock;
use spot_scheduler::scheduler::sink;
use spot_scheduler::{
    InMemoryHighWater, InMemoryLeaseStore, LeaseStore, MissedRunPolicy, Scheduler, SchedulerConfig,
    leader_lease_name,
};

const SEEDS: u64 = 200;
const STEP: u64 = 30;
const TTL: u64 = 90;
const SKEW: i64 = 15;
const STEPS_PER_DAY: u64 = 86_400 / STEP;
const SITE: &str = "main";

/// A small deterministic generator (xorshift64*), so a seed replays exactly.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, per_thousand: u64) -> bool {
        self.below(1000) < per_thousand
    }
}

type Node = Scheduler<InMemoryLeaseStore, Arc<InMemoryHighWater>>;

fn config(node: &str) -> SchedulerConfig {
    SchedulerConfig {
        site: SITE.into(),
        node: node.into(),
        ttl: Duration::from_secs(TTL),
        schedules: vec![
            (
                schedule("minutely", "UTC", "0 * * * * *"),
                MissedRunPolicy::CatchUp(5_000),
            ),
            (
                schedule("quarter", "America/New_York", "0 */15 * * * *"),
                MissedRunPolicy::RunOnce,
            ),
            (
                schedule("hourly", "Asia/Karachi", "0 0 * * * *"),
                MissedRunPolicy::CatchUp(100),
            ),
            (
                schedule("tenth", "UTC", "0 */10 * * * *"),
                MissedRunPolicy::Skip,
            ),
        ],
        app: "sim".into(),
    }
}

struct Delivered {
    /// key -> times handed over.
    counts: BTreeMap<String, u32>,
    /// schedule -> scheduled instants in delivery order.
    order: BTreeMap<String, Vec<Timestamp>>,
}

fn run_seed(seed: u64) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let nodes_n = 2 + usize::try_from(rng.below(2)).unwrap_or(0);
    let t0 = ts("2026-10-07T00:00:00Z");
    let leases = Arc::new(InMemoryLeaseStore::new());
    let marks = Arc::new(InMemoryHighWater::new());
    let delivered = Arc::new(Mutex::new(Delivered {
        counts: BTreeMap::new(),
        order: BTreeMap::new(),
    }));
    let d2 = delivered.clone();
    let s = sink(move |slot| {
        if let Ok(mut d) = d2.lock() {
            *d.counts.entry(slot.key().to_owned()).or_insert(0) += 1;
            d.order
                .entry(slot.schedule.clone())
                .or_default()
                .push(slot.scheduled_for);
        }
        true
    });

    let skews: Vec<i64> = (0..nodes_n)
        .map(|_| i64::try_from(rng.below(2 * SKEW.unsigned_abs() + 1)).unwrap_or(0) - SKEW)
        .collect();
    let clocks: Vec<ManualClock> = skews
        .iter()
        .map(|skew| clock_at(t0 + jiff::Span::new().seconds(*skew)))
        .collect();
    let mut nodes: Vec<Node> = (0..nodes_n)
        .map(|i| Scheduler::new(config(&format!("node{i}")), leases.clone(), marks.clone()))
        .collect();
    let mut paused_until = vec![0_u64; nodes_n];
    let mut node_token = vec![0_u64; nodes_n];
    let mut store_token = 0_u64;
    let mut first_now: Option<Timestamp> = None;
    let mut last_now: Option<Timestamp> = None;

    // A simulated day, then a quiet tail with every node alive so the last runs are handed over.
    let total = STEPS_PER_DAY + 8;
    for step in 0..total {
        let tail = step >= STEPS_PER_DAY;
        let mut order: Vec<usize> = (0..nodes_n).collect();
        for i in (1..order.len()).rev() {
            let j = usize::try_from(rng.below(u64::try_from(i + 1).unwrap_or(1))).unwrap_or(0);
            order.swap(i, j);
        }
        for &n in &order {
            if !tail {
                if rng.chance(4) {
                    // A pause: 1 to 20 steps (30 s to 10 min), longer than the ttl at times.
                    paused_until[n] = step + 1 + rng.below(20);
                }
                if rng.chance(3) {
                    nodes[n] =
                        Scheduler::new(config(&format!("node{n}")), leases.clone(), marks.clone());
                    node_token[n] = 0;
                }
            }
            if step < paused_until[n] && !tail {
                continue;
            }
            let now_clock = &clocks[n];
            match nodes[n].tick(now_clock, s.as_ref()) {
                Ok(_) => {
                    let now = spot_scheduler::scheduler::timestamp_of(now_clock);
                    first_now.get_or_insert(now);
                    last_now = Some(last_now.map_or(now, |l| l.max(now)));
                    let held = nodes[n]
                        .held_lease()
                        .map(|l| l.token.0)
                        .ok_or("an Ok tick left no lease")?;
                    if held < node_token[n] {
                        return Err(format!("node{n} token went {} -> {held}", node_token[n]));
                    }
                    node_token[n] = held;
                }
                Err(e) if e.code == "scheduler.lease_lost" => {}
                Err(e) => return Err(format!("node{n}: unexpected {e}")),
            }
            if let Some(cur) = leases.current(&leader_lease_name(SITE)) {
                if cur.token.0 < store_token {
                    return Err(format!("store token went {store_token} -> {}", cur.token.0));
                }
                store_token = cur.token.0;
            }
        }
        for clock in &clocks {
            clock.advance(Duration::from_secs(STEP));
        }
    }

    let (Some(first_now), Some(last_now)) = (first_now, last_now) else {
        return Err("no node ever ticked".into());
    };
    let d = delivered.lock().map_err(|_| "poisoned".to_owned())?;
    if let Some((key, n)) = d.counts.iter().find(|(_, n)| **n > 1) {
        return Err(format!("{key} handed over {n} times"));
    }
    let from = first_now - jiff::Span::new().nanoseconds(1);
    for sched in config("x").schedules {
        let (s, policy) = sched;
        let got = d.order.get(&s.name).cloned().unwrap_or_default();
        let expected = s.due_between(from, last_now, usize::MAX);
        if !got.windows(2).all(|w| w[0] < w[1]) {
            return Err(format!("{}: delivered out of order", s.name));
        }
        match policy {
            MissedRunPolicy::CatchUp(_) => {
                let got: BTreeSet<_> = got.iter().collect();
                let expected: BTreeSet<_> = expected.iter().collect();
                if got != expected {
                    let missing = expected.difference(&got).next();
                    let extra = got.difference(&expected).next();
                    return Err(format!(
                        "{}: missed {missing:?}, unexpected {extra:?} ({} delivered, {} due)",
                        s.name,
                        got.len(),
                        expected.len()
                    ));
                }
            }
            MissedRunPolicy::RunOnce => {
                if got.last() != expected.last() {
                    return Err(format!(
                        "{}: last delivered {:?}, last due {:?}",
                        s.name,
                        got.last(),
                        expected.last()
                    ));
                }
                if let Some(bad) = got.iter().find(|g| !expected.contains(g)) {
                    return Err(format!("{}: {bad} is not a firing", s.name));
                }
            }
            MissedRunPolicy::Skip => {
                if let Some(bad) = got.iter().find(|g| !expected.contains(g)) {
                    return Err(format!("{}: {bad} is not a firing", s.name));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn two_to_three_nodes_never_duplicate_or_miss_a_slot_over_200_seeds() {
    for seed in 0..SEEDS {
        if let Err(why) = run_seed(seed) {
            panic!("simulation failed on seed {seed}: {why}");
        }
    }
}
