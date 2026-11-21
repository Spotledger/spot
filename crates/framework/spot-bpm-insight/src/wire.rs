//! Serde helpers for the argument-supplied instants and the DAG node ids.
//!
//! `spot-dag`'s [`NodeId`] serialises but does not deserialise, and the `time` crate's
//! [`OffsetDateTime`] has no serde impl in this build. Both are encoded exactly, with integers
//! only (ADR-12): an instant is its `(seconds, nanos)` pair since the Unix epoch, the same pair
//! `spot-dag-exec` stores. Each submodule exposes the `serialize`/`deserialize` pair serde's
//! `with` attribute expects.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;

/// One instant as the integer pair `spot-dag-exec` stores: seconds and nanoseconds.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstantRepr {
    /// Seconds since the Unix epoch, UTC.
    seconds: i64,
    /// Nanoseconds within the second (`0..1_000_000_000`).
    nanos: u32,
}

fn to_repr(at: OffsetDateTime) -> InstantRepr {
    InstantRepr {
        seconds: at.unix_timestamp(),
        nanos: at.nanosecond(),
    }
}

fn from_repr(repr: &InstantRepr) -> Result<OffsetDateTime, String> {
    let base = OffsetDateTime::from_unix_timestamp(repr.seconds).map_err(|e| e.to_string())?;
    base.replace_nanosecond(repr.nanos)
        .map_err(|e| e.to_string())
}

/// An instant as its exact integer pair.
pub(crate) mod instant {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use spot_app_api::prelude::OffsetDateTime;

    use super::{InstantRepr, from_repr, to_repr};

    /// Serialises an instant.
    pub(crate) fn serialize<S: Serializer>(
        value: &OffsetDateTime,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        to_repr(*value).serialize(serializer)
    }

    /// Deserialises an instant.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<OffsetDateTime, D::Error> {
        let repr = InstantRepr::deserialize(deserializer)?;
        from_repr(&repr).map_err(serde::de::Error::custom)
    }
}

/// A node id as its string form, validated on the way in.
pub(crate) mod node_id {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use spot_dag::NodeId;

    /// Serialises a node id.
    pub(crate) fn serialize<S: Serializer>(
        value: &NodeId,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_str().serialize(serializer)
    }

    /// Deserialises a node id.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<NodeId, D::Error> {
        let raw = String::deserialize(deserializer)?;
        NodeId::new(&raw).map_err(serde::de::Error::custom)
    }
}

/// An optional instant as its exact integer pair, or null.
pub(crate) mod option_instant {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use spot_app_api::prelude::OffsetDateTime;

    use super::{InstantRepr, from_repr, to_repr};

    /// Serialises an optional instant.
    pub(crate) fn serialize<S: Serializer>(
        value: &Option<OffsetDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.map(to_repr).serialize(serializer)
    }

    /// Deserialises an optional instant.
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        let repr = Option::<InstantRepr>::deserialize(deserializer)?;
        repr.map(|r| from_repr(&r).map_err(serde::de::Error::custom))
            .transpose()
    }
}
