//! Values crossing the database boundary: instants and the cursor as exact `numeric` text.

use time::OffsetDateTime;

use crate::error::OpError;

/// An instant as nanoseconds since the Unix epoch, in decimal text (bound as `$n::text::numeric`).
pub(crate) fn encode_instant(at: OffsetDateTime) -> String {
    at.unix_timestamp_nanos().to_string()
}

/// The instant a `numeric` text column holds.
pub(crate) fn decode_instant(text: &str) -> Result<OffsetDateTime, OpError> {
    let nanos: i128 = text.parse().map_err(|_| OpError::Decode)?;
    OffsetDateTime::from_unix_timestamp_nanos(nanos).map_err(|_| OpError::Decode)
}

/// A cursor or a range bound from a `numeric` text column.
pub(crate) fn decode_u128(text: &str) -> Result<u128, OpError> {
    text.parse().map_err(|_| OpError::Decode)
}

/// A range bound that must fit a `u64`.
pub(crate) fn decode_u64(text: &str) -> Result<u64, OpError> {
    text.parse().map_err(|_| OpError::Decode)
}

/// A version from its `bigint` column.
pub(crate) fn decode_version(value: i64) -> Result<u64, OpError> {
    u64::try_from(value).map_err(|_| OpError::Decode)
}
