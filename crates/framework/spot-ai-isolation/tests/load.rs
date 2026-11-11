//! The load test (WP-AI-05.T1): twenty agents in parallel during a human-traffic load, against
//! a simulated database with a shared connection pool and virtual time.
//!
//! Acceptance: human latency stays within budget with AI isolation on, and **exceeds** it with
//! isolation off (the control), proving the test is meaningful. The simulation is fully
//! deterministic — integer microseconds, no clock, no threads — so a failure reproduces
//! exactly.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_ai_isolation::admission::{Admission, AiAdmission, AiRequest};
use spot_ai_isolation::config::{AiLimits, AiPoolSettings};
use spot_ai_isolation::identity::{AgentId, TenantId};
use spot_ai_isolation::load::StaticLoad;

/// How many agents run in parallel.
const AI_AGENTS: usize = 20;
/// How many heavy queries each agent issues.
const AI_QUERIES_PER_AGENT: u32 = 20;
/// How long one heavy AI query takes on the simulated database.
const AI_QUERY: Duration = Duration::from_millis(100);
/// The dedicated AI pool's size.
const AI_POOL: u32 = 4;

/// How many human queries arrive during the run.
const HUMAN_QUERIES: u32 = 40;
/// How often a human query arrives.
const HUMAN_PERIOD: Duration = Duration::from_millis(10);
/// How long one human query takes.
const HUMAN_QUERY: Duration = Duration::from_millis(5);
/// The human pool's size.
const HUMAN_POOL: u32 = 4;

/// The interactive p95 budget a human request must stay within.
const BUDGET: Duration = Duration::from_millis(20);

/// Which connection pool a running job holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ai,
    Human,
}

/// One query running on a simulated connection.
struct Running {
    finish: Duration,
    pool: Kind,
    pool_idx: usize,
    arrival: Option<Duration>,
    permit: Option<Admission>,
}

/// What one simulation run measured.
struct Stats {
    human_p95: Duration,
    human_max: Duration,
    ai_done: u32,
    ai_total: u32,
    max_ai_in_flight: u32,
}

/// Runs the workload. `shared` puts AI and human traffic on one pool (isolation off).
fn simulate(shared: bool) -> Stats {
    let pool_settings = AiPoolSettings {
        pool_size: AI_POOL,
        ..AiPoolSettings::default()
    };
    let gate = Arc::new(
        AiAdmission::new(
            AiLimits::default(),
            pool_settings,
            Arc::new(StaticLoad::calm()),
        )
        .expect("a valid configuration"),
    );
    let tenant = TenantId::new("acme").expect("a valid tenant");
    let agents: Vec<AiRequest> = (0..AI_AGENTS)
        .map(|i| {
            AiRequest::new(
                AgentId::new(format!("agent-{i}")).expect("a valid agent"),
                tenant.clone(),
            )
        })
        .collect();

    let mut remaining = [AI_QUERIES_PER_AGENT; AI_AGENTS];
    let mut ai_conns = vec![Duration::ZERO; AI_POOL as usize];
    let mut human_conns = vec![Duration::ZERO; HUMAN_POOL as usize];
    let mut running: Vec<Running> = Vec::new();
    let mut human_latencies: Vec<Duration> = Vec::new();
    let mut human_issued: u32 = 0;
    let mut ai_done: u32 = 0;
    let mut ai_in_flight: u32 = 0;
    let mut max_ai_in_flight: u32 = 0;
    let mut now = Duration::ZERO;

    loop {
        // 1. Complete everything that finished at or before `now`.
        let mut idx = 0;
        while idx < running.len() {
            if running[idx].finish <= now {
                let job = running.swap_remove(idx);
                match job.pool {
                    Kind::Ai => ai_conns[job.pool_idx] = job.finish,
                    Kind::Human => human_conns[job.pool_idx] = job.finish,
                }
                if let Some(arrival) = job.arrival {
                    human_latencies.push(job.finish.saturating_sub(arrival));
                } else {
                    ai_done += 1;
                    ai_in_flight = ai_in_flight.saturating_sub(1);
                }
                // Dropping the permit releases the call's concurrency.
                let _permit = job.permit;
            } else {
                idx += 1;
            }
        }

        // 2. Start every human query that has arrived, queueing on the human pool when busy.
        while human_issued < HUMAN_QUERIES {
            let arrival = HUMAN_PERIOD * human_issued;
            if arrival > now {
                break;
            }
            let (conn, start) = acquire(&mut human_conns, now).expect("the pool has a connection");
            let finish = start + HUMAN_QUERY;
            human_conns[conn] = finish;
            running.push(Running {
                finish,
                pool: Kind::Human,
                pool_idx: conn,
                arrival: Some(arrival),
                permit: None,
            });
            human_issued += 1;
        }

        // 3. Start AI queries while admission and a connection allow.
        for (agent, remaining) in remaining.iter_mut().enumerate() {
            if *remaining == 0 {
                continue;
            }
            if let Ok(permit) = gate.admit(&agents[agent], now) {
                let (conn, start) = if shared {
                    acquire(&mut human_conns, now).expect("the pool has a connection")
                } else {
                    acquire(&mut ai_conns, now).expect("the pool has a connection")
                };
                let finish = start + AI_QUERY;
                let pool = if shared { Kind::Human } else { Kind::Ai };
                match pool {
                    Kind::Ai => ai_conns[conn] = finish,
                    Kind::Human => human_conns[conn] = finish,
                }
                running.push(Running {
                    finish,
                    pool,
                    pool_idx: conn,
                    arrival: None,
                    permit: Some(permit),
                });
                *remaining -= 1;
                ai_in_flight += 1;
                max_ai_in_flight = max_ai_in_flight.max(ai_in_flight);
            }
        }

        // 4. Advance virtual time to the next event.
        let next_arrival = (human_issued < HUMAN_QUERIES).then(|| HUMAN_PERIOD * human_issued);
        let next_finish = running
            .iter()
            .map(|job| job.finish)
            .filter(|finish| *finish > now)
            .min();
        let next = [next_arrival, next_finish]
            .into_iter()
            .flatten()
            .filter(|t| *t > now)
            .min();
        match next {
            Some(t) => now = t,
            None => {
                if remaining.iter().all(|r| *r == 0)
                    && human_issued == HUMAN_QUERIES
                    && running.is_empty()
                {
                    break;
                }
                // Nothing to do but wait for a rate bucket to refill.
                now += Duration::from_millis(1);
            }
        }
    }

    human_latencies.sort_unstable();
    let n = human_latencies.len();
    let rank = (95 * n).div_ceil(100).max(1);
    Stats {
        human_p95: human_latencies[rank - 1],
        human_max: *human_latencies
            .last()
            .expect("at least one human query ran"),
        ai_done,
        ai_total: AI_AGENTS as u32 * AI_QUERIES_PER_AGENT,
        max_ai_in_flight,
    }
}

/// Picks the connection that frees first, returning its index and the instant it can start.
fn acquire(pool: &mut [Duration], now: Duration) -> Option<(usize, Duration)> {
    let mut best: Option<(usize, Duration)> = None;
    for (idx, free) in pool.iter().enumerate() {
        let start = (*free).max(now);
        if best.is_none_or(|(_, best_start)| start < best_start) {
            best = Some((idx, start));
        }
    }
    best
}

#[test]
fn twenty_agents_do_not_degrade_human_latency_when_isolated() {
    let isolated = simulate(false);
    let control = simulate(true);
    eprintln!(
        "isolated: human p95={:?} max={:?}, ai in flight<={}, ai done {}/{}; \
         control: human p95={:?} max={:?}",
        isolated.human_p95,
        isolated.human_max,
        isolated.max_ai_in_flight,
        isolated.ai_done,
        isolated.ai_total,
        control.human_p95,
        control.human_max,
    );
    assert_eq!(
        isolated.ai_done, isolated.ai_total,
        "every agent query must complete"
    );
    assert!(
        isolated.max_ai_in_flight <= AI_POOL,
        "AI never exceeds its dedicated pool: {} > {AI_POOL}",
        isolated.max_ai_in_flight
    );
    assert!(
        isolated.human_p95 <= BUDGET,
        "human p95 {:?} must stay within {BUDGET:?}",
        isolated.human_p95
    );

    // The control: with AI and human traffic sharing one pool, human latency blows the budget,
    // proving the assertion above is not vacuous.
    assert!(
        control.human_p95 > BUDGET,
        "the control must exceed the budget; p95 was {:?}",
        control.human_p95
    );
    assert!(
        isolated.human_p95 < control.human_p95,
        "isolation must beat the control: {:?} vs {:?}",
        isolated.human_p95,
        control.human_p95
    );
    assert!(
        control.human_max >= control.human_p95,
        "the max is at least the p95"
    );
}
