//! Six-field cron expressions for `[[schedules]]`.
//!
//! Fields are `{second} {minute} {hour} {day of month} {month} {day of week}`, separated by
//! whitespace, seconds first (for example `0 */15 * * * *`). A field is `*`, a number, a list
//! of either, or either followed by `/step`. Numbers stay in the field's bounds; `day of
//! week` accepts `0`-`7`, where `7` means the same day as `0` (Sunday). Checks are syntactic
//! only: the scheduler decides when an entry fires (WP-PLT-07).

use std::fmt;
use std::str::FromStr;

use crate::error::{INVALID_CRON, ManifestError};

/// Each field's name and bounds, in order: seconds, minutes, hours, day of month, month and
/// day of week.
const FIELDS: [(&str, u32, u32); 6] = [
    ("second", 0, 59),
    ("minute", 0, 59),
    ("hour", 0, 23),
    ("day of month", 1, 31),
    ("month", 1, 12),
    ("day of week", 0, 7),
];

/// A validated six-field cron expression from a `[[schedules]]` entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cron(String);

impl Cron {
    /// Parses a cron expression, such as `0 */15 * * * *`.
    pub fn parse(raw: &str) -> Result<Cron, ManifestError> {
        let parts: Vec<&str> = raw.split_whitespace().collect();
        if parts.len() != 6 {
            return Err(ManifestError::new(
                INVALID_CRON,
                format!(
                    "a cron expression has 6 space-separated fields (second minute hour day-of-month month day-of-week), got {raw:?}"
                ),
            ));
        }
        for (field, (name, low, high)) in parts.iter().zip(FIELDS) {
            for part in field.split(',') {
                check_part(part, name, low, high)?;
            }
        }
        // The same grammar the scheduler uses (WP-PLT-07): nothing this crate accepts may
        // fail to parse there later.
        if let Err(err) = cron::Schedule::from_str(raw) {
            return Err(ManifestError::new(
                INVALID_CRON,
                format!("cron expression {raw:?}: {err}"),
            ));
        }
        Ok(Cron(raw.to_owned()))
    }

    /// The expression as written in the manifest.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for Cron {
    type Err = ManifestError;

    fn from_str(raw: &str) -> Result<Cron, ManifestError> {
        Cron::parse(raw)
    }
}

impl fmt::Display for Cron {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Checks one part of one field: `number`, `lo-hi` or `*`, optionally with a `/step`.
fn check_part(part: &str, name: &str, low: u32, high: u32) -> Result<(), ManifestError> {
    let bad = |why: String| {
        Err(ManifestError::new(
            INVALID_CRON,
            format!("cron {name} `{part}`: {why}, {name} runs {low}-{high}"),
        ))
    };
    let (base, step) = match part.split_once('/') {
        Some((base, step)) => (base, Some(step)),
        None => (part, None),
    };
    let (lo, hi) = match base {
        "*" => (low, high),
        range => match range.split_once('-') {
            Some((a, b)) => match (number(a), number(b)) {
                (Some(lo), Some(hi)) if lo <= hi => (lo, hi),
                (Some(_), Some(_)) => return bad(format!("range {range} is descending")),
                _ => return bad(format!("`{range}` is not a number or number-number range")),
            },
            None => match number(range) {
                Some(n) => (n, n),
                None => return bad(format!("`{range}` is not a number")),
            },
        },
    };
    if !(low..=high).contains(&lo) || !(low..=high).contains(&hi) {
        return bad(format!("`{base}` is outside the field's bounds"));
    }
    if let Some(step) = step {
        match number(step) {
            Some(s) if s >= 1 => {}
            Some(_) => return bad("its step is 0".to_owned()),
            None => return bad(format!("its step `{step}` is not a number")),
        }
    }
    Ok(())
}

/// A plain decimal number, no sign, no leading `+`.
fn number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}
