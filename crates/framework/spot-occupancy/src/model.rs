//! The values the service speaks in: holder, state, hold, requests, limits.

use std::time::Duration;

use spot_storage_contract::{MAX_RANGE_SPAN_DAYS, RangeValue, RecordId, Version};
use time::OffsetDateTime;

use crate::error::OccupancyError;

/// The longest `holder` text stored (`user:` or `consumer:` plus an id).
pub const MAX_HOLDER_CHARS: u32 = 96;

/// Who holds a unit: a staff user or a consumer (ADR-083 section 2.2 item 4). A consumer is not a
/// `User` row, so the reference is typed text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Holder {
    /// `user:<id>`.
    User(String),
    /// `consumer:<id>`.
    Consumer(String),
}

impl Holder {
    /// A staff user. The id is checked by [`Holder::parse`] rules.
    pub fn user(id: &str) -> Result<Holder, OccupancyError> {
        Holder::parse(&format!("user:{id}"))
    }

    /// A consumer principal.
    pub fn consumer(id: &str) -> Result<Holder, OccupancyError> {
        Holder::parse(&format!("consumer:{id}"))
    }

    /// Parses `user:<id>` or `consumer:<id>`. The id is 1 to 64 characters of `A-Z a-z 0-9 - _`,
    /// so a holder can never carry separators, spaces or markup into an event or a log.
    pub fn parse(text: &str) -> Result<Holder, OccupancyError> {
        let bad = || {
            OccupancyError::InvalidRange(
                "the holder must be `user:<id>` or `consumer:<id>` with an id of 1 to 64 letters, \
                 digits, `-` or `_`"
                    .to_owned(),
            )
        };
        let (kind, id) = text.split_once(':').ok_or_else(bad)?;
        let id_ok = !id.is_empty()
            && id.len() <= 64
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !id_ok {
            return Err(bad());
        }
        match kind {
            "user" => Ok(Holder::User(id.to_owned())),
            "consumer" => Ok(Holder::Consumer(id.to_owned())),
            _ => Err(bad()),
        }
    }

    /// The stored form, `user:<id>` or `consumer:<id>`.
    pub fn to_text(&self) -> String {
        match self {
            Holder::User(id) => format!("user:{id}"),
            Holder::Consumer(id) => format!("consumer:{id}"),
        }
    }
}

/// The state of a stored record. `cancelled` is declared in metadata but never stored: a
/// cancelled record is deleted (the dates are free only when the row is gone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// A hold with a deadline.
    Tentative,
    /// A booking.
    Confirmed,
}

impl State {
    /// The stored text.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Tentative => crate::declaration::STATE_TENTATIVE,
            State::Confirmed => crate::declaration::STATE_CONFIRMED,
        }
    }

    /// Parses the stored text; `None` for anything else, including `cancelled`.
    pub fn parse(text: &str) -> Option<State> {
        match text {
            crate::declaration::STATE_TENTATIVE => Some(State::Tentative),
            crate::declaration::STATE_CONFIRMED => Some(State::Confirmed),
            _ => None,
        }
    }
}

/// One stored occupancy record as the service returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hold {
    /// The record id.
    pub id: RecordId,
    /// The unit it occupies.
    pub unit: RecordId,
    /// The half-open range `[start, end)`.
    pub during: RangeValue,
    /// Tentative or confirmed.
    pub state: State,
    /// Who holds it.
    pub holder: Holder,
    /// When a tentative hold expires; `None` once confirmed.
    pub expires_at: Option<OffsetDateTime>,
    /// The stored version after the operation.
    pub version: Version,
}

/// A request to take a unit for a range, as a hold (`hold`) or as a booking (`book_now`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRequest {
    /// The bookable resource.
    pub unit: RecordId,
    /// The half-open range wanted.
    pub during: RangeValue,
    /// Who asks. Always the authenticated principal, never a request field.
    pub holder: Holder,
    /// How long a hold lasts; `None` means [`Limits::default_ttl`]. `book_now` ignores it.
    pub ttl: Option<Duration>,
}

/// The caller of `confirm` or `release`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// The principal that holds the record; any other holder is refused with `NotHolder`.
    Holder(Holder),
    /// An operator or the platform, whose permission the lifecycle pipeline has already checked
    /// (ADR-51). Never built from a consumer request.
    Operator,
}

/// What `release` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Released {
    /// The record was deleted now.
    Released(Hold),
    /// There was nothing to delete: already released, expired and swept, or never there. A
    /// second release is a success, not an error.
    AlreadyGone,
}

/// One stretch of a unit that is not free. The holder is deliberately absent: availability must
/// not tell a caller who holds what (ADR-087 section 5.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Busy {
    /// The occupied range.
    pub during: RangeValue,
    /// Whether it is a live hold or a booking.
    pub state: State,
}

/// What one sweeper batch did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Holds deleted, each with an `occupancy_expired` event in the same transaction.
    pub expired: u32,
    /// Expired holds this node left alone because another node owns their unit.
    pub not_owned: u32,
    /// Whether the batch was full, so another pass may find more.
    pub more: bool,
}

/// The limits, all configurable defaults and none measured (ADR-087 section 5.4, 13).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// A hold's lifetime when the request names none. Default 10 minutes.
    pub default_ttl: Duration,
    /// The longest hold a request may ask for. Default 1 hour.
    pub max_ttl: Duration,
    /// The longest range, in whole days. Default 366; never above [`MAX_RANGE_SPAN_DAYS`].
    pub max_span_days: i64,
    /// Most rows one sweeper batch deletes. Default 200.
    pub sweep_batch: u32,
    /// Times a transaction that failed to serialise is rerun before the error is returned.
    /// Default 8.
    pub max_retries: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            default_ttl: Duration::from_secs(10 * 60),
            max_ttl: Duration::from_secs(60 * 60),
            max_span_days: 366,
            sweep_batch: 200,
            max_retries: 8,
        }
    }
}

impl Limits {
    /// Refuses a limit set that cannot work.
    pub fn validate(&self) -> Result<(), String> {
        if self.default_ttl.is_zero() || self.default_ttl > self.max_ttl {
            return Err("default_ttl must be above zero and not above max_ttl".into());
        }
        if self.max_span_days < 1 || self.max_span_days > i64::from(MAX_RANGE_SPAN_DAYS) {
            return Err(format!(
                "max_span_days must be between 1 and {MAX_RANGE_SPAN_DAYS}"
            ));
        }
        if self.sweep_batch == 0 {
            return Err("sweep_batch must be at least 1".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    #[test]
    fn holders_round_trip_and_refuse_bad_ids() {
        for text in ["user:abc-1", "consumer:A_9"] {
            let h = Holder::parse(text).expect("valid");
            assert_eq!(h.to_text(), text);
        }
        for bad in [
            "",
            "user:",
            "user",
            "admin:x",
            "user:a b",
            "user:a:b",
            "consumer:é",
        ] {
            assert!(Holder::parse(bad).is_err(), "{bad:?}");
        }
        assert!(Holder::parse(&format!("user:{}", "a".repeat(65))).is_err());
    }

    #[test]
    fn a_stored_cancelled_state_is_not_a_state_this_crate_reads() {
        assert_eq!(State::parse("tentative"), Some(State::Tentative));
        assert_eq!(State::parse("confirmed"), Some(State::Confirmed));
        assert_eq!(State::parse("cancelled"), None);
    }

    #[test]
    fn the_default_limits_are_valid_and_bad_ones_are_refused() {
        assert!(Limits::default().validate().is_ok());
        let l = Limits {
            max_span_days: i64::from(MAX_RANGE_SPAN_DAYS) + 1,
            ..Limits::default()
        };
        assert!(l.validate().is_err());
        let l = Limits {
            default_ttl: Limits::default().max_ttl + Duration::from_secs(1),
            ..Limits::default()
        };
        assert!(l.validate().is_err());
    }
}
