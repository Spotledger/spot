//! The budgets and the pure function that judges a report against them.

use serde::Deserialize;

use crate::error::{HarnessError, Result};
use crate::report::{Breach, Report};

/// The default budgets file, compiled in.
pub const DEFAULT_BUDGETS: &str = include_str!("../budgets.toml");

/// Human-latency budgets.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct HumanBudget {
    /// p95 with agents may be at most baseline p95 times this, in permille.
    pub p95_ratio_permille: u64,
    /// ...plus this many milliseconds.
    pub p95_slack_ms: u64,
    /// p99 with agents may be at most baseline p99 times this, in permille.
    pub p99_ratio_permille: u64,
    /// ...plus this many milliseconds.
    pub p99_slack_ms: u64,
    /// Human operations that may fail.
    pub max_errors: u64,
}

/// AI-side budgets.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct AiBudget {
    /// Active `ai` backends allowed above the pool size.
    pub max_backends_over_cap: u32,
    /// Connections that may be left checked out.
    pub max_leaked_connections: u32,
    /// Permits that may be left held.
    pub max_leaked_permits: u32,
    /// Statements that may still run after cancellation.
    pub max_running_after_cancel: u32,
    /// How long admission may take to recover after the overload, in milliseconds.
    pub recovery_within_ms: u64,
}

/// Every budget.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub struct Budgets {
    /// Human-latency budgets.
    pub human: HumanBudget,
    /// AI-side budgets.
    pub ai: AiBudget,
}

impl Budgets {
    /// The compiled-in defaults.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Config`] when the compiled-in file does not parse (caught by a test).
    pub fn defaults() -> Result<Self> {
        Self::from_toml(DEFAULT_BUDGETS)
    }

    /// Parses a budgets file.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Config`] when the text is not a valid budgets file.
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| HarnessError::Config(format!("budgets: {e}")))
    }
}

/// Judges `report` against `budgets`: every check that failed, in a fixed order.
#[must_use]
pub fn evaluate(report: &Report, budgets: &Budgets) -> Vec<Breach> {
    let mut out = Vec::new();
    let baseline = report.phase("baseline").map(|p| p.human);
    for phase in &report.phases {
        if phase.human_errors > budgets.human.max_errors {
            out.push(breach(
                &format!("{}: human errors", phase.name),
                budgets.human.max_errors,
                phase.human_errors,
            ));
        }
        if phase.name == "baseline" {
            continue;
        }
        let cap = u64::from(report.ai_pool_size) + u64::from(budgets.ai.max_backends_over_cap);
        if u64::from(phase.ai_active_backends_max) > cap {
            out.push(breach(
                &format!("{}: active ai backends", phase.name),
                cap,
                u64::from(phase.ai_active_backends_max),
            ));
        }
        if let Some(base) = baseline {
            let p95_limit = base.p95_us * budgets.human.p95_ratio_permille / 1000
                + budgets.human.p95_slack_ms * 1000;
            if phase.human.p95_us > p95_limit {
                out.push(breach(
                    &format!("{}: human p95 (us)", phase.name),
                    p95_limit,
                    phase.human.p95_us,
                ));
            }
            let p99_limit = base.p99_us * budgets.human.p99_ratio_permille / 1000
                + budgets.human.p99_slack_ms * 1000;
            if phase.human.p99_us > p99_limit {
                out.push(breach(
                    &format!("{}: human p99 (us)", phase.name),
                    p99_limit,
                    phase.human.p99_us,
                ));
            }
        }
    }
    let leaks = &report.leaks;
    if leaks.connections > budgets.ai.max_leaked_connections {
        out.push(breach(
            "leaked connections",
            u64::from(budgets.ai.max_leaked_connections),
            u64::from(leaks.connections),
        ));
    }
    if leaks.permits > budgets.ai.max_leaked_permits {
        out.push(breach(
            "leaked permits",
            u64::from(budgets.ai.max_leaked_permits),
            u64::from(leaks.permits),
        ));
    }
    if leaks.running_after_cancel > budgets.ai.max_running_after_cancel {
        out.push(breach(
            "statements running after cancellation",
            u64::from(budgets.ai.max_running_after_cancel),
            u64::from(leaks.running_after_cancel),
        ));
    }
    if leaks.ai_write_allowed {
        out.push(breach("ai role can write", 0, 1));
    }
    if leaks.left_behind {
        out.push(breach("role or schema left behind", 0, 1));
    }
    match report.recovery.recovered_after_ms {
        Some(ms) if ms <= budgets.ai.recovery_within_ms => {}
        Some(ms) => out.push(breach(
            "admission recovery (ms)",
            budgets.ai.recovery_within_ms,
            ms,
        )),
        None => out.push(breach(
            "admission recovery (ms)",
            budgets.ai.recovery_within_ms,
            u64::MAX,
        )),
    }
    out
}

fn breach(check: &str, limit: u64, actual: u64) -> Breach {
    Breach {
        check: check.to_owned(),
        limit,
        actual,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::report::{LeakReport, PhaseReport, RecoveryReport};
    use crate::stats::LatencySummary;

    fn phase(name: &str, p95_ms: u64, p99_ms: u64) -> PhaseReport {
        PhaseReport {
            name: name.to_owned(),
            human: LatencySummary {
                count: 100,
                p50_us: 1000,
                p95_us: p95_ms * 1000,
                p99_us: p99_ms * 1000,
                max_us: p99_ms * 1000,
            },
            ai_active_backends_max: 4,
            ..PhaseReport::default()
        }
    }

    fn report(with_agents: PhaseReport) -> Report {
        Report {
            ai_pool_size: 4,
            phases: vec![phase("baseline", 20, 40), with_agents],
            recovery: RecoveryReport {
                recovered_after_ms: Some(500),
                ..RecoveryReport::default()
            },
            leaks: LeakReport::default(),
            ..Report::default()
        }
    }

    #[test]
    fn the_default_budgets_parse_and_carry_the_agreed_numbers() {
        let b = Budgets::defaults().unwrap();
        assert_eq!(b.human.p95_ratio_permille, 1250);
        assert_eq!(b.human.p95_slack_ms, 10);
        assert_eq!(b.human.p99_ratio_permille, 1500);
        assert_eq!(b.human.p99_slack_ms, 25);
        assert_eq!(b.human.max_errors, 0);
        assert_eq!(b.ai.max_backends_over_cap, 0);
        assert!(Budgets::from_toml("[human]").is_err());
    }

    #[test]
    fn a_run_inside_the_budgets_has_no_breach() {
        // Limits: p95 20 ms x 1.25 + 10 ms = 35 ms; p99 40 ms x 1.5 + 25 ms = 85 ms.
        let r = report(phase("with_agents", 35, 85));
        assert!(evaluate(&r, &Budgets::defaults().unwrap()).is_empty());
    }

    #[test]
    fn a_latency_one_step_over_the_limit_is_a_breach_naming_the_limit() {
        let r = report(phase("with_agents", 36, 86));
        let found = evaluate(&r, &Budgets::defaults().unwrap());
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].limit, 35_000);
        assert_eq!(found[0].actual, 36_000);
        assert_eq!(found[1].limit, 85_000);
    }

    #[test]
    fn errors_backends_leaks_writes_and_recovery_are_each_checked() {
        let mut with_agents = phase("with_agents", 20, 40);
        with_agents.human_errors = 1;
        with_agents.ai_active_backends_max = 5;
        let mut r = report(with_agents);
        r.leaks = LeakReport {
            permits: 1,
            connections: 1,
            running_after_cancel: 1,
            ai_write_allowed: true,
            left_behind: true,
        };
        r.recovery.recovered_after_ms = None;
        let checks: Vec<String> = evaluate(&r, &Budgets::defaults().unwrap())
            .into_iter()
            .map(|b| b.check)
            .collect();
        assert_eq!(checks.len(), 8, "{checks:?}");
        assert!(checks.iter().any(|c| c.contains("active ai backends")));
        assert!(checks.iter().any(|c| c == "admission recovery (ms)"));
    }
}
