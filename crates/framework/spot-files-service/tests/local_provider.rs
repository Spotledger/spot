//! The same service behaviour over the disk-backed `LocalFileStore`, to prove nothing depends
//! on the in-memory provider.

mod support;

use std::sync::Arc;

use spot_core::{ManualClock, SiteId};
use spot_files::key::FileKeyPrefix;
use spot_files::local::LocalFileStore;
use spot_files::store::{FileStore, body_from_vec};
use spot_files_service::{
    Caller, DownloadRequest, DownloadStatus, FileCtx, FilesApi, FilesService,
    MemoryFileRecordStore, TableAttachAccess, TokenCallers, UploadRequest, storage_key,
};
use spot_storage_contract::RecordId;
use support::{DEFAULT_ACCESS, UPLOADER, must, read_body, small_policy};
use time::{Duration, OffsetDateTime};

struct LocalFixture {
    service: FilesService,
    store: Arc<LocalFileStore>,
    site: SiteId,
    _dir: tempfile::TempDir,
}

fn local_fixture() -> LocalFixture {
    let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let store = Arc::new(LocalFileStore::new(dir.path()));
    let site = SiteId::new(support::SITE).unwrap_or_else(|e| panic!("site: {e}"));
    let uploader = RecordId::new_v7();
    let callers = TokenCallers::new().with(UPLOADER, Caller::User(uploader));
    let access =
        TableAttachAccess::from_toml(DEFAULT_ACCESS).unwrap_or_else(|e| panic!("access: {e}"));
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::UNIX_EPOCH + Duration::days(20_000),
    ));
    let service = FilesService::new(
        store.clone(),
        Arc::new(MemoryFileRecordStore::new()),
        Arc::new(callers),
        Arc::new(access),
        small_policy(),
        clock,
    );
    LocalFixture {
        service,
        store,
        site,
        _dir: dir,
    }
}

#[tokio::test]
async fn upload_download_range_and_conditional_over_the_local_provider() {
    let fixture = local_fixture();
    let ctx = FileCtx::guest(fixture.site.clone()).with_credential(UPLOADER);
    let payload: Vec<u8> = (0u8..50).collect();
    let view = must(
        fixture
            .service
            .upload(
                &ctx,
                UploadRequest::new("numbers.bin").with_content_type("application/octet-stream"),
                body_from_vec(payload.clone()),
            )
            .await,
    );
    assert_eq!(view.size, 50);

    let full = must(
        fixture
            .service
            .download(&ctx, view.id, DownloadRequest::new())
            .await,
    );
    assert_eq!(full.status, DownloadStatus::Full);
    assert_eq!(read_body(full).await, payload);

    let partial = must(
        fixture
            .service
            .download(
                &ctx,
                view.id,
                DownloadRequest::new().with_range("bytes=5-9"),
            )
            .await,
    );
    assert_eq!(partial.status, DownloadStatus::Partial);
    assert_eq!(read_body(partial).await, (5u8..=9).collect::<Vec<u8>>());

    let etag = format!("\"{}\"", view.digest);
    let cached = must(
        fixture
            .service
            .download(
                &ctx,
                view.id,
                DownloadRequest::new().with_if_none_match(etag),
            )
            .await,
    );
    assert_eq!(cached.status, DownloadStatus::NotModified);
    assert!(cached.body.is_none());
}

#[tokio::test]
async fn the_local_provider_orphan_sweep_removes_unreferenced_bytes() {
    let fixture = local_fixture();
    let id = RecordId::new_v7();
    let created = OffsetDateTime::UNIX_EPOCH + Duration::days(20_000);
    let key = must(storage_key(id, created));
    must(
        fixture
            .store
            .put(
                &fixture.site,
                key.clone(),
                body_from_vec(b"orphan".to_vec()),
                spot_files::store::PutOptions::new("application/octet-stream", 4096, false),
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
    assert_eq!(report.swept, 1, "{report:?}");
    let listed = must(
        fixture
            .store
            .list(&fixture.site, &FileKeyPrefix::all(), None, 100)
            .await,
    );
    assert!(listed.is_empty());
}
