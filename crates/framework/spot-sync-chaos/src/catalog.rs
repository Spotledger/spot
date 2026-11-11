//! The seams the hub and the node ask for: scope, references and the handshake report.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use spot_sync_bootstrap::ScopeView;
use spot_sync_closure::{BundleRecord, ClosureError, RecordKey, StaticReferences};
use spot_sync_hub::StreamEntry;
use spot_sync_node::NodeCatalog;
use spot_sync_skew::{Incoming, NodeReport, RecordRef, SoftwareVersion};

use crate::world::{HUB_SOURCE, MAJOR, TOPIC};

/// What one destination is entitled to: a set of entities.
///
/// Scope is the access control of sync (ADR-063): the relay asks the destination's scope before
/// anything is queued, and the closure asks it again to tell an in-scope dependency from an
/// out-of-scope extra. Nothing here reads anything the node sent.
#[derive(Clone, Debug)]
pub struct EntityScope {
    entities: BTreeSet<String>,
}

impl EntityScope {
    /// A scope covering `entities`.
    #[must_use]
    pub fn new(entities: &[&str]) -> Self {
        Self {
            entities: entities.iter().map(|e| (*e).to_owned()).collect(),
        }
    }

    /// A scope covering every entity the world uses.
    #[must_use]
    pub fn everything(entities: &[&str]) -> Self {
        Self::new(entities)
    }

    /// Whether the scope covers `entity`.
    #[must_use]
    pub fn covers_entity(&self, entity: &str) -> bool {
        self.entities.contains(entity)
    }

    /// A stable text for the entity set, so a bootstrap cursor made under one scope does not
    /// resume under another.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut out = String::from("entities:");
        for entity in &self.entities {
            out.push_str(entity);
            out.push(',');
        }
        out
    }
}

impl spot_sync_hub::DestinationScope for EntityScope {
    fn covers(&self, entry: &StreamEntry) -> bool {
        self.covers_entity(&entry.key.entity)
    }

    fn covers_key(&self, key: &RecordKey) -> bool {
        self.covers_entity(&key.entity)
    }

    fn owns(&self, _key: &RecordKey) -> bool {
        // The hub owns every record of these entities; a node owns none of them, so the closure
        // never skips a target as "the destination has it".
        false
    }
}

impl ScopeView for EntityScope {
    fn covers(&self, key: &RecordKey, _image: &Value) -> bool {
        self.covers_entity(&key.entity)
    }

    fn fingerprint(&self) -> String {
        EntityScope::fingerprint(self)
    }
}

/// The node's catalog: no declared links, and one topic and schema major for every record.
///
/// The world's records carry no references, so no bundle is ever parked for a missing dependency
/// and the prefix cannot go backwards. Authority rules are not what this suite tests; order,
/// loss, duplication and acknowledgement are.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorldCatalog;

impl NodeCatalog for WorldCatalog {
    fn references(&self, _record: &BundleRecord) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(Vec::new())
    }

    fn classify(&self, record: &BundleRecord) -> Incoming {
        Incoming {
            record: RecordRef {
                entity: record.key.entity.clone(),
                id: record.key.id.clone(),
                version: record.position,
            },
            topic: TOPIC.to_owned(),
            major: MAJOR,
            requires_metadata: None,
        }
    }
}

/// The catalog the hub's closure reads: the same empty link set as the node's.
#[must_use]
pub fn references() -> StaticReferences {
    StaticReferences::new()
}

/// The handshake report of a node that can decode the world's one topic.
#[must_use]
pub fn report() -> NodeReport {
    let mut decodable: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    decodable.insert(TOPIC.to_owned(), BTreeSet::from([MAJOR]));
    NodeReport {
        software_version: SoftwareVersion::new(1, 0, 0),
        metadata_version: 1,
        migration_epoch: 0,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: decodable,
    }
}

/// The hub source id every delivery must come from.
///
/// # Errors
/// `Invalid` when the constant is not a usable source id, which no build allows.
pub fn hub_source() -> Result<spot_events_contract::SourceId, crate::ChaosError> {
    spot_events_contract::SourceId::new(HUB_SOURCE)
        .map_err(|error| crate::ChaosError::new(0, format!("hub source id: {error}")))
}
