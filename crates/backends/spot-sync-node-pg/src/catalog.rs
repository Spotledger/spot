//! [`PgNodeCatalog`]: `NodeCatalog` over the site's effective metadata generation.
//!
//! The node can decode the entities of one metadata generation: their links (for the references
//! of an image) and their topic and schema major (for version-skew triage). A metadata swap
//! replaces the generation atomically ([`PgNodeCatalog::swap`]); a record already being
//! classified sees either the old generation or the new one, never a mix.
//!
//! A record whose entity the generation does not know is never applied: it is classified as
//! needing a metadata version above the generation's, so the intake holds it
//! (`MetadataTooLow`) until a swap raises the node's metadata version past it.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use serde_json::Value;
use spot_sync_closure::{BundleRecord, ClosureError, RecordKey, References};
use spot_sync_node::NodeCatalog;
use spot_sync_skew::{Incoming, Major, RecordRef, Topic};

/// What the generation says about one entity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityInfo {
    /// The event topic of the entity's records.
    pub topic: Topic,
    /// The schema major the generation decodes.
    pub major: Major,
}

/// One effective metadata generation.
#[derive(Clone)]
pub struct Generation {
    /// The generation's metadata version.
    pub version: u32,
    /// The declared references.
    pub references: Arc<dyn References + Send + Sync>,
    /// The entities the generation decodes.
    pub entities: BTreeMap<String, EntityInfo>,
}

impl std::fmt::Debug for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generation")
            .field("version", &self.version)
            .field("entities", &self.entities.len())
            .finish_non_exhaustive()
    }
}

/// The catalog of the running generation.
#[derive(Debug, Clone)]
pub struct PgNodeCatalog {
    current: Arc<RwLock<Arc<Generation>>>,
}

impl PgNodeCatalog {
    /// A catalog serving `generation`.
    #[must_use]
    pub fn new(generation: Generation) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(generation))),
        }
    }

    /// Replaces the generation (a metadata swap). Every clone of the catalog sees it.
    pub fn swap(&self, generation: Generation) {
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(generation);
    }

    /// The running generation's metadata version.
    #[must_use]
    pub fn version(&self) -> u32 {
        self.generation().version
    }

    fn generation(&self) -> Arc<Generation> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl NodeCatalog for PgNodeCatalog {
    fn references(&self, record: &BundleRecord) -> Result<Vec<RecordKey>, ClosureError> {
        let generation = self.generation();
        if !generation.entities.contains_key(&record.key.entity) {
            return Err(ClosureError::ReferenceUnreadable {
                entity: record.key.entity.clone(),
                field: "unknown".to_owned(),
            });
        }
        generation
            .references
            .references(&record.key.entity, &record.image)
    }

    fn classify(&self, record: &BundleRecord) -> Incoming {
        let generation = self.generation();
        let (topic, major, known) = match generation.entities.get(&record.key.entity) {
            Some(info) => (info.topic.clone(), info.major, true),
            None => (record.key.entity.clone(), 1, false),
        };
        // The sender's metadata version is in the image (the replication payload's
        // `meta_version`); a record whose entity this generation lacks needs a newer one.
        let stated = record
            .image
            .get("meta_version")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok());
        let requires_metadata = if known {
            stated
        } else {
            Some(
                generation
                    .version
                    .saturating_add(1)
                    .max(stated.unwrap_or(0)),
            )
        };
        Incoming {
            record: RecordRef {
                entity: record.key.entity.clone(),
                id: record.key.id.clone(),
                version: record.position,
            },
            topic,
            major,
            requires_metadata,
        }
    }
}
