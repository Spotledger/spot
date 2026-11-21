//! Serde helpers for instants: the stable wire form of every `OffsetDateTime` field.
//!
//! An instant is written as whole **Unix seconds** (an `i64`), UTC. Sub-second precision is
//! deliberately dropped: every SLA offset, due instant and event time in this crate is a whole
//! number of seconds, and a stable, unambiguous form matters more than fractions here. The
//! helpers are self-contained (they never name the `time` crate) so a host can persist a task
//! and its events with nothing but `serde`.

/// Serde for one `OffsetDateTime` as Unix seconds.
pub mod instant {
    use serde::{Deserialize, Deserializer, Serializer};
    use spot_app_api::prelude::OffsetDateTime;

    /// Writes the instant as whole Unix seconds.
    ///
    /// # Errors
    /// Whatever the serializer reports.
    pub fn serialize<S: Serializer>(
        value: &OffsetDateTime,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_i64(value.unix_timestamp())
    }

    /// Reads an instant from whole Unix seconds.
    ///
    /// # Errors
    /// A deserialisation error when the value is not an integer or is out of range.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OffsetDateTime, D::Error> {
        let seconds = i64::deserialize(deserializer)?;
        OffsetDateTime::from_unix_timestamp(seconds).map_err(serde::de::Error::custom)
    }
}

/// Serde for an optional `OffsetDateTime` as optional Unix seconds.
pub mod option_instant {
    use serde::{Deserialize, Deserializer, Serializer};
    use spot_app_api::prelude::OffsetDateTime;

    /// Writes the instant, or nothing when it is absent.
    ///
    /// # Errors
    /// Whatever the serializer reports.
    pub fn serialize<S: Serializer>(
        value: &Option<OffsetDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(instant) => serializer.serialize_some(&instant.unix_timestamp()),
            None => serializer.serialize_none(),
        }
    }

    /// Reads an optional instant from optional Unix seconds.
    ///
    /// # Errors
    /// A deserialisation error when the value is not an integer or is out of range.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        let seconds = Option::<i64>::deserialize(deserializer)?;
        match seconds {
            Some(seconds) => OffsetDateTime::from_unix_timestamp(seconds)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}
