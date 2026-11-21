//! Lifetimes of the credentials the operator commands hand out.
//!
//! Every time in this crate's commands is an argument: the binary reads the clock once and
//! passes the instant down, and the tests pass any instant they like.

use crate::{Error, Result};

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// How long a one-time first sign-in secret lives unless the operator says otherwise.
pub const DEFAULT_SECRET_HOURS: u32 = 72;
/// The longest a one-time first sign-in secret may live (14 days).
pub const MAX_SECRET_HOURS: u32 = 14 * 24;
/// How long a scripted-bootstrap API key lives unless the operator says otherwise.
pub const DEFAULT_KEY_HOURS: u32 = 24;
/// The longest a scripted-bootstrap API key may live (7 days).
pub const MAX_KEY_HOURS: u32 = 7 * 24;

/// When credentials are issued and for how long.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Issue {
    now: i64,
    secret_hours: u32,
    key_hours: u32,
}

impl Issue {
    /// Issuing at `now` (Unix seconds) with the default lifetimes.
    pub fn at(now: i64) -> Self {
        Self {
            now,
            secret_hours: DEFAULT_SECRET_HOURS,
            key_hours: DEFAULT_KEY_HOURS,
        }
    }

    /// Sets the one-time secret's lifetime in hours: at least 1, at most [`MAX_SECRET_HOURS`].
    pub fn secret_hours(mut self, hours: u32) -> Result<Self> {
        self.secret_hours = checked("the one-time secret", hours, MAX_SECRET_HOURS)?;
        Ok(self)
    }

    /// Sets the bootstrap API key's lifetime in hours: at least 1, at most [`MAX_KEY_HOURS`].
    pub fn key_hours(mut self, hours: u32) -> Result<Self> {
        self.key_hours = checked("the bootstrap API key", hours, MAX_KEY_HOURS)?;
        Ok(self)
    }

    /// The instant the one-time secret stops working.
    pub fn secret_expires_at(&self) -> i64 {
        self.now + i64::from(self.secret_hours) * HOUR
    }

    /// The instant the bootstrap API key stops working.
    pub fn key_expires_at(&self) -> i64 {
        self.now + i64::from(self.key_hours) * HOUR
    }

    /// The instant of issue.
    pub fn issued_at(&self) -> i64 {
        self.now
    }

    /// Issuing now, by the system clock. The edge (the binary) and library callers that cannot
    /// pass a time use this; everything below takes the instant as an argument.
    pub fn system() -> Self {
        use spot_core::Clock as _;
        Self::at(spot_core::SystemClock::new().now_utc().unix_timestamp())
    }
}

fn checked(what: &str, hours: u32, max: u32) -> Result<u32> {
    if hours == 0 || hours > max {
        return Err(Error(format!(
            "{what} must live between 1 and {max} hours ({} days at most)",
            i64::from(max) * HOUR / DAY
        )));
    }
    Ok(hours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_72_hours_and_24_hours() {
        let issue = Issue::at(1_000);
        assert_eq!(issue.secret_expires_at(), 1_000 + 72 * 3600);
        assert_eq!(issue.key_expires_at(), 1_000 + 24 * 3600);
    }

    #[test]
    fn lifetimes_are_capped_at_14_days_and_7_days() {
        let issue = Issue::at(0);
        assert!(issue.secret_hours(14 * 24).is_ok());
        assert!(issue.secret_hours(14 * 24 + 1).is_err());
        assert!(issue.secret_hours(0).is_err());
        assert!(issue.key_hours(7 * 24).is_ok());
        assert!(issue.key_hours(7 * 24 + 1).is_err());
        assert!(issue.key_hours(0).is_err());
        assert_eq!(
            issue.secret_hours(1).map(|i| i.secret_expires_at()).ok(),
            Some(3600)
        );
    }
}
