//! A storage `Unavailable` on upload, download or the metadata write answers
//! `server.unavailable` (503, retryable, with its wait), not `file.store_failed` or `http.internal`;
//! nothing is stored, a later retry with the store back succeeds, and a missing file is still a 404.

#![allow(clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_core::{ManualClock, SiteId};
use spot_files::key::FileKey;
use spot_files::memory::MemoryFileStore;
use spot_files::store::body_from_vec;
use spot_files_service::{
    AttachTarget, BoxFuture, Caller, DownloadRequest, FileCtx, FileRecord, FileRecordStore,
    FileServiceError, FilesApi, FilesService, MemoryFileRecordStore, TableAttachAccess,
    TokenCallers, UploadRequest,
};
use spot_storage_contract::RecordId;
use support::{DEFAULT_ACCESS, UPLOADER, small_policy};
use time::{Duration, OffsetDateTime};

/// A record store that can be switched down: while down, every call is `Unavailable`.
struct Switch {
    inner: MemoryFileRecordStore,
    down: AtomicBool,
}

impl Switch {
    fn check(&self) -> Result<(), FileServiceError> {
        if self.down.load(Ordering::SeqCst) {
            Err(FileServiceError::unavailable(None))
        } else {
            Ok(())
        }
    }
}

impl FileRecordStore for Switch {
    fn insert(&self, record: FileRecord) -> BoxFuture<'_, Result<(), FileServiceError>> {
        match self.check() {
            Ok(()) => self.inner.insert(record),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Option<FileRecord>, FileServiceError>> {
        match self.check() {
            Ok(()) => self.inner.get(site, id),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
    fn update(&self, record: FileRecord) -> BoxFuture<'_, Result<(), FileServiceError>> {
        match self.check() {
            Ok(()) => self.inner.update(record),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
    fn delete<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> BoxFuture<'a, Result<bool, FileServiceError>> {
        match self.check() {
            Ok(()) => self.inner.delete(site, id),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
    fn orphans<'a>(
        &'a self,
        site: &'a SiteId,
        older_than: OffsetDateTime,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>> {
        self.inner.orphans(site, older_than, limit)
    }
    fn by_target<'a>(
        &'a self,
        site: &'a SiteId,
        target: &'a AttachTarget,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>> {
        self.inner.by_target(site, target)
    }
    fn by_document<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Vec<FileRecord>, FileServiceError>> {
        self.inner.by_document(site, doctype, id)
    }
    fn known_keys<'a>(
        &'a self,
        site: &'a SiteId,
    ) -> BoxFuture<'a, Result<Vec<FileKey>, FileServiceError>> {
        self.inner.known_keys(site)
    }
}

struct Rig {
    service: FilesService,
    records: Arc<Switch>,
    ctx: FileCtx,
}

fn rig() -> Rig {
    let site = SiteId::new(support::SITE).unwrap_or_else(|e| panic!("site: {e}"));
    let uploader = RecordId::new_v7();
    let callers = TokenCallers::new().with(UPLOADER, Caller::User(uploader));
    let access =
        TableAttachAccess::from_toml(DEFAULT_ACCESS).unwrap_or_else(|e| panic!("access: {e}"));
    let records = Arc::new(Switch {
        inner: MemoryFileRecordStore::new(),
        down: AtomicBool::new(false),
    });
    let service = FilesService::new(
        Arc::new(MemoryFileStore::new()),
        records.clone(),
        Arc::new(callers),
        Arc::new(access),
        small_policy(),
        Arc::new(ManualClock::new(
            OffsetDateTime::UNIX_EPOCH + Duration::days(20_000),
        )),
    );
    let ctx = FileCtx::guest(site).with_credential(UPLOADER);
    Rig {
        service,
        records,
        ctx,
    }
}

async fn upload(rig: &Rig) -> Result<spot_files_service::FileView, FileServiceError> {
    rig.service
        .upload(
            &rig.ctx,
            UploadRequest::new("a.bin"),
            body_from_vec(b"hello".to_vec()),
        )
        .await
}

#[tokio::test]
async fn an_upload_while_storage_is_down_is_unavailable_and_a_retry_succeeds() {
    let rig = rig();
    rig.records.down.store(true, Ordering::SeqCst);
    let error = upload(&rig).await.expect_err("storage is down");
    assert_eq!(error.code(), "server.unavailable");
    assert_eq!(error.status(), 503);
    assert!(error.retryable());
    assert_eq!(error.retry_after(), Some(1));
    // The message names no path, key or connection.
    assert!(!error.to_string().contains('/'));
    // Nothing was recorded; with the store back the same request succeeds.
    rig.records.down.store(false, Ordering::SeqCst);
    assert!(
        rig.records
            .inner
            .known_keys(&rig.ctx.site)
            .await
            .expect("keys")
            .is_empty()
    );
    let view = upload(&rig).await.expect("retry");
    assert_eq!(view.size, 5);
}

#[tokio::test]
async fn a_download_while_storage_is_down_is_unavailable_but_a_missing_file_is_still_404() {
    let rig = rig();
    let view = upload(&rig).await.expect("upload");
    rig.records.down.store(true, Ordering::SeqCst);
    let error = match rig
        .service
        .download(&rig.ctx, view.id, DownloadRequest::new())
        .await
    {
        Ok(_) => panic!("storage is down"),
        Err(error) => error,
    };
    assert_eq!((error.code(), error.status()), ("server.unavailable", 503));
    rig.records.down.store(false, Ordering::SeqCst);
    let unknown = match rig
        .service
        .download(&rig.ctx, RecordId::new_v7(), DownloadRequest::new())
        .await
    {
        Ok(_) => panic!("unknown id"),
        Err(error) => error,
    };
    assert_eq!(
        (unknown.code(), unknown.status()),
        ("document.not_found", 404)
    );
    assert!(
        rig.service
            .download(&rig.ctx, view.id, DownloadRequest::new())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn the_metadata_write_unavailable_keeps_its_code_and_other_store_failures_keep_theirs() {
    let rig = rig();
    let view = upload(&rig).await.expect("upload");
    rig.records.down.store(true, Ordering::SeqCst);
    // `update` is what attach and delete use; it passes the typed error through unchanged.
    let record = FileRecord::clone(
        &rig.records
            .inner
            .get(&rig.ctx.site, view.id)
            .await
            .expect("get")
            .expect("present"),
    );
    let error = rig.records.update(record).await.expect_err("down");
    assert_eq!(error.code(), "server.unavailable");
    assert_eq!(FileServiceError::store("disk").code(), "file.store_failed");
    assert_eq!(FileServiceError::store("disk").retry_after(), None);
}
