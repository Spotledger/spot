//! The database-backed [`VersionStore`] (feature `storage`).
//!
//! A multi-node deployment keeps each site's metadata version and content hash in one reserved
//! document record, so every node can reconcile from the same committed state (ADR-20). This
//! module is the storage-contract implementation of that record: it reads and writes a single
//! document through the contract's document ops, on a fixed [`RecordId`], inside a short
//! transaction.
//!
//! The record lives in an entity named [`META_ENTITY`] with two fields, `snapshot_version` and
//! `content_hash`. (`version` itself is a reserved system column, so the metadata version is
//! carried in a normal field; the record's *storage* version is the contract's optimistic
//! concurrency token, not the site's metadata version.) The host must include this entity in the
//! site catalog it prepares — [`StorageVersionStore::catalog`] builds a catalog containing just
//! it, and [`StorageVersionStore::entity_def`] the entity to merge into the site's own.
//!
//! Reading definitions by version is the loader's job (`MetaLoader`), not this store's; the
//! definitions are persisted by the metadata-persistence task, not here.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, EntityDef, EntityKind, FieldDef, FieldType,
    Isolation, Name, RecordId, RowLock, StorageBackend, StorageError, StorageResult, Transaction,
    TxOptions, Value,
};

use crate::error::LiveError;
use crate::polling::{StoreFuture, VersionStore};
use crate::snapshot::{MetaHash, Version};

/// The entity holding the reserved version record.
pub const META_ENTITY: &str = "meta_version";

/// The field holding the site's metadata version.
const FIELD_VERSION: &str = "snapshot_version";

/// The field holding the content hash of the metadata at that version.
const FIELD_HASH: &str = "content_hash";

/// The fixed UUIDv7 identifying the one reserved record of every site's catalog. It is a
/// constant so every node and every restart addresses the same row.
const META_RECORD_BYTES: [u8; 16] = [
    0x01, 0x8f, 0x4e, 0x1c, 0x00, 0x00, 0x70, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
];

/// A [`VersionStore`] over any storage backend's document ops.
///
/// Generic over the backend so the in-memory fake, PostgreSQL and later backends share it. The
/// host owns preparing [`storage`](StorageVersionStore::catalog) on the backend.
pub struct StorageVersionStore<B: StorageBackend> {
    backend: Arc<B>,
    catalog: Arc<Catalog>,
    entity: Name,
    record: RecordId,
    version_field: Name,
    hash_field: Name,
    tx_read: Name,
    tx_write: Name,
}

impl<B: StorageBackend> StorageVersionStore<B> {
    /// A store over `backend` and the `catalog` that contains [`META_ENTITY`].
    ///
    /// # Errors
    ///
    /// [`StorageError`] when a reserved name is invalid (they are constants, so only a bug) or
    /// the reserved record id is malformed.
    pub fn new(backend: Arc<B>, catalog: Arc<Catalog>) -> StorageResult<Self> {
        Ok(Self {
            backend,
            catalog,
            entity: Name::new(META_ENTITY)?,
            record: Self::record()?,
            version_field: Name::new(FIELD_VERSION)?,
            hash_field: Name::new(FIELD_HASH)?,
            tx_read: Name::new("meta_version_read")?,
            tx_write: Name::new("meta_version_write")?,
        })
    }

    /// The entity to merge into the site's catalog: the reserved record's two fields.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when a reserved name is invalid (only a bug).
    pub fn entity_def() -> StorageResult<EntityDef> {
        Ok(EntityDef {
            name: Name::new(META_ENTITY)?,
            kind: EntityKind::Document,
            fields: vec![
                FieldDef {
                    name: Name::new(FIELD_VERSION)?,
                    ty: FieldType::Int,
                    nullable: false,
                },
                FieldDef {
                    name: Name::new(FIELD_HASH)?,
                    ty: FieldType::Text {
                        max_chars: Some(64),
                    },
                    nullable: false,
                },
            ],
            children: Vec::new(),
            indexes: Vec::new(),
            exclusions: vec![],
            search: None,
        })
    }

    /// A catalog containing only the reserved record's entity, for tests and early starts; a
    /// real host merges [`StorageVersionStore::entity_def`] into the site catalog instead.
    ///
    /// # Errors
    ///
    /// [`StorageError`] from building the entity or the catalog.
    pub fn minimal_catalog(catalog_version: u64, collation: Collation) -> StorageResult<Catalog> {
        Catalog::new(catalog_version, collation, vec![Self::entity_def()?])
    }

    /// The fixed record id every node addresses.
    ///
    /// # Errors
    ///
    /// [`StorageError`] when the constant is not a UUIDv7 (only a bug).
    pub fn record() -> StorageResult<RecordId> {
        RecordId::from_bytes(META_RECORD_BYTES)
    }

    /// The catalog this store reads and writes through.
    #[must_use]
    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    /// The reserved entity's name.
    #[must_use]
    pub fn entity(&self) -> &Name {
        &self.entity
    }
}

impl<B: StorageBackend> VersionStore for StorageVersionStore<B> {
    fn read<'a>(&'a self) -> StoreFuture<'a, Option<(Version, MetaHash)>> {
        Box::pin(async move {
            let options =
                TxOptions::new(self.tx_read.clone(), Isolation::ReadCommitted).read_only();
            let mut tx = self
                .backend
                .begin(Arc::clone(&self.catalog), options)
                .await
                .map_err(store_error)?;
            let document = tx
                .load(&self.entity, self.record, RowLock::None)
                .await
                .map_err(store_error)?;
            tx.commit().await.map_err(store_error)?;
            let Some(document) = document else {
                return Ok(None);
            };
            let version = read_version(&document.data.fields, &self.version_field)?;
            let hash = read_hash(&document.data.fields, &self.hash_field)?;
            Ok(Some((version, hash)))
        })
    }

    fn write<'a>(&'a self, version: Version, hash: MetaHash) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let options = TxOptions::new(self.tx_write.clone(), Isolation::ReadCommitted);
            let mut tx = self
                .backend
                .begin(Arc::clone(&self.catalog), options)
                .await
                .map_err(store_error)?;
            let existing = tx
                .load(&self.entity, self.record, RowLock::None)
                .await
                .map_err(store_error)?;
            let mut fields = BTreeMap::new();
            fields.insert(
                self.version_field.clone(),
                Value::Int(version_to_i64(version)?),
            );
            fields.insert(
                self.hash_field.clone(),
                Value::Text(hash.as_hex().to_owned()),
            );
            let data = DocumentData {
                entity: self.entity.clone(),
                id: self.record,
                fields,
                children: BTreeMap::new(),
            };
            match existing {
                Some(document) => {
                    tx.update(&data, document.version)
                        .await
                        .map_err(store_error)?;
                }
                None => {
                    tx.insert(&data).await.map_err(store_error)?;
                }
            }
            tx.commit().await.map_err(store_error)?;
            Ok(())
        })
    }
}

impl<B: StorageBackend> std::fmt::Debug for StorageVersionStore<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageVersionStore")
            .field("entity", &self.entity)
            .finish_non_exhaustive()
    }
}

fn read_version(fields: &spot_storage_contract::Fields, name: &Name) -> Result<Version, LiveError> {
    match fields.get(name) {
        Some(Value::Int(raw)) => u64::try_from(*raw)
            .map(Version::new)
            .map_err(|_| store_message(format!("stored metadata version {raw} is negative"))),
        Some(other) => Err(store_message(format!(
            "the stored metadata version is {other:?}, not an integer"
        ))),
        None => Err(store_message(
            "the reserved version record has no metadata version field".to_owned(),
        )),
    }
}

fn read_hash(fields: &spot_storage_contract::Fields, name: &Name) -> Result<MetaHash, LiveError> {
    match fields.get(name) {
        Some(Value::Text(hex)) => Ok(MetaHash::from_hex(hex.clone())),
        Some(other) => Err(store_message(format!(
            "the stored metadata hash is {other:?}, not text"
        ))),
        None => Err(store_message(
            "the reserved version record has no content hash field".to_owned(),
        )),
    }
}

fn version_to_i64(version: Version) -> Result<i64, LiveError> {
    i64::try_from(version.get()).map_err(|_| {
        store_message(format!(
            "metadata version {version} does not fit the stored integer"
        ))
    })
}

fn store_error(error: StorageError) -> LiveError {
    store_message(error.to_string())
}

fn store_message(message: String) -> LiveError {
    LiveError::Store { message }
}
