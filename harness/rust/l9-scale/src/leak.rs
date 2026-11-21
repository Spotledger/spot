//! Leak detection for the soak run (WP-TST-15.T5): a series of samples taken every 15 s, a
//! robust slope (Theil-Sen) over the second half of the run, and the budgets of
//! `budgets/soak.toml`.
//!
//! This file is self-contained (standard library and `serde`/`toml` only) so that
//! `tests/soak.rs` includes it by path. Everything is integer arithmetic: slopes are in
//! thousandths of a unit per second, rates in parts per million. Time is an argument: samples
//! carry their own second offset.
#![allow(dead_code)]

use std::fmt::Write as _;

use serde::Deserialize;

/// The most points a slope is fitted over (longer series are thinned evenly): the fit is
/// quadratic in the number of points.
pub const MAX_FIT_POINTS: usize = 400;

/// One reading of the run. A series the run could not read is `None` in every sample.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sample {
    /// Seconds since the run started.
    pub at_s: u64,
    /// Resident set size of the host, kB.
    pub rss_kb: Option<u64>,
    /// Open file descriptors of the host.
    pub open_fds: Option<u64>,
    /// PostgreSQL connections to the run's database.
    pub pg_connections: Option<u64>,
    /// Job backlog (rows waiting).
    pub backlog: Option<u64>,
    /// Outbox lag, milliseconds.
    pub outbox_lag_ms: Option<u64>,
    /// WAL written since the run started, kB.
    pub wal_kb: Option<u64>,
    /// Size of the log and site directory, kB.
    pub log_kb: Option<u64>,
    /// Operations completed so far (cumulative).
    pub ops_total: u64,
    /// Operations failed so far (cumulative).
    pub ops_failed: u64,
}

/// The limits of `budgets/soak.toml`; every one is optional.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Resident memory growth, kB per 10 minutes.
    pub rss_kb_per_10min: Option<u64>,
    /// Allowed max minus min of the file descriptors.
    pub fd_spread: Option<u64>,
    /// Allowed max minus min of the PostgreSQL connections.
    pub pg_connection_spread: Option<u64>,
    /// Job backlog p99.
    pub backlog_p99: Option<u64>,
    /// Outbox lag p99, milliseconds.
    pub outbox_lag_p99_ms: Option<u64>,
    /// WAL kB per completed operation.
    pub wal_kb_per_op: Option<u64>,
    /// Log and site directory kB per completed operation.
    pub log_kb_per_op: Option<u64>,
    /// Highest error rate of any sampling interval, parts per million.
    pub error_ppm_max: Option<u64>,
    /// Allowed upward trend of the interval error rate, ppm per 10 minutes.
    pub error_ppm_slope_per_10min: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    leak: Limits,
}

impl Limits {
    /// Parses the `[leak]` table of a soak budgets file.
    pub fn from_toml(text: &str) -> Result<Limits, String> {
        let file: File = toml::from_str(text).map_err(|e| format!("soak budgets: {e}"))?;
        Ok(file.leak)
    }
}

/// The result of one check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The series (`rss_kb`, `open_fds`, ...).
    pub series: &'static str,
    /// What was checked (`slope per 10 min`, `spread`, `p99`, ...).
    pub check: &'static str,
    /// The limit with its unit.
    pub limit: String,
    /// The measured value with its unit, or `not measured`.
    pub measured: String,
    /// `None` when the series was not measured; else whether the budget held.
    pub passed: Option<bool>,
    /// The second of the first sample at which the budget was breached.
    pub first_breach_at_s: Option<u64>,
}

impl Finding {
    /// Whether this finding fails the run (a breach; "not measured" does not fail).
    pub fn failed(&self) -> bool {
        self.passed == Some(false)
    }
}

/// The Theil-Sen slope of `points` (`x` in seconds, `y` in the series' unit) in thousandths of
/// a unit per second: the median of the slopes of all point pairs. `None` with fewer than two
/// distinct `x`.
pub fn theil_sen_milli(points: &[(u64, u64)]) -> Option<i64> {
    let thinned = thin(points);
    let mut slopes: Vec<i64> = Vec::new();
    for (i, a) in thinned.iter().enumerate() {
        for b in &thinned[i + 1..] {
            if b.0 > a.0 {
                let dy = i128::from(b.1) - i128::from(a.1);
                let dx = i128::from(b.0 - a.0);
                slopes.push(i64::try_from(dy * 1000 / dx).unwrap_or(i64::MAX));
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    slopes.sort_unstable();
    let mid = slopes.len() / 2;
    if slopes.len() % 2 == 1 {
        slopes.get(mid).copied()
    } else {
        let low = *slopes.get(mid - 1)?;
        let high = *slopes.get(mid)?;
        Some(low / 2 + high / 2 + (low % 2 + high % 2) / 2)
    }
}

fn thin(points: &[(u64, u64)]) -> Vec<(u64, u64)> {
    if points.len() <= MAX_FIT_POINTS {
        return points.to_vec();
    }
    (0..MAX_FIT_POINTS)
        .filter_map(|i| {
            points
                .get(i * (points.len() - 1) / (MAX_FIT_POINTS - 1))
                .copied()
        })
        .collect()
}

/// A slope in thousandths of a unit per second, as units per 10 minutes.
pub fn per_10min(slope_milli: i64) -> i64 {
    slope_milli.saturating_mul(600) / 1000
}

/// The samples of the second half of the run (by index), the part budgets are judged on.
pub fn second_half(samples: &[Sample]) -> &[Sample] {
    samples.get(samples.len() / 2..).unwrap_or(&[])
}

/// The value at percentile `permille`/1000 of `values` (nearest rank, rounded up).
pub fn percentile(values: &[u64], permille: usize) -> Option<u64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank = (sorted.len() * permille).div_ceil(1000).max(1);
    sorted.get(rank - 1).copied()
}

/// True when the series' four quarter maxima strictly increase: growth that never levels off.
pub fn grows_monotonically(values: &[u64]) -> bool {
    if values.len() < 8 {
        return false;
    }
    let q = values.len() / 4;
    let maxima: Vec<u64> = (0..4)
        .map(|k| {
            let end = if k == 3 { values.len() } else { (k + 1) * q };
            values
                .get(k * q..end)
                .and_then(|s| s.iter().max().copied())
                .unwrap_or(0)
        })
        .collect();
    maxima.windows(2).all(|w| w[0] < w[1])
}

fn points_of(samples: &[Sample], pick: impl Fn(&Sample) -> Option<u64>) -> Vec<(u64, u64)> {
    samples
        .iter()
        .filter_map(|s| pick(s).map(|y| (s.at_s, y)))
        .collect()
}

fn spread(values: &[u64]) -> Option<u64> {
    Some(values.iter().max()?.saturating_sub(*values.iter().min()?))
}

/// Error rate of each sampling interval in parts per million, with the second it ends at.
pub fn interval_error_ppm(samples: &[Sample]) -> Vec<(u64, u64)> {
    samples
        .windows(2)
        .filter_map(|w| {
            let ops = w[1].ops_total.checked_sub(w[0].ops_total)?;
            let failed = w[1].ops_failed.checked_sub(w[0].ops_failed)?;
            if ops == 0 {
                return None;
            }
            let ppm = u128::from(failed) * 1_000_000 / u128::from(ops);
            Some((w[1].at_s, u64::try_from(ppm).unwrap_or(u64::MAX)))
        })
        .collect()
}

/// The first second at which `breached` holds on the growing prefix of `points` (checked from
/// 8 points on, at no more than 60 prefixes).
fn first_breach_of(points: &[(u64, u64)], breached: impl Fn(&[(u64, u64)]) -> bool) -> Option<u64> {
    let min = 8.min(points.len());
    let step = (points.len().saturating_sub(min) / 60).max(1);
    let mut n = min;
    loop {
        if n > points.len() {
            return None;
        }
        let prefix = points.get(..n)?;
        if breached(prefix) {
            return prefix.last().map(|p| p.0);
        }
        if n == points.len() {
            return None;
        }
        n = (n + step).min(points.len());
    }
}

fn not_measured(series: &'static str, check: &'static str, limit: String) -> Finding {
    Finding {
        series,
        check,
        limit,
        measured: "not measured".to_owned(),
        passed: None,
        first_breach_at_s: None,
    }
}

fn slope_finding(
    series: &'static str,
    check: &'static str,
    unit: &str,
    points: &[(u64, u64)],
    limit_per_10min: u64,
) -> Finding {
    let limit = format!("{limit_per_10min} {unit} per 10 min");
    let Some(slope) = theil_sen_milli(points) else {
        return not_measured(series, check, limit);
    };
    let over = |p: &[(u64, u64)]| {
        theil_sen_milli(p)
            .is_some_and(|s| per_10min(s) > i64::try_from(limit_per_10min).unwrap_or(i64::MAX))
    };
    let passed = per_10min(slope) <= i64::try_from(limit_per_10min).unwrap_or(i64::MAX);
    Finding {
        series,
        check,
        limit,
        measured: format!("{} {unit} per 10 min", per_10min(slope)),
        passed: Some(passed),
        first_breach_at_s: if passed {
            None
        } else {
            first_breach_of(points, over)
        },
    }
}

fn spread_finding(series: &'static str, points: &[(u64, u64)], limit: u64) -> Finding {
    let values: Vec<u64> = points.iter().map(|p| p.1).collect();
    let text = format!("{limit}");
    let Some(measured) = spread(&values) else {
        return not_measured(series, "max minus min", text);
    };
    let over = |p: &[(u64, u64)]| {
        let v: Vec<u64> = p.iter().map(|x| x.1).collect();
        spread(&v).is_some_and(|s| s > limit)
    };
    Finding {
        series,
        check: "max minus min",
        limit: text,
        measured: format!("{measured}"),
        passed: Some(measured <= limit),
        first_breach_at_s: if measured <= limit {
            None
        } else {
            first_breach_of(points, over)
        },
    }
}

fn bounded_finding(
    series: &'static str,
    unit: &str,
    points: &[(u64, u64)],
    limit: u64,
) -> Vec<Finding> {
    let values: Vec<u64> = points.iter().map(|p| p.1).collect();
    let Some(p99) = percentile(&values, 990) else {
        return vec![
            not_measured(series, "p99", format!("{limit} {unit}")),
            not_measured(series, "no monotonic growth", "levels off".to_owned()),
        ];
    };
    let first = points.iter().find(|p| p.1 > limit).map(|p| p.0);
    let grows = grows_monotonically(&values);
    vec![
        Finding {
            series,
            check: "p99",
            limit: format!("{limit} {unit}"),
            measured: format!("{p99} {unit}"),
            passed: Some(p99 <= limit),
            first_breach_at_s: if p99 <= limit { None } else { first },
        },
        Finding {
            series,
            check: "no monotonic growth",
            limit: "levels off".to_owned(),
            measured: if grows {
                "grows in every quarter"
            } else {
                "levels off"
            }
            .to_owned(),
            passed: Some(!grows),
            first_breach_at_s: None,
        },
    ]
}

fn per_op_finding(
    series: &'static str,
    samples: &[Sample],
    pick: impl Fn(&Sample) -> Option<u64>,
    limit: u64,
) -> Finding {
    let text = format!("{limit} kB per operation");
    let measured: Vec<&Sample> = samples.iter().filter(|s| pick(s).is_some()).collect();
    let (Some(first), Some(last)) = (measured.first(), measured.last()) else {
        return not_measured(series, "growth per operation", text);
    };
    let grown = pick(last)
        .unwrap_or(0)
        .saturating_sub(pick(first).unwrap_or(0));
    let ops = last.ops_total.saturating_sub(first.ops_total);
    if ops == 0 {
        return not_measured(series, "growth per operation", text);
    }
    // Rounded up, so a run with a few operations is not flattered by the division.
    let per_op = grown.div_ceil(ops);
    let breach = if per_op > limit {
        measured
            .iter()
            .find(|s| {
                let g = pick(s)
                    .unwrap_or(0)
                    .saturating_sub(pick(first).unwrap_or(0));
                let o = s.ops_total.saturating_sub(first.ops_total);
                o > 0 && g.div_ceil(o) > limit
            })
            .map(|s| s.at_s)
    } else {
        None
    };
    Finding {
        series,
        check: "growth per operation",
        limit: text,
        measured: format!("{per_op} kB per operation ({grown} kB over {ops} operations)"),
        passed: Some(per_op <= limit),
        first_breach_at_s: breach,
    }
}

/// Judges a whole run: every check of `limits` on the series of `samples`. Slopes, spreads and
/// the bounded series use the second half of the run; per-operation growth and the error rate
/// use all of it (growth is cumulative, and an error in the first half still counts).
pub fn evaluate(samples: &[Sample], limits: &Limits) -> Vec<Finding> {
    let half = second_half(samples);
    let mut out = Vec::new();
    if let Some(limit) = limits.rss_kb_per_10min {
        out.push(slope_finding(
            "rss_kb",
            "slope",
            "kB",
            &points_of(half, |s| s.rss_kb),
            limit,
        ));
    }
    if let Some(limit) = limits.fd_spread {
        out.push(spread_finding(
            "open_fds",
            &points_of(half, |s| s.open_fds),
            limit,
        ));
    }
    if let Some(limit) = limits.pg_connection_spread {
        out.push(spread_finding(
            "pg_connections",
            &points_of(half, |s| s.pg_connections),
            limit,
        ));
    }
    if let Some(limit) = limits.backlog_p99 {
        out.extend(bounded_finding(
            "job_backlog",
            "rows",
            &points_of(half, |s| s.backlog),
            limit,
        ));
    }
    if let Some(limit) = limits.outbox_lag_p99_ms {
        out.extend(bounded_finding(
            "outbox_lag_ms",
            "ms",
            &points_of(half, |s| s.outbox_lag_ms),
            limit,
        ));
    }
    if let Some(limit) = limits.wal_kb_per_op {
        out.push(per_op_finding("wal_kb", samples, |s| s.wal_kb, limit));
    }
    if let Some(limit) = limits.log_kb_per_op {
        out.push(per_op_finding("log_kb", samples, |s| s.log_kb, limit));
    }
    let errors = interval_error_ppm(samples);
    if let Some(limit) = limits.error_ppm_max {
        let worst = errors.iter().max_by_key(|p| p.1).copied();
        out.push(match worst {
            None => not_measured("error_ppm", "highest interval", format!("{limit} ppm")),
            Some((_, ppm)) => Finding {
                series: "error_ppm",
                check: "highest interval",
                limit: format!("{limit} ppm"),
                measured: format!("{ppm} ppm"),
                passed: Some(ppm <= limit),
                first_breach_at_s: errors.iter().find(|p| p.1 > limit).map(|p| p.0),
            },
        });
    }
    if let Some(limit) = limits.error_ppm_slope_per_10min {
        let second: Vec<(u64, u64)> = errors.iter().skip(errors.len() / 2).copied().collect();
        out.push(slope_finding("error_ppm", "slope", "ppm", &second, limit));
    }
    out
}

/// The series as CSV (header and one line per sample); an absent value is an empty field. No
/// image, no dependency: the file is plotted wherever it is read.
pub fn csv(samples: &[Sample]) -> String {
    let mut out = String::from(
        "at_s,rss_kb,open_fds,pg_connections,backlog,outbox_lag_ms,wal_kb,log_kb,ops_total,ops_failed\n",
    );
    let field = |v: Option<u64>| v.map(|n| n.to_string()).unwrap_or_default();
    for s in samples {
        let _ = writeln!(
            out,
            "{},{},{},{},{},{},{},{},{},{}",
            s.at_s,
            field(s.rss_kb),
            field(s.open_fds),
            field(s.pg_connections),
            field(s.backlog),
            field(s.outbox_lag_ms),
            field(s.wal_kb),
            field(s.log_kb),
            s.ops_total,
            s.ops_failed
        );
    }
    out
}

/// The findings as a Markdown table plus the first breach of any budget.
pub fn findings_markdown(findings: &[Finding]) -> String {
    let mut out =
        String::from("| Series | Check | Limit | Measured | Verdict | First breach (s) |\n");
    out.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for f in findings {
        let verdict = match f.passed {
            Some(true) => "pass",
            Some(false) => "FAIL",
            None => "not measured",
        };
        let breach = f
            .first_breach_at_s
            .map_or("-".to_owned(), |s| s.to_string());
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {verdict} | {breach} |",
            f.series, f.check, f.limit, f.measured
        );
    }
    let first = findings
        .iter()
        .filter(|f| f.failed())
        .filter_map(|f| f.first_breach_at_s.map(|s| (s, f.series)))
        .min();
    if let Some((s, series)) = first {
        let _ = writeln!(out, "\nFirst budget breach: `{series}` at second {s}.");
    }
    out
}
