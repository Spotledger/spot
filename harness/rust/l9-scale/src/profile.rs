//! The workload model: a [`Profile`] (TOML) names the number of sessions, the ramp and the
//! steady phase, the think-time distribution, the operation mix and the open-loop arrival
//! rate. Decimal values in a file (a rate, an error budget) are parsed as text into integers,
//! so no float ever takes part in a calculation.

use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One kind of simulated user action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// A new sign-in (a fresh session token).
    SignIn,
    /// Open a form: the DocType metadata, then one document.
    OpenForm,
    /// A filtered list.
    List,
    /// Create a draft on the posting DocType and submit (post) it.
    CreateSubmit,
    /// Update the session's own draft with the version it last saw.
    Update,
    /// Run a report.
    ReportRun,
    /// An update that carries a stale version: a negative operation whose 4xx is expected.
    StaleUpdate,
}

/// Every operation, in report order.
pub const OPS: [Op; 7] = [
    Op::SignIn,
    Op::OpenForm,
    Op::List,
    Op::CreateSubmit,
    Op::Update,
    Op::ReportRun,
    Op::StaleUpdate,
];

impl Op {
    /// The operation's name in profiles, budgets and reports.
    pub fn name(self) -> &'static str {
        match self {
            Op::SignIn => "sign_in",
            Op::OpenForm => "open_form",
            Op::List => "list",
            Op::CreateSubmit => "create_submit",
            Op::Update => "update",
            Op::ReportRun => "report_run",
            Op::StaleUpdate => "stale_update",
        }
    }

    /// The operation named `name`.
    pub fn parse(name: &str) -> Option<Op> {
        OPS.into_iter().find(|op| op.name() == name)
    }

    /// The operation's position in [`OPS`].
    pub fn index(self) -> usize {
        OPS.iter().position(|op| *op == self).unwrap_or(0)
    }
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A decimal number written in a TOML file as an integer, a float or a string. It is turned
/// into a scaled integer by [`Decimal::scaled`] without any float arithmetic.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Decimal {
    /// `5`.
    Int(u64),
    /// `0.05`.
    Float(f64),
    /// `"0.05"`.
    Text(String),
}

impl Decimal {
    /// The value times `10^digits`, truncated, or `None` when it is negative, not a plain
    /// decimal, or too large.
    pub fn scaled(&self, digits: u32) -> Option<u64> {
        match self {
            Decimal::Int(n) => n.checked_mul(10u64.checked_pow(digits)?),
            Decimal::Float(f) => parse_decimal(&f.to_string(), digits),
            Decimal::Text(text) => parse_decimal(text, digits),
        }
    }
}

/// Parses `12`, `12.5` or `.5` into `value * 10^digits`, truncating extra fraction digits.
pub fn parse_decimal(text: &str, digits: u32) -> Option<u64> {
    let text = text.trim();
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    if whole.is_empty() && fraction.is_empty() {
        return None;
    }
    if !whole.bytes().all(|b| b.is_ascii_digit()) || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: u64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    value = value.checked_mul(10u64.checked_pow(digits)?)?;
    let mut part: u64 = 0;
    for position in 0..digits as usize {
        let digit = fraction
            .as_bytes()
            .get(position)
            .map_or(0, |b| u64::from(b - b'0'));
        part = part * 10 + digit;
    }
    value.checked_add(part)
}

/// How long a simulated user pauses after an action.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Think {
    /// Always `ms`.
    Fixed {
        /// The pause.
        ms: u64,
    },
    /// Uniform between `min_ms` and `max_ms`, drawn from the profile's seed.
    Uniform {
        /// The shortest pause.
        min_ms: u64,
        /// The longest pause.
        max_ms: u64,
    },
}

impl Think {
    /// One pause in nanoseconds, drawn with `rng`.
    pub fn sample_ns(&self, rng: &mut crate::rng::Rng) -> u64 {
        let ms = match self {
            Think::Fixed { ms } => *ms,
            Think::Uniform { min_ms, max_ms } => rng.between(*min_ms, *max_ms),
        };
        ms.saturating_mul(1_000_000)
    }
}

/// Weights over the operations; an operation with weight 0 is never chosen.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Mix {
    /// Weight of [`Op::SignIn`].
    #[serde(default)]
    pub sign_in: u32,
    /// Weight of [`Op::OpenForm`].
    #[serde(default)]
    pub open_form: u32,
    /// Weight of [`Op::List`].
    #[serde(default)]
    pub list: u32,
    /// Weight of [`Op::CreateSubmit`].
    #[serde(default)]
    pub create_submit: u32,
    /// Weight of [`Op::Update`].
    #[serde(default)]
    pub update: u32,
    /// Weight of [`Op::ReportRun`].
    #[serde(default)]
    pub report_run: u32,
    /// Weight of [`Op::StaleUpdate`].
    #[serde(default)]
    pub stale_update: u32,
}

impl Mix {
    /// The weight of `op`.
    pub fn weight(&self, op: Op) -> u32 {
        match op {
            Op::SignIn => self.sign_in,
            Op::OpenForm => self.open_form,
            Op::List => self.list,
            Op::CreateSubmit => self.create_submit,
            Op::Update => self.update,
            Op::ReportRun => self.report_run,
            Op::StaleUpdate => self.stale_update,
        }
    }

    /// The sum of the weights.
    pub fn total(&self) -> u64 {
        OPS.iter().map(|op| u64::from(self.weight(*op))).sum()
    }

    /// The operation a `roll` in `0..total` selects; the last weighted operation when the roll
    /// is out of range.
    pub fn pick(&self, roll: u64) -> Op {
        let mut left = roll;
        let mut last = Op::OpenForm;
        for op in OPS {
            let weight = u64::from(self.weight(op));
            if weight == 0 {
                continue;
            }
            last = op;
            if left < weight {
                return op;
            }
            left -= weight;
        }
        last
    }
}

fn default_seed() -> u64 {
    1
}
fn default_timeout_ms() -> u64 {
    10_000
}
fn default_sample_seconds() -> u64 {
    5
}

/// One workload profile, as a TOML file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The profile's name (budgets are keyed by it).
    pub name: String,
    /// The README tier it stands for (`smoke`, `S`, `M`, ...).
    pub tier: String,
    /// Simulated users, each with a session of its own.
    pub sessions: u32,
    /// Seconds over which the arrival rate climbs to the target (not budgeted).
    #[serde(default)]
    pub ramp_seconds: u64,
    /// Seconds at the target rate (the budgeted window).
    pub steady_seconds: u64,
    /// The seed of the schedule: the same seed gives the same arrivals and mix.
    #[serde(default = "default_seed")]
    pub seed: u64,
    /// The open-loop arrival rate in operations per second (decimals allowed).
    pub target_tps: Decimal,
    /// The pause a session takes after an arrival is assigned to it.
    pub think: Think,
    /// The operation mix.
    pub mix: Mix,
    /// A request that takes longer than this is a timeout failure.
    #[serde(default = "default_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Seconds between server resource samples.
    #[serde(default = "default_sample_seconds")]
    pub sample_every_seconds: u64,
    /// The report the `report_run` operation runs.
    #[serde(default = "default_report")]
    pub report: String,
}

fn default_report() -> String {
    "task_summary".to_owned()
}

/// What is wrong with a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError(pub String);

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProfileError {}

impl Profile {
    /// Parses and validates a profile.
    pub fn from_toml(text: &str) -> Result<Profile, ProfileError> {
        let profile: Profile =
            toml::from_str(text).map_err(|e| ProfileError(format!("profile: {e}")))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Reads and validates the profile at `path`.
    pub fn load(path: &Path) -> Result<Profile, ProfileError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ProfileError(format!("cannot read {}: {e}", path.display())))?;
        Profile::from_toml(&text)
    }

    /// The arrival rate in thousandths of an operation per second.
    pub fn target_milli_tps(&self) -> u64 {
        self.target_tps.scaled(3).unwrap_or(0)
    }

    /// Checks the profile; the first problem found is the error.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let bad = |what: &str| Err(ProfileError(format!("profile `{}`: {what}", self.name)));
        if self.name.is_empty()
            || !self
                .name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        {
            return Err(ProfileError(format!(
                "profile name `{}` must be lowercase letters, digits, `_` or `-`",
                self.name
            )));
        }
        if self.sessions == 0 || self.sessions > 100_000 {
            return bad("sessions must be between 1 and 100000");
        }
        if self.steady_seconds == 0 || self.steady_seconds > 7 * 24 * 3600 {
            return bad("steady_seconds must be between 1 and 604800");
        }
        if self.ramp_seconds > 24 * 3600 {
            return bad("ramp_seconds must be at most 86400");
        }
        if self.target_milli_tps() == 0 {
            return bad("target_tps must be a positive decimal with at most 3 fraction digits");
        }
        if self.request_timeout_ms == 0 {
            return bad("request_timeout_ms must be positive");
        }
        if self.sample_every_seconds == 0 {
            return bad("sample_every_seconds must be positive");
        }
        if self.mix.total() == 0 {
            return bad("the operation mix has no weight");
        }
        if let Think::Uniform { min_ms, max_ms } = &self.think
            && min_ms > max_ms
        {
            return bad("think min_ms is above max_ms");
        }
        if self.report.is_empty() {
            return bad("report must name a report");
        }
        Ok(())
    }

    /// The whole run, ramp and steady phase, in nanoseconds.
    pub fn total_ns(&self) -> u64 {
        self.ramp_seconds
            .saturating_add(self.steady_seconds)
            .saturating_mul(1_000_000_000)
    }

    /// The ramp in nanoseconds.
    pub fn ramp_ns(&self) -> u64 {
        self.ramp_seconds.saturating_mul(1_000_000_000)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    const GOOD: &str = r#"
name = "unit"
tier = "S"
sessions = 4
ramp_seconds = 2
steady_seconds = 8
target_tps = 2.5
[think]
kind = "uniform"
min_ms = 100
max_ms = 300
[mix]
open_form = 3
list = 1
"#;

    #[test]
    fn a_profile_parses_with_defaults() {
        let p = Profile::from_toml(GOOD).unwrap();
        assert_eq!(p.sessions, 4);
        assert_eq!(p.target_milli_tps(), 2_500);
        assert_eq!(p.seed, 1);
        assert_eq!(p.request_timeout_ms, 10_000);
        assert_eq!(p.mix.total(), 4);
        assert_eq!(p.total_ns(), 10_000_000_000);
    }

    #[test]
    fn decimals_are_parsed_without_floats() {
        assert_eq!(parse_decimal("0.05", 6), Some(50_000));
        assert_eq!(parse_decimal("12", 3), Some(12_000));
        assert_eq!(parse_decimal(".5", 3), Some(500));
        assert_eq!(parse_decimal("1.23456", 3), Some(1_234));
        assert_eq!(parse_decimal("-1", 3), None);
        assert_eq!(parse_decimal("1e3", 3), None);
        assert_eq!(parse_decimal("", 3), None);
        assert_eq!(Decimal::Int(7).scaled(3), Some(7_000));
        assert_eq!(Decimal::Float(0.25).scaled(6), Some(250_000));
        assert_eq!(Decimal::Text("1.5".into()).scaled(3), Some(1_500));
    }

    #[test]
    fn a_bad_profile_is_refused_with_the_reason() {
        let cases = [
            (GOOD.replace("sessions = 4", "sessions = 0"), "sessions"),
            (
                GOOD.replace("steady_seconds = 8", "steady_seconds = 0"),
                "steady",
            ),
            (
                GOOD.replace("target_tps = 2.5", "target_tps = 0"),
                "target_tps",
            ),
            (GOOD.replace("min_ms = 100", "min_ms = 900"), "min_ms"),
            (
                GOOD.replace("open_form = 3\nlist = 1", "list = 0"),
                "no weight",
            ),
            (
                GOOD.replace("name = \"unit\"", "name = \"Unit One\""),
                "name",
            ),
            (format!("{GOOD}\nextra = 1\n"), "extra"),
            (GOOD.replace("[mix]", "[mix]\nlogin = 1"), "login"),
        ];
        for (text, needle) in cases {
            let err = Profile::from_toml(&text).unwrap_err().to_string();
            assert!(err.contains(needle), "{err} should mention {needle}");
        }
    }

    #[test]
    fn the_mix_picks_by_weight_and_skips_zero_weights() {
        let p = Profile::from_toml(GOOD).unwrap();
        let picks: Vec<Op> = (0..4).map(|roll| p.mix.pick(roll)).collect();
        assert_eq!(picks, [Op::OpenForm, Op::OpenForm, Op::OpenForm, Op::List]);
        assert_eq!(p.mix.pick(99), Op::List);
    }

    #[test]
    fn operation_names_round_trip() {
        for op in OPS {
            assert_eq!(Op::parse(op.name()), Some(op));
            assert_eq!(OPS[op.index()], op);
        }
        assert_eq!(Op::parse("nope"), None);
    }

    #[test]
    fn think_times_are_seeded() {
        let think = Think::Uniform {
            min_ms: 10,
            max_ms: 20,
        };
        let a: Vec<u64> = {
            let mut r = crate::rng::Rng::new(3);
            (0..20).map(|_| think.sample_ns(&mut r)).collect()
        };
        let b: Vec<u64> = {
            let mut r = crate::rng::Rng::new(3);
            (0..20).map(|_| think.sample_ns(&mut r)).collect()
        };
        assert_eq!(a, b);
        assert!(a.iter().all(|ns| (10_000_000..=20_000_000).contains(ns)));
        assert_eq!(
            Think::Fixed { ms: 5 }.sample_ns(&mut crate::rng::Rng::new(1)),
            5_000_000
        );
    }
}
