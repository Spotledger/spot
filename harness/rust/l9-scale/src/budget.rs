//! Budgets: what a run must stay within, per profile and operation.
//!
//! `budgets.toml` holds one `[[budget]]` table per profile and operation:
//!
//! ```toml
//! [[budget]]
//! profile = "smoke"
//! operation = "all"          # or one of: sign_in open_form list create_submit update report_run stale_update
//! p95_ms = 2000              # 95th percentile latency, milliseconds
//! p99_ms = 5000
//! max_error_rate = 0.05      # a fraction of operations: 0.05 is 5 %
//! min_throughput = 1         # completed operations per second over the steady phase
//! ```
//!
//! Every limit is optional; `all` covers every operation together. Decimals are read as
//! integers (parts per million, thousandths per second), so no float is compared.

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::metrics::{Metrics, OpStats};
use crate::profile::{Decimal, OPS, Op};

/// The operation name that stands for every operation together.
pub const ALL: &str = "all";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    profile: String,
    operation: String,
    p95_ms: Option<u64>,
    p99_ms: Option<u64>,
    max_error_rate: Option<Decimal>,
    min_throughput: Option<Decimal>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    budget: Vec<Row>,
}

/// One budget line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// The profile it applies to.
    pub profile: String,
    /// `all` or an operation name.
    pub operation: String,
    /// The p95 limit in milliseconds.
    pub p95_ms: Option<u64>,
    /// The p99 limit in milliseconds.
    pub p99_ms: Option<u64>,
    /// The error-rate limit in parts per million.
    pub max_error_ppm: Option<u64>,
    /// The throughput floor in thousandths of an operation per second.
    pub min_throughput_milli: Option<u64>,
}

/// All budgets of a file.
#[derive(Debug, Clone, Default)]
pub struct Budgets {
    lines: Vec<Budget>,
}

/// What is wrong with a budgets file or a run's budgets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetError(pub String);

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BudgetError {}

impl Budgets {
    /// Parses and validates a budgets file's text.
    pub fn from_toml(text: &str) -> Result<Budgets, BudgetError> {
        let file: File = toml::from_str(text).map_err(|e| BudgetError(format!("budgets: {e}")))?;
        let mut seen = BTreeSet::new();
        let mut lines = Vec::new();
        for row in file.budget {
            if row.operation != ALL && Op::parse(&row.operation).is_none() {
                return Err(BudgetError(format!(
                    "budgets: `{}` is not an operation (use `all` or one of {})",
                    row.operation,
                    OPS.iter().map(|o| o.name()).collect::<Vec<_>>().join(", ")
                )));
            }
            if !seen.insert((row.profile.clone(), row.operation.clone())) {
                return Err(BudgetError(format!(
                    "budgets: `{}` / `{}` appears twice",
                    row.profile, row.operation
                )));
            }
            let ppm = match &row.max_error_rate {
                Some(rate) => {
                    Some(rate.scaled(6).filter(|v| *v <= 1_000_000).ok_or_else(|| {
                        BudgetError(format!(
                            "budgets: max_error_rate of `{}` / `{}` must be a fraction from 0 to 1",
                            row.profile, row.operation
                        ))
                    })?)
                }
                None => None,
            };
            let throughput = match &row.min_throughput {
                Some(rate) => Some(rate.scaled(3).ok_or_else(|| {
                    BudgetError(format!(
                        "budgets: min_throughput of `{}` / `{}` must be a non-negative decimal",
                        row.profile, row.operation
                    ))
                })?),
                None => None,
            };
            if row.p95_ms.is_none() && row.p99_ms.is_none() && ppm.is_none() && throughput.is_none()
            {
                return Err(BudgetError(format!(
                    "budgets: `{}` / `{}` sets no limit",
                    row.profile, row.operation
                )));
            }
            lines.push(Budget {
                profile: row.profile,
                operation: row.operation,
                p95_ms: row.p95_ms,
                p99_ms: row.p99_ms,
                max_error_ppm: ppm,
                min_throughput_milli: throughput,
            });
        }
        Ok(Budgets { lines })
    }

    /// Reads and validates the file at `path`.
    pub fn load(path: &Path) -> Result<Budgets, BudgetError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| BudgetError(format!("cannot read {}: {e}", path.display())))?;
        Budgets::from_toml(&text)
    }

    /// The lines that apply to `profile`.
    pub fn for_profile(&self, profile: &str) -> Vec<&Budget> {
        self.lines.iter().filter(|b| b.profile == profile).collect()
    }

    /// Checks a run against the budgets of `profile`. A profile with no budget at all is an
    /// error: a run nobody budgeted must not pass by default.
    pub fn evaluate(
        &self,
        profile: &str,
        metrics: &Metrics,
        steady_seconds: u64,
    ) -> Result<Vec<Verdict>, BudgetError> {
        let lines = self.for_profile(profile);
        if lines.is_empty() {
            return Err(BudgetError(format!(
                "no budget for profile `{profile}`: add a [[budget]] table to the budgets file"
            )));
        }
        let mut verdicts = Vec::new();
        for line in lines {
            let stats = if line.operation == ALL {
                metrics.all()
            } else {
                match Op::parse(&line.operation) {
                    Some(op) => metrics.op(op).clone(),
                    None => continue,
                }
            };
            check_line(line, &stats, steady_seconds, &mut verdicts);
        }
        Ok(verdicts)
    }
}

/// The result of one limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    /// The operation (`all` or a name).
    pub operation: String,
    /// The metric (`p95_ms`, `p99_ms`, `error_rate`, `throughput`).
    pub metric: String,
    /// The limit, as text with its unit.
    pub limit: String,
    /// The measured value, as text with its unit.
    pub measured: String,
    /// Whether the run is within the limit.
    pub passed: bool,
}

/// Microseconds as `ms` with three decimals, in integers.
pub fn fmt_ms(us: u64) -> String {
    format!("{}.{:03} ms", us / 1000, us % 1000)
}

/// Parts per million as a percentage with four decimals.
pub fn fmt_ppm(ppm: u64) -> String {
    format!("{}.{:04} %", ppm / 10_000, ppm % 10_000)
}

/// Thousandths as a decimal with three digits.
pub fn fmt_milli(milli: u64) -> String {
    format!("{}.{:03}", milli / 1000, milli % 1000)
}

fn check_line(line: &Budget, stats: &OpStats, steady_seconds: u64, out: &mut Vec<Verdict>) {
    let op = line.operation.clone();
    let measured_any = stats.total() > 0;
    if let Some(limit) = line.p95_ms {
        let got = stats.latency.percentile(950);
        out.push(Verdict {
            operation: op.clone(),
            metric: "p95_ms".into(),
            limit: format!("{limit}.000 ms"),
            measured: if measured_any {
                fmt_ms(got)
            } else {
                "no samples".into()
            },
            passed: measured_any && got <= limit.saturating_mul(1000),
        });
    }
    if let Some(limit) = line.p99_ms {
        let got = stats.latency.percentile(990);
        out.push(Verdict {
            operation: op.clone(),
            metric: "p99_ms".into(),
            limit: format!("{limit}.000 ms"),
            measured: if measured_any {
                fmt_ms(got)
            } else {
                "no samples".into()
            },
            passed: measured_any && got <= limit.saturating_mul(1000),
        });
    }
    if let Some(limit) = line.max_error_ppm {
        let got = stats.error_rate_ppm();
        out.push(Verdict {
            operation: op.clone(),
            metric: "error_rate".into(),
            limit: fmt_ppm(limit),
            measured: if measured_any {
                fmt_ppm(got)
            } else {
                "no samples".into()
            },
            passed: measured_any && got <= limit,
        });
    }
    if let Some(limit) = line.min_throughput_milli {
        let got = throughput_milli(stats, steady_seconds);
        out.push(Verdict {
            operation: op,
            metric: "throughput".into(),
            limit: format!("{} ops/s", fmt_milli(limit)),
            measured: format!("{} ops/s", fmt_milli(got)),
            passed: got >= limit,
        });
    }
}

/// Completed operations per second over the steady phase, in thousandths.
pub fn throughput_milli(stats: &OpStats, steady_seconds: u64) -> u64 {
    if steady_seconds == 0 {
        return 0;
    }
    u64::try_from(u128::from(stats.total()) * 1000 / u128::from(steady_seconds)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::metrics::{Class, Outcome, classify_status};

    const FILE: &str = r#"
[[budget]]
profile = "p"
operation = "all"
p95_ms = 100
max_error_rate = 0.1
min_throughput = 1.5

[[budget]]
profile = "p"
operation = "list"
p99_ms = 50
"#;

    fn metrics(list_ms: &[u64], fail: usize) -> Metrics {
        let mut m = Metrics::new();
        for (i, ms) in list_ms.iter().enumerate() {
            let outcome = if i < fail {
                Outcome::of(Class::ServerError)
            } else {
                classify_status(200, false)
            };
            m.record(
                Op::List,
                true,
                outcome,
                ms * 1_000_000,
                ms * 1_000_000,
                1_000_000_000,
            );
        }
        m
    }

    #[test]
    fn a_run_inside_every_limit_passes() {
        let budgets = Budgets::from_toml(FILE).unwrap();
        let verdicts = budgets.evaluate("p", &metrics(&[10; 40], 0), 10).unwrap();
        assert_eq!(verdicts.len(), 4);
        assert!(verdicts.iter().all(|v| v.passed), "{verdicts:?}");
    }

    #[test]
    fn a_breach_names_the_budget_and_the_measured_value() {
        let budgets = Budgets::from_toml(FILE).unwrap();
        // 40 operations in 10 s is 4 ops/s; 2 of them failed (5 %), and one took 400 ms.
        let mut times = vec![10u64; 39];
        times.push(400);
        let verdicts = budgets.evaluate("p", &metrics(&times, 2), 10).unwrap();
        let by = |op: &str, metric: &str| {
            verdicts
                .iter()
                .find(|v| v.operation == op && v.metric == metric)
                .unwrap()
                .clone()
        };
        let p99 = by("list", "p99_ms");
        assert!(!p99.passed);
        assert_eq!(p99.limit, "50.000 ms");
        assert!(
            p99.measured.starts_with("40") || p99.measured.starts_with("400"),
            "{}",
            p99.measured
        );
        assert!(by("all", "p95_ms").passed);
        let rate = by("all", "error_rate");
        assert!(rate.passed);
        assert_eq!(rate.measured, "5.0000 %");
        assert_eq!(by("all", "throughput").measured, "4.000 ops/s");
    }

    #[test]
    fn too_slow_too_many_errors_and_too_little_throughput_all_fail() {
        let budgets = Budgets::from_toml(FILE).unwrap();
        let verdicts = budgets.evaluate("p", &metrics(&[500; 10], 5), 10).unwrap();
        let failed: Vec<(&str, &str)> = verdicts
            .iter()
            .filter(|v| !v.passed)
            .map(|v| (v.operation.as_str(), v.metric.as_str()))
            .collect();
        assert!(failed.contains(&("all", "p95_ms")));
        assert!(failed.contains(&("all", "error_rate")));
        assert!(failed.contains(&("all", "throughput")));
        assert!(failed.contains(&("list", "p99_ms")));
    }

    #[test]
    fn an_operation_that_never_ran_fails_its_latency_budget() {
        let budgets = Budgets::from_toml(FILE).unwrap();
        let verdicts = budgets.evaluate("p", &Metrics::new(), 10).unwrap();
        let list = verdicts.iter().find(|v| v.operation == "list").unwrap();
        assert!(!list.passed);
        assert_eq!(list.measured, "no samples");
    }

    #[test]
    fn a_profile_without_a_budget_is_an_error_not_a_pass() {
        let budgets = Budgets::from_toml(FILE).unwrap();
        let err = budgets.evaluate("other", &Metrics::new(), 10).unwrap_err();
        assert!(err.to_string().contains("other"));
    }

    #[test]
    fn a_bad_budgets_file_is_refused() {
        let cases = [
            (
                "[[budget]]\nprofile=\"p\"\noperation=\"nope\"\np95_ms=1\n",
                "nope",
            ),
            ("[[budget]]\nprofile=\"p\"\noperation=\"all\"\n", "no limit"),
            (
                "[[budget]]\nprofile=\"p\"\noperation=\"all\"\np95_ms=1\n[[budget]]\nprofile=\"p\"\noperation=\"all\"\np95_ms=2\n",
                "twice",
            ),
            (
                "[[budget]]\nprofile=\"p\"\noperation=\"all\"\nmax_error_rate=2\n",
                "fraction",
            ),
            (
                "[[budget]]\nprofile=\"p\"\noperation=\"all\"\np95=1\n",
                "p95",
            ),
        ];
        for (text, needle) in cases {
            let err = Budgets::from_toml(text).unwrap_err().to_string();
            assert!(err.contains(needle), "{err} should mention {needle}");
        }
    }

    #[test]
    fn numbers_are_rendered_in_integers() {
        assert_eq!(fmt_ms(1_234_567), "1234.567 ms");
        assert_eq!(fmt_ppm(12_345), "1.2345 %");
        assert_eq!(fmt_milli(9_500), "9.500");
    }
}
