//! The `File` record and the store it lives in.
//!
//! A [`FileRecord`] is the row the `file` DocType describes: the display name, the original
//! name, the content type, the size, the SHA-256 digest, whether it is private, what document
//! (and protected field) it is attached to, who uploaded it, when, its lifecycle
//! [`FileState`], a quarantine flag and the **server-generated** [`FileKey`] the bytes live
//! under. The key is never part of [`FileView`], the shape the native API returns, so no
//! response can name a storage path.
//!
//! [`FileRecordStore`] is the seam the host implements over the document engine (the `file`
//! DocType). [`MemoryFileRecordStore`] is the in-process implementation the tests and a
//! single-process host use.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use spot_core::SiteId;
use spot_files::key::FileKey;
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use crate::BoxFuture;
use crate::error::FileServiceError;

/// Where a file is attached: a document of `doctype` with `id`, through the `attach` field
/// `field` when it is a protected field.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AttachTarget {
    /// The owning DocType, a storage name such as `sales_invoice`.
    pub doctype: String,
    /// The owning document's record id.
    pub id: RecordId,
    /// The `attach` field the file is attached through, when the destination named one.
    pub field: Option<String>,
}

impl AttachTarget {
    /// A target on a document, through a named `attach` field.
    pub fn field(doctype: impl Into<String>, id: RecordId, field: impl Into<String>) -> Self {
        AttachTarget {
            doctype: doctype.into(),
            id,
            field: Some(field.into()),
        }
    }

    /// A target on a document as a whole (no single field).
    pub fn document(doctype: impl Into<String>, id: RecordId) -> Self {
        AttachTarget {
            doctype: doctype.into(),
            id,
            field: None,
        }
    }
}

/// A file's lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FileState {
    /// Uploaded and not yet attached to a document.
    Pending,
    /// Attached to a document (through an `attach` field).
    Attached,
    /// Was attached, then the value was replaced, cleared or the document deleted.
    Orphaned,
}

impl FileState {
    /// The state's storage name (`pending`, `attached`, `orphaned`).
    pub fn as_str(self) -> &'static str {
        match self {
            FileState::Pending => "pending",
            FileState::Attached => "attached",
            FileState::Orphaned => "orphaned",
        }
    }

    /// Parses a storage name; `None` when it is not one of the three.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(FileState::Pending),
            "attached" => Some(FileState::Attached),
            "orphaned" => Some(FileState::Orphaned),
            _ => None,
        }
    }
}

/// One file record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    /// The record id, a UUIDv7.
    pub id: RecordId,
    /// The site the file belongs to.
    pub site: SiteId,
    /// The display name: the client's name, sanitised for display.
    pub name: String,
    /// The original file name as the client sent it, kept for audit.
    pub original_name: String,
    /// The stored content type.
    pub content_type: String,
    /// The size in bytes.
    pub size: u64,
    /// The SHA-256 digest of the stored bytes.
    pub digest: [u8; 32],
    /// True while the file is unattached and only its uploader may read it; false when it is
    /// attached to (or will be attached to) a document whose readers see it.
    pub private: bool,
    /// The document and field it is attached to, when attached.
    pub attached_to: Option<AttachTarget>,
    /// The uploader's record id.
    pub uploaded_by: RecordId,
    /// When the record was created (UTC).
    pub created: OffsetDateTime,
    /// The lifecycle state.
    pub state: FileState,
    /// True when the scan seam quarantined the file.
    pub quarantined: bool,
    /// The server-generated storage key. Never leaves this crate's responses.
    pub key: FileKey,
}

impl FileRecord {
    /// The native API's `FileRecord`: the fields a client may see, and nothing else.
    pub fn view(&self) -> FileView {
        FileView {
            id: self.id,
            name: self.name.clone(),
            size: self.size,
            content_type: self.content_type.clone(),
            digest: crate::hex_lower(&self.digest),
            private: self.private,
        }
    }
}

/// The native API's `FileRecord`: `{id, name, size, content_type, digest, private}`. It never
/// names a path, a key or a provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileView {
    /// The record id.
    pub id: RecordId,
    /// The display name.
    pub name: String,
    /// The size in bytes.
    pub size: u64,
    /// The stored content type.
    pub content_type: String,
    /// SHA-256 of the bytes, hex, lower-case.
    pub digest: String,
    /// The private flag (see [`FileRecord::private`]).
    pub private: bool,
}

/// The store of [`FileRecord`]s: the `file` DocType behind the document engine.
///
/// Every method is site-scoped; a missing id is `Ok(None)`, never an error, so the caller
/// decides whether to reveal existence.
pub trait FileRecordStore: Send + Sync + 'static {
    /// Inserts a new record. An id that already exists is replaced (the service generates a
    /// fresh UUIDv7 per upload, so this never happens in practice).
    fn insert(&self, record: FileRecord) -> BoxFuture<'_, Result<(), FileServiceError>>;

    /// One record by id.
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Option<FileRecord>, FileServiceError>>;

    /// Replaces an existing record.
    fn update(&self, record: FileRecord) -> BoxFuture<'_, Result<(), FileServiceError>>;

    /// Removes a record, returning whether one was there.
    fn delete<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> BoxFuture<'a, Result<bool, FileServiceError>>;

    /// Records in state `orphaned` created at or before `older_than`, oldest first, at most
    /// `limit`.
    fn orphans<'a>(
        &'a self,
        site: &'a SiteId,
        older_than: OffsetDateTime,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>>;

    /// Records attached to exactly `target`.
    fn by_target<'a>(
        &'a self,
        site: &'a SiteId,
        target: &'a AttachTarget,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>>;

    /// Records attached to any field of the document `doctype`/`id`.
    fn by_document<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>>;

    /// Every storage key a record still refers to on this site, for the orphan sweep.
    fn known_keys<'a>(
        &'a self,
        site: &'a SiteId,
    ) -> BoxFuture<'a, Result<Vec<FileKey>, FileServiceError>>;
}

/// The in-process [`FileRecordStore`], for tests and single-process hosts.
#[derive(Clone, Debug, Default)]
pub struct MemoryFileRecordStore {
    /// `(site, id) -> record`, kept sorted so listings are deterministic.
    records: Arc<Mutex<BTreeMap<(String, RecordId), FileRecord>>>,
}

impl MemoryFileRecordStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<(String, RecordId), FileRecord>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Every record on `site` whose id matches `filter`, in id order.
    fn scan(&self, site: &SiteId, filter: impl Fn(&FileRecord) -> bool) -> Vec<FileRecord> {
        self.lock()
            .values()
            .filter(|r| &r.site == site && filter(r))
            .cloned()
            .collect()
    }
}

impl FileRecordStore for MemoryFileRecordStore {
    fn insert(&self, record: FileRecord) -> BoxFuture<'_, Result<(), FileServiceError>> {
        Box::pin(async move {
            self.lock()
                .insert((record.site.to_string(), record.id), record);
            Ok(())
        })
    }

    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Option<FileRecord>, FileServiceError>> {
        Box::pin(async move { Ok(self.lock().get(&(site.to_string(), id)).cloned()) })
    }

    fn update(&self, record: FileRecord) -> BoxFuture<'_, Result<(), FileServiceError>> {
        Box::pin(async move {
            self.lock()
                .insert((record.site.to_string(), record.id), record);
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> BoxFuture<'a, Result<bool, FileServiceError>> {
        Box::pin(async move { Ok(self.lock().remove(&(site.to_string(), id)).is_some()) })
    }

    fn orphans<'a>(
        &'a self,
        site: &'a SiteId,
        older_than: OffsetDateTime,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>> {
        Box::pin(async move {
            let mut found = self.scan(site, |r| {
                r.state == FileState::Orphaned && r.created <= older_than
            });
            found.truncate(limit);
            Ok(found)
        })
    }

    fn by_target<'a>(
        &'a self,
        site: &'a SiteId,
        target: &'a AttachTarget,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>> {
        Box::pin(async move {
            Ok(self.scan(site, |r| {
                r.state == FileState::Attached && r.attached_to.as_ref() == Some(target)
            }))
        })
    }

    fn by_document<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>> {
        Box::pin(async move {
            Ok(self.scan(site, |r| {
                r.attached_to
                    .as_ref()
                    .is_some_and(|t| t.doctype == doctype && t.id == id)
            }))
        })
    }

    fn known_keys<'a>(
        &'a self,
        site: &'a SiteId,
    ) -> BoxFuture<'a, Result<Vec<FileKey>, FileServiceError>> {
        Box::pin(async move {
            Ok(self
                .scan(site, |_| true)
                .into_iter()
                .map(|r| r.key)
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AttachTarget, FileRecord, FileState, FileView, MemoryFileRecordStore};
    use crate::record::FileRecordStore;
    use spot_core::SiteId;
    use spot_files::key::{FileKey, Visibility};
    use spot_storage_contract::RecordId;

    fn record(state: FileState, target: Option<AttachTarget>) -> FileRecord {
        let id = RecordId::new_v7();
        FileRecord {
            id,
            site: SiteId::new("files.test").unwrap_or_else(|e| panic!("site: {e:?}")),
            name: "spec.pdf".to_owned(),
            original_name: "spec.pdf".to_owned(),
            content_type: "application/pdf".to_owned(),
            size: 3,
            digest: [7u8; 32],
            private: true,
            attached_to: target,
            uploaded_by: RecordId::new_v7(),
            created: time::OffsetDateTime::UNIX_EPOCH,
            state,
            quarantined: false,
            key: FileKey::new(Visibility::Private, "2026/10/spec.pdf")
                .unwrap_or_else(|e| panic!("key: {e:?}")),
        }
    }

    #[test]
    fn file_state_round_trips_its_names() {
        for state in [FileState::Pending, FileState::Attached, FileState::Orphaned] {
            assert_eq!(FileState::parse(state.as_str()), Some(state));
        }
        assert_eq!(FileState::parse("gone"), None);
    }

    #[test]
    fn view_hides_the_key() {
        let record = record(FileState::Pending, None);
        let view: FileView = record.view();
        assert_eq!(view.name, record.name);
        assert_eq!(view.digest.len(), 64);
        assert!(view.digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!view.digest.contains('/'));
    }

    #[tokio::test]
    async fn memory_store_round_trips_and_scans() {
        let store = MemoryFileRecordStore::new();
        let site = SiteId::new("files.test").unwrap_or_else(|e| panic!("site: {e:?}"));
        let doc = RecordId::new_v7();
        let record = record(
            FileState::Attached,
            Some(AttachTarget::document("note", doc)),
        );
        let id = record.id;
        store
            .insert(record.clone())
            .await
            .unwrap_or_else(|e| panic!("insert: {e:?}"));
        let loaded = store
            .get(&site, id)
            .await
            .unwrap_or_else(|e| panic!("get: {e:?}"));
        assert_eq!(loaded.as_ref(), Some(&record));
        let by_doc = store
            .by_document(&site, "note", doc)
            .await
            .unwrap_or_else(|e| panic!("by_document: {e:?}"));
        assert_eq!(by_doc.len(), 1);
        assert_eq!(
            store
                .known_keys(&site)
                .await
                .unwrap_or_else(|e| panic!("keys: {e:?}"))
                .len(),
            1
        );
        assert!(
            store
                .delete(&site, id)
                .await
                .unwrap_or_else(|e| panic!("delete: {e:?}"))
        );
        assert!(
            store
                .get(&site, id)
                .await
                .unwrap_or_else(|e| panic!("get: {e:?}"))
                .is_none()
        );
    }
}
