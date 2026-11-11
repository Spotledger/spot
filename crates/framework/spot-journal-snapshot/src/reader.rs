//! The snapshot reader: opens the signed index, then streams images part by part, verifying each
//! part's hash and shape before a single image of it is handed out.

use std::collections::VecDeque;

use spot_journal_store::{JournalStore, Layout, etag};

use crate::error::SnapshotError;
use crate::index::{INDEX_NAME, PartEntry, SignedIndex};
use crate::record::ImageRecord;
use crate::sign::{IdentityTransform, PartTransform, SnapshotKeys};

/// Reads the index of `(tenant, generation, position)` and verifies it; also returns the index
/// object's SHA-256 (the etag).
pub(crate) async fn read_index(
    store: &dyn JournalStore,
    tenant: &str,
    generation: u64,
    position: u64,
    keys: &dyn SnapshotKeys,
) -> Result<(SignedIndex, String), SnapshotError> {
    let key = Layout::snapshot_object(tenant, generation, position, INDEX_NAME)?.to_key();
    let object = store.get(&key).await?.ok_or(SnapshotError::IndexMissing)?;
    let index = SignedIndex::open(&object.bytes, tenant, generation, position, keys)?;
    Ok((index, object.meta.etag))
}

/// An opened snapshot whose index signature has been verified.
pub struct SnapshotReader<'a> {
    store: &'a dyn JournalStore,
    transform: &'a dyn PartTransform,
    index: SignedIndex,
}

impl<'a> SnapshotReader<'a> {
    /// Opens a snapshot: fetches the index, checks that it belongs to this tenant, generation and
    /// position, and verifies its signature. Parts are verified as they are streamed.
    ///
    /// # Errors
    /// [`SnapshotError::IndexMissing`] for an incomplete snapshot, [`SnapshotError::IndexMisplaced`],
    /// [`SnapshotError::SignatureInvalid`], [`SnapshotError::IndexInvalid`] or a store failure.
    pub async fn open(
        store: &'a dyn JournalStore,
        tenant: &str,
        generation: u64,
        position: u64,
        keys: &dyn SnapshotKeys,
    ) -> Result<SnapshotReader<'a>, SnapshotError> {
        let (index, _) = read_index(store, tenant, generation, position, keys).await?;
        Ok(Self {
            store,
            transform: &IdentityTransform,
            index,
        })
    }

    /// Sets the part transform (must match the writer's; the default is the identity).
    #[must_use]
    pub fn with_transform(mut self, transform: &'a dyn PartTransform) -> Self {
        self.transform = transform;
        self
    }

    /// The verified index.
    #[must_use]
    pub fn index(&self) -> &SignedIndex {
        &self.index
    }

    /// The images in stored order, verified part by part.
    #[must_use]
    pub fn images(&self) -> ImageStream<'_, 'a> {
        ImageStream {
            reader: self,
            next_part: 0,
            current: VecDeque::new(),
            last: None,
        }
    }

    async fn load_part(&self, entry: &PartEntry) -> Result<Vec<ImageRecord>, SnapshotError> {
        let body = &self.index.body;
        let key =
            Layout::snapshot_object(&body.tenant, body.generation, body.position, &entry.name)?
                .to_key();
        let object = self
            .store
            .get(&key)
            .await?
            .ok_or_else(|| SnapshotError::part(&entry.name, "the part is missing"))?;
        if etag(&object.bytes) != entry.sha256 {
            return Err(SnapshotError::part(
                &entry.name,
                "the hash differs from the index",
            ));
        }
        let compressed = self.transform.reverse(object.bytes)?;
        let capacity = usize::try_from(entry.plain_bytes)
            .map_err(|_| SnapshotError::part(&entry.name, "the size does not fit memory"))?;
        let plain = zstd::bulk::decompress(&compressed, capacity)
            .map_err(|_| SnapshotError::part(&entry.name, "the part does not decompress"))?;
        if plain.len() != capacity {
            return Err(SnapshotError::part(
                &entry.name,
                "the size differs from the index",
            ));
        }
        let mut images = Vec::new();
        for line in plain.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let record: ImageRecord = serde_json::from_slice(line)
                .map_err(|_| SnapshotError::part(&entry.name, "an image line does not parse"))?;
            images.push(record);
        }
        if u64::try_from(images.len()).ok() != Some(entry.images) {
            return Err(SnapshotError::part(
                &entry.name,
                "the image count differs from the index",
            ));
        }
        Ok(images)
    }
}

/// A stream of verified images; call [`ImageStream::next`] until it returns `None`.
pub struct ImageStream<'r, 'a> {
    reader: &'r SnapshotReader<'a>,
    next_part: usize,
    current: VecDeque<ImageRecord>,
    last: Option<(String, String)>,
}

impl ImageStream<'_, '_> {
    /// The next image, or `None` at the end.
    ///
    /// # Errors
    /// [`SnapshotError::PartInvalid`] naming the part when it is missing, altered or out of order.
    pub async fn next(&mut self) -> Result<Option<ImageRecord>, SnapshotError> {
        loop {
            if let Some(record) = self.current.pop_front() {
                let key = (record.entity.clone(), record.id.clone());
                if self.last.as_ref().is_some_and(|last| *last >= key) {
                    let name = self.reader.index.body.parts[self.next_part - 1]
                        .name
                        .clone();
                    return Err(SnapshotError::part(&name, "the images are not in order"));
                }
                self.last = Some(key);
                return Ok(Some(record));
            }
            let Some(entry) = self.reader.index.body.parts.get(self.next_part) else {
                return Ok(None);
            };
            self.current = self.reader.load_part(entry).await?.into();
            self.next_part += 1;
        }
    }
}

/// Reads every part of a snapshot, verifying hashes and the order, and returns the image count.
///
/// # Errors
/// Whatever [`SnapshotReader::open`] or [`ImageStream::next`] report.
pub async fn verify_deep(
    store: &dyn JournalStore,
    tenant: &str,
    generation: u64,
    position: u64,
    keys: &dyn SnapshotKeys,
) -> Result<u64, SnapshotError> {
    let reader = SnapshotReader::open(store, tenant, generation, position, keys).await?;
    let mut stream = reader.images();
    let mut count = 0_u64;
    while stream.next().await?.is_some() {
        count += 1;
    }
    if count != reader.index.body.total_images {
        return Err(SnapshotError::IndexInvalid(
            "the images read differ from the index",
        ));
    }
    Ok(count)
}
