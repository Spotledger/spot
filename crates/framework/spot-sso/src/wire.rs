//! Serde helpers for the storage contract's [`RecordId`], which has no serde of its own.
//!
//! Ids travel as their 16 raw bytes; a value that is not a UUIDv7 is refused on the way in,
//! so a persisted policy can never carry a malformed user id.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use spot_storage_contract::RecordId;

/// One [`RecordId`] as 16 bytes.
pub(crate) mod record_id {
    use super::{Deserialize, Deserializer, RecordId, Serialize, Serializer};

    /// Serialises the id's 16 bytes.
    pub fn serialize<S: Serializer>(id: &RecordId, serializer: S) -> Result<S::Ok, S::Error> {
        id.as_bytes().serialize(serializer)
    }

    /// Reads 16 bytes and validates them as a UUIDv7.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<RecordId, D::Error> {
        let bytes = <[u8; 16]>::deserialize(deserializer)?;
        RecordId::from_bytes(bytes).map_err(serde::de::Error::custom)
    }
}

/// A list of [`RecordId`]s as a list of 16-byte arrays.
pub(crate) mod vec_record_id {
    use super::{Deserialize, Deserializer, RecordId, Serialize, Serializer};

    /// Serialises the ids' 16 bytes.
    pub fn serialize<S: Serializer>(ids: &[RecordId], serializer: S) -> Result<S::Ok, S::Error> {
        let raw: Vec<[u8; 16]> = ids.iter().map(|id| *id.as_bytes()).collect();
        raw.serialize(serializer)
    }

    /// Reads 16-byte arrays, validating each as a UUIDv7.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<RecordId>, D::Error> {
        let raw = <Vec<[u8; 16]>>::deserialize(deserializer)?;
        raw.into_iter()
            .map(|bytes| RecordId::from_bytes(bytes).map_err(serde::de::Error::custom))
            .collect()
    }
}
