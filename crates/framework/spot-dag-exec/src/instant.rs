//! The instant type: an argument-supplied UTC timestamp, serialised without a clock.
//!
//! ADR-13 and the brief's "must not" both forbid a second clock: this crate never reads the
//! wall clock and never formats an instant. Callers pass a
//! [`OffsetDateTime`](spot_app_api::prelude::OffsetDateTime) in, and [`Instant`] stores it as
//! the integer seconds and nanoseconds since the Unix epoch, so the wire form is float-free
//! (ADR-12) and stable.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;

use crate::error::ExecError;

/// A UTC instant as seconds and nanoseconds since the Unix epoch.
///
/// The two integers are the exact value of the `OffsetDateTime` a caller supplied; nothing is
/// rounded and no float is ever produced. Compare, sort and store `Instant`s; convert back with
/// [`Instant::to_offset`] only when a caller needs calendar fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instant {
    /// Seconds since the Unix epoch, UTC.
    pub seconds: i64,
    /// Nanoseconds within the second (`0..1_000_000_000`).
    pub nanos: u32,
}

impl Instant {
    /// The instant a caller-supplied timestamp denotes.
    #[must_use]
    pub fn from_offset(at: OffsetDateTime) -> Self {
        Self {
            seconds: at.unix_timestamp(),
            nanos: at.nanosecond(),
        }
    }

    /// The timestamp this instant denotes.
    ///
    /// # Errors
    /// [`ExecError::TimeInvalid`] when the seconds are outside the representable range or the
    /// nanoseconds are not within the second.
    pub fn to_offset(self) -> Result<OffsetDateTime, ExecError> {
        let base = OffsetDateTime::from_unix_timestamp(self.seconds).map_err(|_| {
            ExecError::TimeInvalid {
                seconds: self.seconds,
                nanos: self.nanos,
            }
        })?;
        base.replace_nanosecond(self.nanos)
            .map_err(|_| ExecError::TimeInvalid {
                seconds: self.seconds,
                nanos: self.nanos,
            })
    }
}
