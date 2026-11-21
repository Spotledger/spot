//! The unknown topics of a run, as a report row may carry them: names only, capped and checked.
//!
//! A topic name is metadata, never content, and the registry grammar (`[a-z][a-z0-9_]*`, at most
//! 63 bytes) cannot hold a tenant's data. The list is capped and sanitised all the same: a name
//! that does not match the grammar is counted as invalid and never echoed.

use spot_events_contract::names::Topic;
use spot_journal_rebuild::RebuildOutcome;

/// The most topic names a report row lists; the rest are counted.
pub const MAX_REPORTED_TOPICS: usize = 50;

/// The stable code of a rebuild stage that failed because of a topic the classifier did not know
/// (the strict policy). Tells "failed because of an unknown topic" from "found a mismatch" and
/// from "could not run".
pub const REBUILD_UNKNOWN_TOPIC: &str = "journal_audit_run.rebuild_unknown_topic";

/// Whether `name` is a topic name the registry grammar allows.
#[must_use]
pub fn is_valid_topic(name: &str) -> bool {
    Topic::any(name).is_ok()
}

/// The topics the classifier did not know: names only, at most [`MAX_REPORTED_TOPICS`], then
/// counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnknownTopics {
    /// Valid topic names, sorted, at most [`MAX_REPORTED_TOPICS`].
    pub names: Vec<String>,
    /// Valid names the rebuild listed beyond the cap.
    pub more: u64,
    /// Names that did not match the topic grammar (`invalid_topic`): counted, never echoed.
    pub invalid: u64,
    /// Whether the rebuild met more unknown topics than it listed.
    pub truncated: bool,
}

impl UnknownTopics {
    /// No unknown topic.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// From a list of names (sorted or not) and whether the list was already cut short.
    #[must_use]
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>, truncated: bool) -> Self {
        let mut found = Self {
            truncated,
            ..Self::default()
        };
        let mut valid: Vec<&str> = Vec::new();
        for name in names {
            if is_valid_topic(name) {
                valid.push(name);
            } else {
                found.invalid += 1;
            }
        }
        valid.sort_unstable();
        valid.dedup();
        for name in valid {
            if found.names.len() < MAX_REPORTED_TOPICS {
                found.names.push(name.to_owned());
            } else {
                found.more += 1;
            }
        }
        found
    }

    /// From what a completed rebuild reports.
    #[must_use]
    pub fn from_outcome(outcome: &RebuildOutcome) -> Self {
        Self::from_names(
            outcome.unknown_topics.iter().map(String::as_str),
            outcome.unknown_topics_truncated,
        )
    }

    /// How many topics are accounted for: listed, beyond the cap and invalid.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.names.len() as u64 + self.more + self.invalid
    }

    /// Whether no unknown topic was met.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.total() == 0 && !self.truncated
    }
}
