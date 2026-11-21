//! The image record: one captured record as of the snapshot's position.

use serde::{Deserialize, Serialize};

/// One state image. Input must arrive strictly ascending by `(entity, id)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRecord {
    /// The entity (DocType) name.
    pub entity: String,
    /// The record id within the entity.
    pub id: String,
    /// The record's version.
    pub version: u64,
    /// The entity schema version the image was captured under.
    pub entity_schema: u64,
    /// The image. Personal fields arrive already protected; this crate does not read them.
    pub image: serde_json::Value,
}
