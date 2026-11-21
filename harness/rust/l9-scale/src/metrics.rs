//! What a run measures: per-operation outcome counts and two latency histograms (the
//! coordinated-omission-corrected latency and the plain service time), and throughput per
//! second.

use crate::hist::Histogram;
use crate::profile::{OPS, Op};

/// How one operation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// A 2xx or 3xx answer.
    Ok,
    /// A 4xx answer to an operation that is meant to be refused (a negative operation).
    ExpectedClient,
    /// A 4xx answer to an operation that should have worked.
    ClientError,
    /// A 5xx answer.
    ServerError,
    /// No answer within the request timeout.
    Timeout,
    /// The connection failed or the answer was not HTTP.
    Transport,
    /// The run ended before the queued operation could start.
    Abandoned,
}

impl Class {
    /// Whether this ending counts against the error budget: everything but a success and an
    /// expected refusal.
    pub fn is_failure(self) -> bool {
        !matches!(self, Class::Ok | Class::ExpectedClient)
    }
}

/// The ending of an operation and the last status it saw (0 when there was none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// How it ended.
    pub class: Class,
    /// The HTTP status of the last request, or 0.
    pub status: u16,
}

impl Outcome {
    /// A success.
    pub fn ok(status: u16) -> Self {
        Self {
            class: Class::Ok,
            status,
        }
    }

    /// An ending without a status.
    pub fn of(class: Class) -> Self {
        Self { class, status: 0 }
    }
}

/// Classifies an HTTP status. `refusal_expected` is true for a negative operation, whose 4xx
/// is the correct answer.
pub fn classify_status(status: u16, refusal_expected: bool) -> Outcome {
    let class = match status {
        200..=399 => Class::Ok,
        400..=499 if refusal_expected => Class::ExpectedClient,
        400..=499 => Class::ClientError,
        _ => Class::ServerError,
    };
    Outcome { class, status }
}

/// The counts and histograms of one operation.
#[derive(Debug, Clone, Default)]
pub struct OpStats {
    /// Latency from the intended start to the completion (microseconds).
    pub latency: Histogram,
    /// Time from the actual start to the completion (microseconds).
    pub service: Histogram,
    /// Successes.
    pub ok: u64,
    /// Refusals that were expected.
    pub expected_client: u64,
    /// Unexpected 4xx answers.
    pub client_errors: u64,
    /// 5xx answers.
    pub server_errors: u64,
    /// Timeouts.
    pub timeouts: u64,
    /// Connection failures.
    pub transport: u64,
    /// Operations dropped at the end of the run.
    pub abandoned: u64,
}

impl OpStats {
    /// Operations that finished one way or another.
    pub fn total(&self) -> u64 {
        self.ok
            + self.expected_client
            + self.client_errors
            + self.server_errors
            + self.timeouts
            + self.transport
            + self.abandoned
    }

    /// Operations that count against the error budget.
    pub fn failures(&self) -> u64 {
        self.client_errors + self.server_errors + self.timeouts + self.transport + self.abandoned
    }

    /// The failure rate in parts per million.
    pub fn error_rate_ppm(&self) -> u64 {
        let total = self.total();
        if total == 0 {
            0
        } else {
            u64::try_from(u128::from(self.failures()) * 1_000_000 / u128::from(total))
                .unwrap_or(u64::MAX)
        }
    }

    fn merge(&mut self, other: &OpStats) {
        self.latency.merge(&other.latency);
        self.service.merge(&other.service);
        self.ok += other.ok;
        self.expected_client += other.expected_client;
        self.client_errors += other.client_errors;
        self.server_errors += other.server_errors;
        self.timeouts += other.timeouts;
        self.transport += other.transport;
        self.abandoned += other.abandoned;
    }
}

/// Completions in one second of the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecondBucket {
    /// Operations that completed in this second.
    pub completed: u64,
    /// Of which failures.
    pub failed: u64,
}

/// The most seconds a timeline keeps; later seconds fold into the last bucket.
pub const MAX_TIMELINE: usize = 100_000;

/// Everything a run measured. Operations whose arrival fell in the ramp are only counted
/// (`ramp_completed`); the histograms and error counts are the steady phase's.
#[derive(Debug, Clone)]
pub struct Metrics {
    ops: Vec<OpStats>,
    /// Completions of ramp-phase arrivals (not budgeted).
    pub ramp_completed: u64,
    /// Failures among them.
    pub ramp_failed: u64,
    /// Completions per second of the run, ramp and steady phase together.
    pub timeline: Vec<SecondBucket>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// An empty set of metrics.
    pub fn new() -> Self {
        Self {
            ops: vec![OpStats::default(); OPS.len()],
            ramp_completed: 0,
            ramp_failed: 0,
            timeline: Vec::new(),
        }
    }

    /// Records one finished operation. `done_ns` places it on the timeline; the latencies are in
    /// nanoseconds.
    pub fn record(
        &mut self,
        op: Op,
        steady: bool,
        outcome: Outcome,
        latency_ns: u64,
        service_ns: u64,
        done_ns: u64,
    ) {
        let second = usize::try_from(done_ns / 1_000_000_000).unwrap_or(MAX_TIMELINE);
        let second = second.min(MAX_TIMELINE - 1);
        if self.timeline.len() <= second {
            self.timeline.resize(second + 1, SecondBucket::default());
        }
        if let Some(bucket) = self.timeline.get_mut(second) {
            bucket.completed += 1;
            if outcome.class.is_failure() {
                bucket.failed += 1;
            }
        }
        if !steady {
            self.ramp_completed += 1;
            if outcome.class.is_failure() {
                self.ramp_failed += 1;
            }
            return;
        }
        let Some(stats) = self.ops.get_mut(op.index()) else {
            return;
        };
        stats.latency.record(latency_ns / 1_000);
        stats.service.record(service_ns / 1_000);
        match outcome.class {
            Class::Ok => stats.ok += 1,
            Class::ExpectedClient => stats.expected_client += 1,
            Class::ClientError => stats.client_errors += 1,
            Class::ServerError => stats.server_errors += 1,
            Class::Timeout => stats.timeouts += 1,
            Class::Transport => stats.transport += 1,
            Class::Abandoned => stats.abandoned += 1,
        }
    }

    /// The stats of one operation.
    pub fn op(&self, op: Op) -> &OpStats {
        // `ops` always has one entry per operation.
        &self.ops[op.index().min(self.ops.len() - 1)]
    }

    /// All operations merged into one.
    pub fn all(&self) -> OpStats {
        let mut merged = OpStats::default();
        for stats in &self.ops {
            merged.merge(stats);
        }
        merged
    }

    /// Adds another worker's metrics.
    pub fn merge(&mut self, other: &Metrics) {
        for (mine, theirs) in self.ops.iter_mut().zip(&other.ops) {
            mine.merge(theirs);
        }
        self.ramp_completed += other.ramp_completed;
        self.ramp_failed += other.ramp_failed;
        if self.timeline.len() < other.timeline.len() {
            self.timeline
                .resize(other.timeline.len(), SecondBucket::default());
        }
        for (mine, theirs) in self.timeline.iter_mut().zip(&other.timeline) {
            mine.completed += theirs.completed;
            mine.failed += theirs.failed;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn statuses_are_classified_by_what_the_operation_expects() {
        assert_eq!(classify_status(200, false).class, Class::Ok);
        assert_eq!(classify_status(201, false).class, Class::Ok);
        assert_eq!(classify_status(409, true).class, Class::ExpectedClient);
        assert_eq!(classify_status(409, false).class, Class::ClientError);
        assert_eq!(classify_status(404, false).class, Class::ClientError);
        assert_eq!(classify_status(500, true).class, Class::ServerError);
        assert_eq!(classify_status(503, false).class, Class::ServerError);
        assert!(!Class::ExpectedClient.is_failure());
        assert!(Class::Timeout.is_failure() && Class::Abandoned.is_failure());
    }

    #[test]
    fn steady_results_are_counted_per_operation_and_ramp_results_only_tallied() {
        let mut m = Metrics::new();
        m.record(
            Op::List,
            true,
            Outcome::ok(200),
            5_000_000,
            4_000_000,
            1_500_000_000,
        );
        m.record(
            Op::List,
            true,
            Outcome::of(Class::Timeout),
            10_000_000_000,
            10_000_000_000,
            2_000_000_000,
        );
        m.record(Op::List, false, Outcome::ok(200), 1_000, 1_000, 100);
        m.record(
            Op::StaleUpdate,
            true,
            classify_status(409, true),
            1_000_000,
            900_000,
            3_100_000_000,
        );
        let list = m.op(Op::List);
        assert_eq!(
            (list.ok, list.timeouts, list.total(), list.failures()),
            (1, 1, 2, 1)
        );
        assert_eq!(list.error_rate_ppm(), 500_000);
        assert_eq!(list.latency.max(), 10_000_000);
        assert_eq!(m.op(Op::StaleUpdate).failures(), 0);
        assert_eq!(m.op(Op::StaleUpdate).expected_client, 1);
        assert_eq!((m.ramp_completed, m.ramp_failed), (1, 0));
        assert_eq!(m.all().total(), 3);
        assert_eq!(m.timeline.len(), 4);
        assert_eq!(
            m.timeline[1],
            SecondBucket {
                completed: 1,
                failed: 0
            }
        );
        assert_eq!(
            m.timeline[2],
            SecondBucket {
                completed: 1,
                failed: 1
            }
        );
    }

    #[test]
    fn merging_workers_adds_everything() {
        let mut a = Metrics::new();
        let mut b = Metrics::new();
        a.record(Op::SignIn, true, Outcome::ok(200), 1_000_000, 1_000_000, 0);
        b.record(
            Op::SignIn,
            true,
            Outcome::ok(200),
            3_000_000,
            3_000_000,
            5_000_000_000,
        );
        a.merge(&b);
        assert_eq!(a.op(Op::SignIn).ok, 2);
        assert_eq!(a.op(Op::SignIn).latency.max(), 3_000);
        assert_eq!(a.timeline.len(), 6);
    }

    #[test]
    fn an_empty_operation_has_a_zero_error_rate() {
        assert_eq!(Metrics::new().op(Op::Update).error_rate_ppm(), 0);
    }
}
