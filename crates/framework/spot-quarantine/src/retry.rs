//! Automatic retry: the backoff schedule, the cap, and the condition a capped item raises.
//!
//! Retry is **automatic first** (ADR-062 rule 5): a held item is re-applied after a backoff that
//! starts immediately and widens (ADR-064 section 7.4). The cap bounds the automatic work; once
//! it is reached the item stays `Held` and a [`Condition`] is raised for the health surface
//! (ADR-067). Time is always an argument, never the wall clock.

use serde::{Deserialize, Serialize};

use crate::item::QuarantineItem;
use crate::reason::ReasonInfo;

/// One microsecond, the unit every instant in this crate is measured in.
const SECOND: i64 = 1_000_000;

/// How urgent a condition is, in the health surface's three colours (ADR-067 section 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Informational; never pushed.
    Info,
    /// The system is handling it, or it is minor.
    Amber,
    /// A person is needed, or damage is possible.
    Red,
}

impl Severity {
    /// The stable text form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Amber => "amber",
            Severity::Red => "red",
        }
    }
}

/// A plain struct the future health surface consumes (ADR-067): severity, a sentence in plain
/// language, and the suggested action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Condition {
    /// The stable condition name (ADR-067's catalogue); `QUARANTINED` here.
    pub code: String,
    /// How urgent it is.
    pub severity: Severity,
    /// What it is about (the record's apply key).
    pub subject: String,
    /// One sentence in plain language.
    pub sentence: String,
    /// The suggested action.
    pub action: String,
}

impl Condition {
    /// The `QUARANTINED` condition for an item that has exhausted its automatic retries.
    #[must_use]
    pub fn quarantined(item: &QuarantineItem, info: &ReasonInfo) -> Condition {
        Condition {
            code: "QUARANTINED".to_owned(),
            severity: info.severity,
            subject: item.subject(),
            sentence: format!(
                "{} from {} could not be applied after {} attempts: {}",
                item.subject(),
                item.source,
                item.attempts,
                info.summary
            ),
            action: info.remedy.clone(),
        }
    }
}

/// The automatic retry schedule: a backoff after each attempt and a cap on the attempts.
///
/// `backoff[i]` is the delay (microseconds) before the `i`-th retry; the last value repeats.
/// `cap` is how many automatic attempts are allowed before the item stays `Held` and raises a
/// condition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Delays before each retry, in microseconds; the last value repeats. Must not be empty.
    pub backoff: Vec<i64>,
    /// How many automatic attempts are allowed.
    pub cap: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::sync_default()
    }
}

impl RetryPolicy {
    /// The ADR-064 section 7.4 schedule: immediately, then 1 minute, 5 minutes, 30 minutes,
    /// 2 hours, then every 6 hours; eight automatic attempts.
    #[must_use]
    pub fn sync_default() -> Self {
        Self {
            backoff: vec![
                0,
                60 * SECOND,
                5 * 60 * SECOND,
                30 * 60 * SECOND,
                2 * 60 * 60 * SECOND,
                6 * 60 * 60 * SECOND,
            ],
            cap: 8,
        }
    }

    /// A policy from explicit parts.
    #[must_use]
    pub fn new(backoff: Vec<i64>, cap: u32) -> Self {
        Self { backoff, cap }
    }

    /// The delay before the `attempt`-th retry (0-based); the last value repeats.
    #[must_use]
    pub fn delay(&self, attempt: u32) -> i64 {
        let last = self.backoff.len().saturating_sub(1);
        let index = usize::try_from(attempt).unwrap_or(usize::MAX).min(last);
        self.backoff.get(index).copied().unwrap_or(0)
    }

    /// The total delay before the `n`-th retry, saturating.
    fn cumulative(&self, n: u32) -> i64 {
        (0..n).fold(0_i64, |total, i| total.saturating_add(self.delay(i)))
    }

    /// When the item's next automatic retry is due: `first_seen` plus the delays already elapsed
    /// and the one before this attempt.
    #[must_use]
    pub fn due_at(&self, item: &QuarantineItem) -> i64 {
        item.first_seen
            .saturating_add(self.cumulative(item.attempts.saturating_add(1)))
    }

    /// Whether the item has used every automatic attempt.
    #[must_use]
    pub fn exhausted(&self, item: &QuarantineItem) -> bool {
        item.attempts >= self.cap
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::item::{ItemId, RecordRef, State};
    use spot_sync_protocol::{RecordName, SourceId};

    fn item(attempts: u32, first_seen: i64) -> QuarantineItem {
        QuarantineItem {
            id: ItemId(1),
            source: SourceId::new("head.office.app#hub").unwrap(),
            seq: 1,
            record_ref: RecordRef {
                name: RecordName::new("note:abc").unwrap(),
                version: 1,
                entity: "note".to_owned(),
                id: "abc".to_owned(),
            },
            reason_code: "replica_apply.unknown_entity".to_owned(),
            detail: String::new(),
            first_seen,
            attempts,
            state: State::Held,
        }
    }

    #[test]
    fn the_default_schedule_matches_adr_064() {
        let policy = RetryPolicy::sync_default();
        assert_eq!(policy.delay(0), 0);
        assert_eq!(policy.delay(1), 60 * SECOND);
        assert_eq!(policy.delay(2), 5 * 60 * SECOND);
        assert_eq!(policy.delay(5), 6 * 60 * 60 * SECOND);
        assert_eq!(policy.delay(99), 6 * 60 * 60 * SECOND, "the last repeats");
        assert_eq!(policy.cap, 8);
    }

    #[test]
    fn the_due_time_grows_with_the_attempts() {
        let policy = RetryPolicy::new(vec![0, 10, 20], 3);
        assert_eq!(policy.due_at(&item(0, 1_000)), 1_000);
        assert_eq!(policy.due_at(&item(1, 1_000)), 1_010);
        assert_eq!(policy.due_at(&item(2, 1_000)), 1_030);
        assert_eq!(
            policy.due_at(&item(3, 1_000)),
            1_050,
            "the last delay repeats"
        );
        assert!(policy.exhausted(&item(3, 1_000)));
        assert!(!policy.exhausted(&item(2, 1_000)));
    }
}
