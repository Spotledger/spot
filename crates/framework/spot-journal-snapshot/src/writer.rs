//! The snapshot writer: images in, zstd parts and a signed index out, the index last.

use std::collections::BTreeMap;

use spot_journal_rebuild_plan::SnapshotInfo;
use spot_journal_store::{JournalStore, Layout, StoreError, etag};

use crate::error::SnapshotError;
use crate::index::{FORMAT, INDEX_NAME, IndexBody, PartEntry, sign_index};
use crate::record::ImageRecord;
use crate::sign::{IdentityTransform, PartTransform, SnapshotSigner};

/// The default part size: 64 MB of uncompressed JSON lines.
pub const DEFAULT_PART_BYTES: usize = 64 * 1024 * 1024;
const ZSTD_LEVEL: i32 = 3;

/// What identifies a snapshot, all of it supplied by the caller (no clock, no randomness).
#[derive(Clone, Debug)]
pub struct SnapshotParams {
    /// The tenant that owns the images.
    pub tenant: String,
    /// The stream generation the position belongs to.
    pub generation: u64,
    /// The position every image is as of.
    pub position: u64,
    /// Lower-case hex hash of the manifest the database read was taken against.
    pub manifest_hash: String,
    /// When the snapshot was taken, milliseconds since the Unix epoch.
    pub taken_at_millis: i64,
}

/// Stores `bytes` under `key`; an identical object already there is accepted, so a rerun after a
/// crash (or a second node writing the same snapshot) converges instead of failing.
pub(crate) async fn put_idempotent(
    store: &dyn JournalStore,
    key: &str,
    bytes: Vec<u8>,
) -> Result<(), SnapshotError> {
    match store.put_new(key, bytes.clone()).await {
        Ok(_) => Ok(()),
        Err(StoreError::AlreadyExists { .. }) => match store.get(key).await? {
            Some(existing) if existing.bytes == bytes => Ok(()),
            _ => Err(SnapshotError::Exists {
                key: key.to_owned(),
            }),
        },
        Err(other) => Err(other.into()),
    }
}

/// Writes one snapshot. Parts are stored as they fill; [`SnapshotWriter::finish`] stores the index
/// last, so a writer dropped before `finish` leaves a snapshot nothing lists.
pub struct SnapshotWriter<'a> {
    store: &'a dyn JournalStore,
    signer: &'a dyn SnapshotSigner,
    transform: &'a dyn PartTransform,
    params: SnapshotParams,
    part_bytes: usize,
    expected_count: Option<u64>,
    buffer: Vec<u8>,
    buffer_images: u64,
    parts: Vec<PartEntry>,
    last: Option<(String, String)>,
    entity_counts: BTreeMap<String, u64>,
    entity_schemas: BTreeMap<String, Vec<u64>>,
    total: u64,
}

impl<'a> SnapshotWriter<'a> {
    /// Starts a snapshot.
    ///
    /// # Errors
    /// [`SnapshotError::ParamsInvalid`] for an unusable tenant or manifest hash.
    pub fn new(
        store: &'a dyn JournalStore,
        params: SnapshotParams,
        signer: &'a dyn SnapshotSigner,
    ) -> Result<Self, SnapshotError> {
        Layout::snapshot(&params.tenant, params.generation, params.position)
            .map_err(|_| SnapshotError::ParamsInvalid("the tenant is not a valid tenant id"))?;
        if !crate::index::is_hash(&params.manifest_hash) {
            return Err(SnapshotError::ParamsInvalid(
                "the manifest hash is not 64 lower-case hex digits",
            ));
        }
        Ok(Self {
            store,
            signer,
            transform: &IdentityTransform,
            params,
            part_bytes: DEFAULT_PART_BYTES,
            expected_count: None,
            buffer: Vec::new(),
            buffer_images: 0,
            parts: Vec::new(),
            last: None,
            entity_counts: BTreeMap::new(),
            entity_schemas: BTreeMap::new(),
            total: 0,
        })
    }

    /// Sets the part size (uncompressed bytes); a single larger image gets a part of its own.
    ///
    /// # Errors
    /// [`SnapshotError::ParamsInvalid`] for zero.
    pub fn with_part_bytes(mut self, part_bytes: usize) -> Result<Self, SnapshotError> {
        if part_bytes == 0 {
            return Err(SnapshotError::ParamsInvalid("the part size is zero"));
        }
        self.part_bytes = part_bytes;
        Ok(self)
    }

    /// Sets the image count the database reported; `finish` fails when the images differ.
    #[must_use]
    pub fn with_expected_count(mut self, expected: u64) -> Self {
        self.expected_count = Some(expected);
        self
    }

    /// Sets the part transform (the default is the identity).
    #[must_use]
    pub fn with_transform(mut self, transform: &'a dyn PartTransform) -> Self {
        self.transform = transform;
        self
    }

    /// Adds a batch of images. The whole batch is checked (non-empty keys, strictly ascending
    /// after everything already written) **before** any of it is accepted or any object written.
    ///
    /// # Errors
    /// [`SnapshotError::ImageInvalid`], [`SnapshotError::InputNotSorted`] or a store failure.
    pub async fn write_part(
        &mut self,
        images: impl IntoIterator<Item = ImageRecord>,
    ) -> Result<(), SnapshotError> {
        let mut lines = Vec::new();
        let mut previous = self.last.clone();
        for (ordinal, record) in (1_u64..).zip(images) {
            if record.entity.is_empty() || record.id.is_empty() {
                return Err(SnapshotError::ImageInvalid { ordinal });
            }
            let key = (record.entity.clone(), record.id.clone());
            if previous.as_ref().is_some_and(|p| *p >= key) {
                return Err(SnapshotError::InputNotSorted { ordinal });
            }
            let mut line =
                serde_json::to_vec(&record).map_err(|_| SnapshotError::ImageInvalid { ordinal })?;
            line.push(b'\n');
            previous = Some(key);
            lines.push((record, line));
        }
        for (record, line) in lines {
            if !self.buffer.is_empty() && self.buffer.len() + line.len() > self.part_bytes {
                self.flush().await?;
            }
            self.buffer.extend_from_slice(&line);
            self.buffer_images += 1;
            self.total += 1;
            *self.entity_counts.entry(record.entity.clone()).or_insert(0) += 1;
            let schemas = self
                .entity_schemas
                .entry(record.entity.clone())
                .or_default();
            if let Err(at) = schemas.binary_search(&record.entity_schema) {
                schemas.insert(at, record.entity_schema);
            }
            self.last = Some((record.entity, record.id));
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), SnapshotError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let plain = std::mem::take(&mut self.buffer);
        let compressed = zstd::bulk::compress(&plain, ZSTD_LEVEL)
            .map_err(|e| SnapshotError::TransformFailed(e.to_string()))?;
        let stored = self.transform.forward(compressed)?;
        let name = format!("part-{:08}.jsonl.zst", self.parts.len() + 1);
        let key = Layout::snapshot_object(
            &self.params.tenant,
            self.params.generation,
            self.params.position,
            &name,
        )?
        .to_key();
        let entry = PartEntry {
            name,
            sha256: etag(&stored),
            images: self.buffer_images,
            plain_bytes: u64::try_from(plain.len()).unwrap_or(u64::MAX),
            stored_bytes: u64::try_from(stored.len()).unwrap_or(u64::MAX),
        };
        put_idempotent(self.store, &key, stored).await?;
        self.parts.push(entry);
        self.buffer_images = 0;
        Ok(())
    }

    /// Stores the last part and then the signed index, and returns the snapshot as the rebuild
    /// planner takes it (`verified` is false: only the rebuild test marks a snapshot verified).
    ///
    /// # Errors
    /// [`SnapshotError::CountMismatch`] when an expected count was set and differs (nothing but
    /// parts is written then), or a store failure.
    pub async fn finish(mut self) -> Result<SnapshotInfo, SnapshotError> {
        if let Some(expected) = self.expected_count
            && expected != self.total
        {
            return Err(SnapshotError::CountMismatch {
                expected,
                actual: self.total,
            });
        }
        self.flush().await?;
        let body = IndexBody {
            format: FORMAT,
            tenant: self.params.tenant.clone(),
            generation: self.params.generation,
            position: self.params.position,
            taken_at_millis: self.params.taken_at_millis,
            manifest_hash: self.params.manifest_hash.clone(),
            total_images: self.total,
            parts: self.parts,
            entity_counts: self.entity_counts,
            entity_schemas: self.entity_schemas,
        };
        let index = sign_index(body, self.signer)?;
        let key = Layout::snapshot_object(
            &self.params.tenant,
            self.params.generation,
            self.params.position,
            INDEX_NAME,
        )?
        .to_key();
        put_idempotent(self.store, &key, index.to_bytes()?).await?;
        Ok(index.info(false))
    }
}
