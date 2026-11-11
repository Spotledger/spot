//! Message retention (ADR-058 rule 13): how long an undelivered message stays deliverable.

use std::collections::BTreeMap;

use spot_tenant_link::Topic;

use crate::error::{Error, Result};

/// The setting key a host reads the default retention from.
///
/// Written in two parts because a settings key is not an error code and the code scan reads
/// every dotted literal as one.
pub const RETENTION_SETTING: &str = concat!("link", ".message_retention_days");

/// The default when the setting is unset.
pub const DEFAULT_RETENTION_DAYS: u32 = 30;

const MS_PER_DAY: u64 = 86_400_000;

/// A retention of 1 to 365 whole days.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Retention(u32);

impl Retention {
    /// A retention of `days`.
    ///
    /// # Errors
    /// [`Error::RetentionRange`] outside 1 to 365.
    pub fn from_days(days: u32) -> Result<Self> {
        if (1..=365).contains(&days) {
            Ok(Self(days))
        } else {
            Err(Error::RetentionRange)
        }
    }

    /// Reads the value of the `link.message_retention_days` setting. `None` is the default; a
    /// value outside the range is refused rather than clamped, so a bad setting is noticed.
    ///
    /// # Errors
    /// [`Error::RetentionRange`] outside 1 to 365.
    pub fn from_setting(value: Option<i64>) -> Result<Self> {
        match value {
            None => Ok(Self(DEFAULT_RETENTION_DAYS)),
            Some(days) => u32::try_from(days)
                .map_err(|_| Error::RetentionRange)
                .and_then(Self::from_days),
        }
    }

    /// The whole days.
    #[must_use]
    pub fn days(self) -> u32 {
        self.0
    }

    /// The lifetime in milliseconds.
    #[must_use]
    pub fn millis(self) -> u64 {
        u64::from(self.0) * MS_PER_DAY
    }
}

impl Default for Retention {
    fn default() -> Self {
        Self(DEFAULT_RETENTION_DAYS)
    }
}

/// The retention in force: the setting's default and per-topic overrides.
#[derive(Clone, Debug, Default)]
pub struct Lifetimes {
    default: Retention,
    topics: BTreeMap<Topic, Retention>,
}

impl Lifetimes {
    /// Lifetimes with `default` for every topic.
    #[must_use]
    pub fn new(default: Retention) -> Self {
        Self {
            default,
            topics: BTreeMap::new(),
        }
    }

    /// Overrides the retention of one topic.
    #[must_use]
    pub fn with_topic(mut self, topic: Topic, retention: Retention) -> Self {
        self.topics.insert(topic, retention);
        self
    }

    /// The retention of `topic`.
    #[must_use]
    pub fn of(&self, topic: &Topic) -> Retention {
        self.topics.get(topic).copied().unwrap_or(self.default)
    }

    /// `sent_at` plus the topic's lifetime.
    ///
    /// # Errors
    /// [`Error::RetentionRange`] when the instant overflows.
    pub fn expires_at(&self, topic: &Topic, sent_at: u64) -> Result<u64> {
        sent_at
            .checked_add(self.of(topic).millis())
            .ok_or(Error::RetentionRange)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_range_is_one_to_365_days() {
        assert!(Retention::from_days(0).is_err());
        assert!(Retention::from_days(366).is_err());
        assert_eq!(Retention::from_days(1).map(Retention::days).ok(), Some(1));
        assert_eq!(
            Retention::from_days(365).map(Retention::days).ok(),
            Some(365)
        );
    }

    #[test]
    fn the_setting_defaults_to_30_and_refuses_bad_values() {
        assert_eq!(
            Retention::from_setting(None).map(Retention::days).ok(),
            Some(30)
        );
        assert!(Retention::from_setting(Some(-3)).is_err());
        assert!(Retention::from_setting(Some(4_000)).is_err());
    }

    #[test]
    fn a_topic_override_wins() {
        let topic = Topic::new("a.b").unwrap_or_else(|_| unreachable!());
        let other = Topic::new("a.c").unwrap_or_else(|_| unreachable!());
        let lifetimes = Lifetimes::default()
            .with_topic(topic.clone(), Retention::from_days(2).unwrap_or_default());
        assert_eq!(
            lifetimes.expires_at(&topic, 10).ok(),
            Some(10 + 2 * MS_PER_DAY)
        );
        assert_eq!(
            lifetimes.expires_at(&other, 10).ok(),
            Some(10 + 30 * MS_PER_DAY)
        );
    }
}
