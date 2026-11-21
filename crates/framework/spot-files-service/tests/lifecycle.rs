//! The attach lifecycle, orphan purging and the crash-between-writes invariant.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_core::{ManualClock, SiteId};
use spot_files::error::FileError;
use spot_files::key::{FileKey, FileKeyPrefix};
use spot_files::memory::MemoryFileStore;
use spot_files::store::{
    Body, FileFuture, FileMeta, FileResult, FileStore, PutOptions, StoredFile,
};
use spot_files_service::{
    AttachTarget, Caller, FileCtx, FileRecordStore, FileServiceError, FileState, FilesApi,
    FilesService, MemoryFileRecordStore, TableAttachAccess, TokenCallers, UploadRequest,
};
use spot_storage_contract::RecordId;
use support::{
    DEFAULT_ACCESS, Fixture, UPLOADER, must, must_some, read_body, small_policy, upload_ok,
};
use time::{Duration, OffsetDateTime};

fn field_attachment() -> Vec<(String, RecordId)> {
    Vec::new()
}

#[tokio::test]
async fn attaching_marks_attached_and_replacing_orphans_the_old_file() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let a = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("a.bin"),
        b"aaaa",
    )
    .await;
    let b = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("b.bin"),
        b"bbbb",
    )
    .await;

    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "note",
                fixture.doc,
                &[("attachment".to_owned(), a.id)],
            )
            .await,
    );
    let record_a = must_some(must(fixture.records.get(&fixture.site, a.id).await));
    assert_eq!(record_a.state, FileState::Attached);
    assert!(!record_a.private);
    assert_eq!(
        record_a.attached_to,
        Some(AttachTarget::field("note", fixture.doc, "attachment"))
    );

    // Replacing the value attaches `b` and orphans `a`.
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "note",
                fixture.doc,
                &[("attachment".to_owned(), b.id)],
            )
            .await,
    );
    assert_eq!(
        must_some(must(fixture.records.get(&fixture.site, a.id).await)).state,
        FileState::Orphaned
    );
    assert_eq!(
        must_some(must(fixture.records.get(&fixture.site, b.id).await)).state,
        FileState::Attached
    );
}

#[tokio::test]
async fn clearing_the_value_orphans_the_file() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let a = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("a.bin"),
        b"aaaa",
    )
    .await;
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "note",
                fixture.doc,
                &[("attachment".to_owned(), a.id)],
            )
            .await,
    );
    let empty: Vec<(String, RecordId)> = field_attachment();
    must(
        fixture
            .service
            .attach_saved(&fixture.site, "note", fixture.doc, &empty)
            .await,
    );
    assert_eq!(
        must_some(must(fixture.records.get(&fixture.site, a.id).await)).state,
        FileState::Orphaned
    );
}

#[tokio::test]
async fn deleting_the_document_orphans_its_files() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let a = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("a.bin"),
        b"aaaa",
    )
    .await;
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "note",
                fixture.doc,
                &[("attachment".to_owned(), a.id)],
            )
            .await,
    );
    must(
        fixture
            .service
            .attach_deleted(&fixture.site, "note", fixture.doc)
            .await,
    );
    assert_eq!(
        must_some(must(fixture.records.get(&fixture.site, a.id).await)).state,
        FileState::Orphaned
    );
}

#[tokio::test]
async fn purge_never_removes_an_attached_file() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let a = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("a.bin"),
        b"aaaa",
    )
    .await;
    let b = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("b.bin"),
        b"bbbb",
    )
    .await;
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "note",
                fixture.doc,
                &[("attachment".to_owned(), a.id)],
            )
            .await,
    );
    // Replacing orphans `a`; `b` is attached.
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "note",
                fixture.doc,
                &[("attachment".to_owned(), b.id)],
            )
            .await,
    );

    let future = OffsetDateTime::UNIX_EPOCH + Duration::days(100_000);
    let report = must(
        fixture
            .service
            .purge_orphans(&fixture.site, future, 100)
            .await,
    );
    assert_eq!(report.purged, 1, "{report:?}");

    // `a` is gone; `b` and its bytes survive.
    assert!(must(fixture.records.get(&fixture.site, a.id).await).is_none());
    let record_b = must_some(must(fixture.records.get(&fixture.site, b.id).await));
    assert_eq!(record_b.state, FileState::Attached);
    assert!(must(fixture.store.head(&fixture.site, record_b.key).await).is_some());
}

#[tokio::test]
async fn purge_leaves_recent_orphans_alone() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let a = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("a.bin"),
        b"aaaa",
    )
    .await;
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "note",
                fixture.doc,
                &[("attachment".to_owned(), a.id)],
            )
            .await,
    );
    must(
        fixture
            .service
            .attach_deleted(&fixture.site, "note", fixture.doc)
            .await,
    );
    // The threshold is before the file was created, so nothing is purged.
    let past = OffsetDateTime::UNIX_EPOCH + Duration::days(1);
    let report = must(
        fixture
            .service
            .purge_orphans(&fixture.site, past, 100)
            .await,
    );
    assert_eq!(report.purged, 0);
    assert!(must(fixture.records.get(&fixture.site, a.id).await).is_some());
}

#[tokio::test]
async fn purge_sweeps_bytes_with_no_record() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    // Simulate a crash between the byte write and the record write: bytes exist, no record.
    let id = RecordId::new_v7();
    let created = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
    let key = must(spot_files_service::storage_key(id, created));
    must(
        fixture
            .store
            .put(
                &fixture.site,
                key.clone(),
                spot_files::store::body_from_vec(b"orphan".to_vec()),
                PutOptions::new("application/octet-stream", 4096, false),
            )
            .await,
    );
    assert!(must(fixture.store.head(&fixture.site, key.clone()).await).is_some());

    let future = OffsetDateTime::UNIX_EPOCH + Duration::days(100_000);
    let report = must(
        fixture
            .service
            .purge_orphans(&fixture.site, future, 100)
            .await,
    );
    assert_eq!(report.swept, 1, "{report:?}");
    assert!(must(fixture.store.head(&fixture.site, key).await).is_none());
}

/// A `FileStore` whose `delete` can be made to fail, to model a process that died before it
/// could clean up after a failed record write.
struct FlakyStore {
    inner: Arc<MemoryFileStore>,
    fail_delete: AtomicBool,
}

impl FlakyStore {
    fn new(inner: Arc<MemoryFileStore>) -> Self {
        FlakyStore {
            inner,
            fail_delete: AtomicBool::new(true),
        }
    }

    fn allow_deletes(&self) {
        self.fail_delete.store(false, Ordering::SeqCst);
    }
}

impl FileStore for FlakyStore {
    fn put<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
        body: Body,
        opts: PutOptions,
    ) -> FileFuture<'a, FileResult<StoredFile>> {
        self.inner.put(site, key, body, opts)
    }

    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<(FileMeta, Body)>> {
        self.inner.get(site, key)
    }

    fn head<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<Option<FileMeta>>> {
        self.inner.head(site, key)
    }

    fn delete<'a>(&'a self, site: &'a SiteId, key: FileKey) -> FileFuture<'a, FileResult<bool>> {
        if self.fail_delete.load(Ordering::SeqCst) {
            return Box::pin(async { Err(FileError::io("simulated crash: delete refused")) });
        }
        self.inner.delete(site, key)
    }

    fn list<'a>(
        &'a self,
        site: &'a SiteId,
        prefix: &'a FileKeyPrefix,
        after: Option<&'a FileKey>,
        limit: usize,
    ) -> FileFuture<'a, FileResult<Vec<FileMeta>>> {
        self.inner.list(site, prefix, after, limit)
    }
}

/// A record store whose `insert` always fails, to model the record write failing after the
/// bytes are stored.
struct FailingRecordStore {
    inner: Arc<MemoryFileRecordStore>,
}

impl FileRecordStore for FailingRecordStore {
    fn insert(
        &self,
        _record: spot_files_service::FileRecord,
    ) -> spot_files_service::BoxFuture<'_, Result<(), FileServiceError>> {
        Box::pin(async {
            Err(FileServiceError::store(
                "simulated crash: record write failed",
            ))
        })
    }

    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> spot_files_service::BoxFuture<
        'a,
        Result<Option<spot_files_service::FileRecord>, FileServiceError>,
    > {
        self.inner.get(site, id)
    }

    fn update(
        &self,
        record: spot_files_service::FileRecord,
    ) -> spot_files_service::BoxFuture<'_, Result<(), FileServiceError>> {
        self.inner.update(record)
    }

    fn delete<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> spot_files_service::BoxFuture<'a, Result<bool, FileServiceError>> {
        self.inner.delete(site, id)
    }

    fn orphans<'a>(
        &'a self,
        site: &'a SiteId,
        older_than: OffsetDateTime,
        limit: usize,
    ) -> spot_files_service::BoxFuture<
        'a,
        Result<Vec<spot_files_service::FileRecord>, FileServiceError>,
    > {
        self.inner.orphans(site, older_than, limit)
    }

    fn by_target<'a>(
        &'a self,
        site: &'a SiteId,
        target: &'a AttachTarget,
    ) -> spot_files_service::BoxFuture<
        'a,
        Result<Vec<spot_files_service::FileRecord>, FileServiceError>,
    > {
        self.inner.by_target(site, target)
    }

    fn by_document<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
    ) -> spot_files_service::BoxFuture<
        'a,
        Result<Vec<spot_files_service::FileRecord>, FileServiceError>,
    > {
        self.inner.by_document(site, doctype, id)
    }

    fn known_keys<'a>(
        &'a self,
        site: &'a SiteId,
    ) -> spot_files_service::BoxFuture<'a, Result<Vec<FileKey>, FileServiceError>> {
        self.inner.known_keys(site)
    }
}

#[tokio::test]
async fn a_crash_between_the_byte_and_record_writes_leaves_a_purgeable_orphan() {
    let site = SiteId::new(support::SITE).unwrap_or_else(|e| panic!("site: {e}"));
    let uploader = RecordId::new_v7();
    let store = Arc::new(FlakyStore::new(Arc::new(MemoryFileStore::new())));
    let records = Arc::new(FailingRecordStore {
        inner: Arc::new(MemoryFileRecordStore::new()),
    });
    let callers = TokenCallers::new().with(UPLOADER, Caller::User(uploader));
    let access =
        TableAttachAccess::from_toml(DEFAULT_ACCESS).unwrap_or_else(|e| panic!("access: {e}"));
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000),
    ));
    let service = FilesService::new(
        store.clone(),
        records.clone(),
        Arc::new(callers),
        Arc::new(access),
        small_policy(),
        clock,
    );
    let ctx = FileCtx::guest(site.clone()).with_credential(UPLOADER);

    // The record write fails, and the cleanup's byte delete fails too (the crash).
    let error = match service
        .upload(
            &ctx,
            UploadRequest::new("x.bin"),
            spot_files::store::body_from_vec(b"hello".to_vec()),
        )
        .await
    {
        Ok(_) => panic!("expected the record write to fail"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "file.store_failed");
    // No record exists, and the bytes remain as a purgeable orphan.
    assert!(must(records.known_keys(&site).await).is_empty());
    let listed = must(store.list(&site, &FileKeyPrefix::all(), None, 100).await);
    assert_eq!(listed.len(), 1, "the crashed upload left no orphan");

    // Recovery: allow deletes and purge; the orphan is swept and nothing dangles.
    store.allow_deletes();
    let future = OffsetDateTime::UNIX_EPOCH + Duration::days(100_000);
    let report = must(service.purge_orphans(&site, future, 100).await);
    assert_eq!(report.swept, 1, "{report:?}");
    assert!(must(store.list(&site, &FileKeyPrefix::all(), None, 100).await).is_empty());
    assert!(must(records.known_keys(&site).await).is_empty());
}

/// A tiny deterministic generator, so the property test never flakes and needs no dependency.
struct Lcg(u64);

impl Lcg {
    fn below(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n
    }
}

#[tokio::test]
async fn random_attach_replace_delete_sequences_keep_every_attached_file() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let files: Vec<RecordId> = {
        let mut ids = Vec::new();
        for index in 0..3 {
            let view = upload_ok(
                &fixture,
                Some(UPLOADER),
                UploadRequest::new(format!("f{index}.bin")),
                format!("payload-{index}").as_bytes(),
            )
            .await;
            ids.push(view.id);
        }
        ids
    };
    let future = OffsetDateTime::UNIX_EPOCH + Duration::days(100_000);
    let empty: Vec<(String, RecordId)> = Vec::new();
    let mut rng = Lcg(0x5eed);
    let mut attached: Option<RecordId> = None;

    for _ in 0..200 {
        match rng.below(5) {
            0 | 1 => {
                // Attach (or replace with) a random file that still has a record: the engine
                // only ever passes ids that exist, and a purged file cannot be attached.
                let mut alive = Vec::new();
                for id in &files {
                    if must(fixture.records.get(&fixture.site, *id).await).is_some() {
                        alive.push(*id);
                    }
                }
                if !alive.is_empty() {
                    let file = alive[rng.below(alive.len())];
                    must(
                        fixture
                            .service
                            .attach_saved(
                                &fixture.site,
                                "note",
                                fixture.doc,
                                &[("attachment".to_owned(), file)],
                            )
                            .await,
                    );
                    attached = Some(file);
                }
            }
            2 => {
                // Clear the field.
                must(
                    fixture
                        .service
                        .attach_saved(&fixture.site, "note", fixture.doc, &empty)
                        .await,
                );
                attached = None;
            }
            3 => {
                // Delete the document.
                must(
                    fixture
                        .service
                        .attach_deleted(&fixture.site, "note", fixture.doc)
                        .await,
                );
                attached = None;
            }
            _ => {
                // Purge.
                must(
                    fixture
                        .service
                        .purge_orphans(&fixture.site, future, 100)
                        .await,
                );
            }
        }

        // Purge after every step and check the invariants.
        must(
            fixture
                .service
                .purge_orphans(&fixture.site, future, 100)
                .await,
        );
        let record_keys = must(fixture.records.known_keys(&fixture.site).await);
        let store_keys = must(
            fixture
                .store
                .list(&fixture.site, &FileKeyPrefix::all(), None, 1000)
                .await,
        )
        .into_iter()
        .map(|meta| meta.key)
        .collect::<Vec<_>>();

        // Never a record pointing at nothing: every record's bytes exist.
        for key in &record_keys {
            assert!(
                must(fixture.store.head(&fixture.site, key.clone()).await).is_some(),
                "a record points at missing bytes"
            );
        }
        // Never dangling bytes after a purge: every stored key has a record.
        for key in &store_keys {
            assert!(
                record_keys.contains(key),
                "purge left unreferenced bytes: {key}"
            );
        }

        // An attached file is always still attached, with its bytes.
        if let Some(file) = attached {
            let record = must_some(must(fixture.records.get(&fixture.site, file).await));
            assert_eq!(record.state, FileState::Attached);
            assert!(
                must(fixture.store.head(&fixture.site, record.key).await).is_some(),
                "purge removed an attached file's bytes"
            );
        }
    }
}

#[tokio::test]
async fn an_attached_file_can_be_downloaded_by_its_reader() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "other",
                fixture.doc,
                &[("plain".to_owned(), view.id)],
            )
            .await,
    );
    let download = support::download_ok(
        &fixture,
        Some(support::READER),
        view.id,
        spot_files_service::DownloadRequest::new(),
    )
    .await;
    assert_eq!(read_body(download).await, b"hello");
}

#[tokio::test]
async fn the_key_recomputed_from_id_and_created_finds_the_bytes_after_attach_flips_private() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    let before = must_some(must(fixture.records.get(&fixture.site, view.id).await));
    assert!(before.private);
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "other",
                fixture.doc,
                &[("plain".to_owned(), view.id)],
            )
            .await,
    );
    let after = must_some(must(fixture.records.get(&fixture.site, view.id).await));
    assert!(!after.private, "attach flips the flag");
    // A record store that does not persist `key` recomputes it from (id, created).
    let recomputed = must(spot_files_service::storage_key(after.id, after.created));
    assert_eq!(recomputed, before.key);
    assert_eq!(recomputed, after.key);
    assert!(
        must(fixture.store.head(&fixture.site, recomputed).await).is_some(),
        "the bytes stay where they were uploaded"
    );
    // Read permission is still the access check's call: a stranger gets nothing.
    let denied = support::download_err(
        &fixture,
        None,
        view.id,
        spot_files_service::DownloadRequest::new(),
    )
    .await;
    let missing = support::download_err(
        &fixture,
        None,
        RecordId::new_v7(),
        spot_files_service::DownloadRequest::new(),
    )
    .await;
    assert_eq!(format!("{denied:?}"), format!("{missing:?}"));
}

#[test]
fn storage_keys_are_always_private_prefixed_and_differ_per_id() {
    let mut rng = Lcg(0x5eed);
    let mut seen = std::collections::HashSet::new();
    for _ in 0..500 {
        let secs = i64::try_from(rng.below(4_000_000_000)).unwrap_or(0) - 1_000_000_000;
        let created = OffsetDateTime::UNIX_EPOCH + Duration::seconds(secs);
        let id = RecordId::new_v7();
        let key = must(spot_files_service::storage_key(id, created));
        let text = key.as_str();
        assert!(text.starts_with("private/"), "{text}");
        assert!(!text.contains(".."), "{text}");
        assert!(text.ends_with(&id.to_string()), "{text}");
        // Deterministic.
        assert_eq!(key, must(spot_files_service::storage_key(id, created)));
        assert!(seen.insert(text.to_owned()), "duplicate key {text}");
    }
}
