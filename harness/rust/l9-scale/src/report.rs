//! The report: `report.json` for machines and `report.md` for people. It holds latencies,
//! counts and server resource readings only: no credential, token, request body or user name
//! is ever put into it.

use serde::Serialize;

use crate::budget::{Verdict, fmt_milli, fmt_ms, fmt_ppm, throughput_milli};
use crate::metrics::{OpStats, SecondBucket};
use crate::profile::{OPS, Profile};
use crate::runner::{ResourceSample, RunResult};

/// The report's JSON layout version.
pub const SCHEMA: u32 = 1;

/// Percentiles of one histogram, in microseconds.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Latency {
    /// The median.
    pub p50: u64,
    /// The 95th percentile.
    pub p95: u64,
    /// The 99th percentile.
    pub p99: u64,
    /// The largest value.
    pub max: u64,
}

/// One operation's section.
#[derive(Debug, Clone, Serialize)]
pub struct OperationReport {
    /// The operation's name (`all` for the merged row).
    pub name: String,
    /// Operations that finished in the steady phase.
    pub count: u64,
    /// Successes.
    pub ok: u64,
    /// Refusals that were expected.
    pub expected_client_errors: u64,
    /// Unexpected 4xx answers.
    pub client_errors: u64,
    /// 5xx answers.
    pub server_errors: u64,
    /// Timeouts.
    pub timeouts: u64,
    /// Connection failures.
    pub transport_errors: u64,
    /// Operations dropped at the end of the run.
    pub abandoned: u64,
    /// Failures per million operations.
    pub error_rate_ppm: u64,
    /// Operations per second over the steady phase, in thousandths.
    pub throughput_milli: u64,
    /// Latency from the intended start (coordinated-omission corrected), microseconds.
    pub latency_us: Latency,
    /// Time from the actual start, microseconds.
    pub service_us: Latency,
}

/// The whole report.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The layout version.
    pub schema: u32,
    /// The run's id.
    pub run_id: String,
    /// `launched` (this harness started the server) or `attached`.
    pub target: String,
    /// The profile's name.
    pub profile: String,
    /// The profile's tier.
    pub tier: String,
    /// Simulated users.
    pub sessions: u32,
    /// Ramp length in seconds.
    pub ramp_seconds: u64,
    /// Steady phase in seconds.
    pub steady_seconds: u64,
    /// The planned rate in thousandths of an operation per second.
    pub target_milli_tps: u64,
    /// The schedule's seed.
    pub seed: u64,
    /// Arrivals dispatched.
    pub dispatched: u64,
    /// Arrivals dropped because every session was thinking.
    pub shed_arrivals: u64,
    /// Operations of the ramp (not budgeted).
    pub ramp_completed: u64,
    /// Seconds from the first arrival to the last completion.
    pub elapsed_seconds: u64,
    /// The merged row first, then one per operation that ran.
    pub operations: Vec<OperationReport>,
    /// Completions per second of the run.
    pub timeline: Vec<TimelinePoint>,
    /// Server resource readings.
    pub resources: Vec<ResourceSample>,
    /// Each budget limit and what the run measured.
    pub budgets: Vec<Verdict>,
    /// Whether every budget passed.
    pub passed: bool,
}

/// One second of the timeline.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TimelinePoint {
    /// Seconds into the run.
    pub second: u64,
    /// Completions.
    pub completed: u64,
    /// Failures among them.
    pub failed: u64,
}

fn latency(h: &crate::hist::Histogram) -> Latency {
    Latency {
        p50: h.percentile(500),
        p95: h.percentile(950),
        p99: h.percentile(990),
        max: h.max(),
    }
}

fn section(name: &str, stats: &OpStats, steady_seconds: u64) -> OperationReport {
    OperationReport {
        name: name.to_owned(),
        count: stats.total(),
        ok: stats.ok,
        expected_client_errors: stats.expected_client,
        client_errors: stats.client_errors,
        server_errors: stats.server_errors,
        timeouts: stats.timeouts,
        transport_errors: stats.transport,
        abandoned: stats.abandoned,
        error_rate_ppm: stats.error_rate_ppm(),
        throughput_milli: throughput_milli(stats, steady_seconds),
        latency_us: latency(&stats.latency),
        service_us: latency(&stats.service),
    }
}

impl Report {
    /// Builds the report of a run.
    pub fn build(
        run_id: &str,
        target: &str,
        profile: &Profile,
        result: &RunResult,
        verdicts: Vec<Verdict>,
    ) -> Report {
        let mut operations = vec![section(
            "all",
            &result.metrics.all(),
            profile.steady_seconds,
        )];
        for op in OPS {
            let stats = result.metrics.op(op);
            if stats.total() > 0 {
                operations.push(section(op.name(), stats, profile.steady_seconds));
            }
        }
        let timeline = result
            .metrics
            .timeline
            .iter()
            .enumerate()
            .map(
                |(second, SecondBucket { completed, failed })| TimelinePoint {
                    second: second as u64,
                    completed: *completed,
                    failed: *failed,
                },
            )
            .collect();
        let passed = verdicts.iter().all(|v| v.passed);
        Report {
            schema: SCHEMA,
            run_id: run_id.to_owned(),
            target: target.to_owned(),
            profile: profile.name.clone(),
            tier: profile.tier.clone(),
            sessions: profile.sessions,
            ramp_seconds: profile.ramp_seconds,
            steady_seconds: profile.steady_seconds,
            target_milli_tps: profile.target_milli_tps(),
            seed: profile.seed,
            dispatched: result.dispatched,
            shed_arrivals: result.shed,
            ramp_completed: result.metrics.ramp_completed,
            elapsed_seconds: result.elapsed_ns / 1_000_000_000,
            operations,
            timeline,
            resources: result.resources.clone(),
            budgets: verdicts,
            passed,
        }
    }

    /// The JSON document.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_owned())
    }

    /// The Markdown document.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        let verdict = if self.passed { "PASS" } else { "FAIL" };
        out.push_str(&format!(
            "# L9 scale report: {} ({verdict})\n\n",
            self.profile
        ));
        out.push_str(&format!(
            "- Run: `{}`, target: {}\n- Tier: {}, sessions: {}, ramp {} s, steady {} s, planned {} ops/s, seed {}\n\
             - Dispatched {} operations, shed {} (every session was thinking), {} in the ramp (not budgeted)\n\
             - Elapsed {} s. Latency is measured from each operation's intended start \
             (coordinated-omission corrected); service time from its actual start.\n\n",
            self.run_id,
            self.target,
            self.tier,
            self.sessions,
            self.ramp_seconds,
            self.steady_seconds,
            fmt_milli(self.target_milli_tps),
            self.seed,
            self.dispatched,
            self.shed_arrivals,
            self.ramp_completed,
            self.elapsed_seconds,
        ));
        out.push_str("## Operations (steady phase)\n\n");
        out.push_str("| Operation | Count | ok | refused (expected) | 4xx | 5xx | timeout | transport | abandoned | error rate | ops/s | p50 | p95 | p99 | max | service p95 |\n");
        out.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
        for op in &self.operations {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                op.name,
                op.count,
                op.ok,
                op.expected_client_errors,
                op.client_errors,
                op.server_errors,
                op.timeouts,
                op.transport_errors,
                op.abandoned,
                fmt_ppm(op.error_rate_ppm),
                fmt_milli(op.throughput_milli),
                fmt_ms(op.latency_us.p50),
                fmt_ms(op.latency_us.p95),
                fmt_ms(op.latency_us.p99),
                fmt_ms(op.latency_us.max),
                fmt_ms(op.service_us.p95),
            ));
        }
        out.push_str("\n## Budgets\n\n| Operation | Metric | Limit | Measured | Result |\n| --- | --- | ---: | ---: | --- |\n");
        for v in &self.budgets {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                v.operation,
                v.metric,
                v.limit,
                v.measured,
                if v.passed { "pass" } else { "FAIL" }
            ));
        }
        if !self.resources.is_empty() {
            out.push_str("\n## Server resources\n\n| Second | RSS (KB) | Open FDs | PG connections | PG waiting locks |\n| ---: | ---: | ---: | ---: | ---: |\n");
            for r in &self.resources {
                let show = |v: Option<u64>| v.map_or("n/a".to_owned(), |n| n.to_string());
                out.push_str(&format!(
                    "| {} | {} | {} | {} | {} |\n",
                    r.at_seconds,
                    show(r.rss_kb),
                    show(r.open_fds),
                    show(r.pg_connections),
                    show(r.pg_waiting_locks)
                ));
            }
        }
        if !self.timeline.is_empty() {
            out.push_str("\n## Throughput per second\n\n| Second | Completed | Failed |\n| ---: | ---: | ---: |\n");
            for p in &self.timeline {
                out.push_str(&format!(
                    "| {} | {} | {} |\n",
                    p.second, p.completed, p.failed
                ));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::budget::Budgets;
    use crate::metrics::{Class, Metrics, Outcome, classify_status};
    use crate::profile::Op;

    fn profile() -> Profile {
        Profile::from_toml(
            "name = \"unit\"\ntier = \"S\"\nsessions = 3\nsteady_seconds = 10\ntarget_tps = 4\n\
             [think]\nkind = \"fixed\"\nms = 10\n[mix]\nlist = 1\nopen_form = 1\n",
        )
        .unwrap()
    }

    fn result() -> RunResult {
        let mut m = Metrics::new();
        for i in 0..20u64 {
            m.record(
                Op::List,
                true,
                classify_status(200, false),
                (5 + i) * 1_000_000,
                4_000_000,
                i * 400_000_000,
            );
        }
        m.record(
            Op::OpenForm,
            true,
            Outcome::of(Class::Timeout),
            10_000_000_000,
            10_000_000_000,
            9_000_000_000,
        );
        RunResult {
            metrics: m,
            shed: 2,
            dispatched: 21,
            resources: vec![ResourceSample {
                at_seconds: 5,
                rss_kb: Some(50_000),
                open_fds: Some(40),
                pg_connections: None,
                pg_waiting_locks: Some(0),
            }],
            elapsed_ns: 12_000_000_000,
        }
    }

    fn report() -> Report {
        let budgets = Budgets::from_toml(
            "[[budget]]\nprofile=\"unit\"\noperation=\"list\"\np95_ms=100\n[[budget]]\nprofile=\"unit\"\noperation=\"open_form\"\nmax_error_rate=0.1\n",
        )
        .unwrap();
        let p = profile();
        let r = result();
        let verdicts = budgets
            .evaluate("unit", &r.metrics, p.steady_seconds)
            .unwrap();
        Report::build("run1", "launched", &p, &r, verdicts)
    }

    #[test]
    fn the_json_names_the_operations_the_budgets_and_the_verdict() {
        let report = report();
        let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
        assert_eq!(json["schema"], 1);
        assert_eq!(json["profile"], "unit");
        assert_eq!(json["passed"], false);
        assert_eq!(json["operations"][0]["name"], "all");
        assert_eq!(json["operations"][0]["count"], 21);
        assert_eq!(json["operations"][1]["name"], "open_form");
        let list = json["operations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["name"] == "list")
            .unwrap();
        assert_eq!(list["latency_us"]["max"], 24_000);
        assert_eq!(list["throughput_milli"], 2_000);
        assert_eq!(json["shed_arrivals"], 2);
        assert_eq!(json["resources"][0]["rss_kb"], 50_000);
        assert!(json["resources"][0]["pg_connections"].is_null());
        let failed: Vec<&serde_json::Value> = json["budgets"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["passed"] == false)
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0]["operation"], "open_form");
    }

    #[test]
    fn the_markdown_shows_the_failing_budget_and_its_measured_value() {
        let text = report().to_markdown();
        assert!(text.starts_with("# L9 scale report: unit (FAIL)"));
        assert!(
            text.contains("| open_form | error_rate | 10.0000 % | 100.0000 % | FAIL |"),
            "{text}"
        );
        assert!(text.contains("| list | p95_ms | 100.000 ms |"));
        assert!(text.contains("## Server resources"));
        assert!(text.contains("| 5 | 50000 | 40 | n/a | 0 |"));
        assert!(text.contains("coordinated-omission corrected"));
    }

    #[test]
    fn a_report_carries_no_credential_field() {
        let json = report().to_json().to_lowercase();
        for word in ["password", "token", "secret", "authorization", "bearer"] {
            assert!(!json.contains(word), "the report mentions {word}");
        }
    }
}
