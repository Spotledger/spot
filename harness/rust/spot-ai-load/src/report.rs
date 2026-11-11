//! The report of one run: what was measured, per phase, and what the budgets said.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use crate::stats::LatencySummary;

/// Counters of what happened to the agents' calls in one phase.
#[derive(Debug, Default)]
pub struct AgentStats {
    /// Calls admitted by `AiAdmission::admit`.
    pub admitted: AtomicU64,
    /// Calls that were refused with `busy` at least once and waited for a free slot.
    pub queued: AtomicU64,
    /// Every `busy` refusal (a waiting call asks again every retry pause).
    pub busy_refusals: AtomicU64,
    /// Admission refusals with `shed`: interactive load was high.
    pub shed: AtomicU64,
    /// Admission refusals with `rate_limited`.
    pub rate_limited: AtomicU64,
    /// Admission refusals with `circuit_open`.
    pub circuit_open: AtomicU64,
    /// Calls that ran to the end.
    pub completed: AtomicU64,
    /// Calls that failed for any other reason.
    pub failed: AtomicU64,
    /// Calls the caller gave up on (`Admission::cancel`).
    pub cancelled: AtomicU64,
    /// Calls that passed their deadline and whose statement was cancelled.
    pub timed_out: AtomicU64,
    /// Cancelled statements the server had not ended within the grace period.
    pub cancel_unconfirmed: AtomicU64,
}

/// A plain copy of [`AgentStats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct AgentCounters {
    /// Calls admitted.
    pub admitted: u64,
    /// Calls that queued: refused with `busy` at least once, then admitted or given up.
    pub queued: u64,
    /// Every busy refusal, retries included.
    pub busy_refusals: u64,
    /// Calls shed because interactive load was high.
    pub shed: u64,
    /// Rate-limit refusals.
    pub rate_limited: u64,
    /// Refusals by an open circuit breaker.
    pub circuit_open: u64,
    /// Calls that completed.
    pub completed: u64,
    /// Calls that failed.
    pub failed: u64,
    /// Calls the caller cancelled.
    pub cancelled: u64,
    /// Calls that timed out and were cancelled.
    pub timed_out: u64,
    /// Cancelled statements the server did not end in time.
    pub cancel_unconfirmed: u64,
}

impl AgentStats {
    /// Bumps one counter.
    pub fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// A copy of the counters.
    #[must_use]
    pub fn snapshot(&self) -> AgentCounters {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        AgentCounters {
            admitted: get(&self.admitted),
            queued: get(&self.queued),
            busy_refusals: get(&self.busy_refusals),
            shed: get(&self.shed),
            rate_limited: get(&self.rate_limited),
            circuit_open: get(&self.circuit_open),
            completed: get(&self.completed),
            failed: get(&self.failed),
            cancelled: get(&self.cancelled),
            timed_out: get(&self.timed_out),
            cancel_unconfirmed: get(&self.cancel_unconfirmed),
        }
    }
}

/// How many breakers were in each state at one sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BreakerSample {
    /// Milliseconds since the phase began.
    pub at_ms: u64,
    /// Breakers that admit calls.
    pub closed: u32,
    /// Breakers that refuse calls.
    pub open: u32,
    /// Breakers that admit probe calls.
    pub half_open: u32,
}

/// What one phase measured.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PhaseReport {
    /// `baseline`, `with_agents` or `overload`.
    pub name: String,
    /// How long the phase ran, in milliseconds.
    pub duration_ms: u64,
    /// How many agents ran.
    pub agents: u32,
    /// Human latency over all operations (pool wait included).
    pub human: LatencySummary,
    /// Human latency per operation (`read`, `list`, `report`).
    pub human_by_op: BTreeMap<String, LatencySummary>,
    /// Human operations that failed.
    pub human_errors: u64,
    /// Time the humans waited for a pool connection.
    pub human_pool_wait: LatencySummary,
    /// Time the agents waited for a pool connection.
    pub ai_pool_wait: LatencySummary,
    /// How long admitted-after-refusal calls waited for a slot.
    pub ai_queue_wait: LatencySummary,
    /// What happened to the agents' calls.
    pub agent_calls: AgentCounters,
    /// Backends running a statement for the `ai` role (client backends and the parallel workers
    /// they start), one sample an interval.
    pub ai_active_backends: Vec<u32>,
    /// The largest of those samples: the number the cap budget judges.
    pub ai_active_backends_max: u32,
    /// The largest number of those that were client backends (connections of the `ai` pool).
    pub ai_active_client_backends_max: u32,
    /// The largest number of those that were parallel workers started by an `ai` statement.
    pub ai_parallel_workers_max: u32,
    /// Client connections open for the `ai` role, one sample an interval.
    pub ai_open_backends: Vec<u32>,
    /// The AIMD admission limit, one sample a second.
    pub aimd_limit: Vec<u32>,
    /// Waiters on the `ai` pool, the largest seen in the samples.
    pub ai_pool_waiting_max: u32,
    /// Breaker states over time.
    pub breakers: Vec<BreakerSample>,
}

/// How AI admission recovered after the overload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RecoveryReport {
    /// The AIMD limit when the overload phase ended.
    pub aimd_limit_at_overload_end: u32,
    /// The configured level (the pool size).
    pub configured_limit: u32,
    /// Milliseconds until the limit was back at the configured level; `None` if it never was.
    pub recovered_after_ms: Option<u64>,
    /// How many probe admissions it took.
    pub probes: u64,
}

/// What was left behind when the run ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct LeakReport {
    /// Admission permits still held after the agents stopped.
    pub permits: u32,
    /// Pool connections still checked out after the agents stopped.
    pub connections: u32,
    /// Statements of the `ai` role still running after the five-second poll.
    pub running_after_cancel: u32,
    /// Whether the `ai` role could write (it must not).
    pub ai_write_allowed: bool,
    /// Whether the `ai` role or the seeded schema survived the clean-up.
    pub left_behind: bool,
}

/// One budget check that failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Breach {
    /// Which check.
    pub check: String,
    /// The limit, in the check's unit.
    pub limit: u64,
    /// The measured value, in the same unit.
    pub actual: u64,
}

/// The whole report.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Report {
    /// `short` or `full`.
    pub mode: String,
    /// Rows in the seeded table.
    pub rows: u64,
    /// Size of the human pool.
    pub human_pool_size: u32,
    /// Size of the `ai` pool.
    pub ai_pool_size: u32,
    /// The phases, in order.
    pub phases: Vec<PhaseReport>,
    /// The recovery after the overload.
    pub recovery: RecoveryReport,
    /// What was left behind.
    pub leaks: LeakReport,
    /// The budget checks that failed.
    pub breaches: Vec<Breach>,
    /// Whether every check passed.
    pub passed: bool,
}

impl Report {
    /// The phase named `name`.
    #[must_use]
    pub fn phase(&self, name: &str) -> Option<&PhaseReport> {
        self.phases.iter().find(|p| p.name == name)
    }

    /// The report as pretty JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }
}
