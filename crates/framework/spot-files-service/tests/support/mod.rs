//! Shared fixtures for the `spot-files-service` integration tests.
#![allow(dead_code)]

use std::sync::Arc;

use spot_core::{ManualClock, SiteId};
use spot_files::memory::MemoryFileStore;
use spot_files::store::{Body, body_from_vec};
use spot_files_service::{
    Caller, Download, DownloadRequest, FileCtx, FilePolicy, FileServiceError, FileView, FilesApi,
    FilesService, MemoryFileRecordStore, TableAttachAccess, TokenCallers, UploadRequest,
};
use spot_storage_contract::RecordId;
use time::{Duration, OffsetDateTime};

/// The site every fixture uses.
pub const SITE: &str = "files.test";
/// The uploader's token.
pub const UPLOADER: &str = "uploader";
/// A user who may read the `note` DocType.
pub const READER: &str = "reader";
/// A signed-in user with no grants.
pub const STRANGER: &str = "stranger";

/// Unwraps a result, naming the failure (the workspace denies `unwrap`/`expect`).
pub fn must<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
}

/// Unwraps an option, naming the failure.
pub fn must_some<T>(value: Option<T>) -> T {
    value.unwrap_or_else(|| panic!("expected Some"))
}

/// The grant table most tests use: `note` is readable by the reader, writable by the uploader;
/// `attachment` is a protected field the reader may not read; `other` is readable by the reader
/// only.
pub const DEFAULT_ACCESS: &str = r#"
protected_fields = ["attachment"]

[[grant]]
user = "{{uploader}}"
doctype = "note"
read = true
write = true

[[grant.field]]
name = "attachment"
read = true
write = true

[[grant]]
user = "{{reader}}"
doctype = "note"
read = true
write = false

[[grant]]
user = "{{reader}}"
doctype = "other"
read = true
write = false
"#;

/// A fixture: a memory store, a memory record store, the three users and one document.
pub struct Fixture {
    /// The service under test.
    pub service: FilesService,
    /// The byte store.
    pub store: Arc<MemoryFileStore>,
    /// The record store.
    pub records: Arc<MemoryFileRecordStore>,
    /// The clock (manual, so timestamps are deterministic).
    pub clock: Arc<ManualClock>,
    /// The site.
    pub site: SiteId,
    /// The uploader.
    pub uploader: RecordId,
    /// A user who may read `note` and `other`.
    pub reader: RecordId,
    /// A signed-in user with no grants.
    pub stranger: RecordId,
    /// A document id.
    pub doc: RecordId,
}

impl Fixture {
    /// Builds a fixture with the given grant table (placeholders `{{uploader}}`,
    /// `{{reader}}`, `{{stranger}}` are substituted) and policy.
    pub fn new(access: &str, policy: FilePolicy) -> Self {
        let site = SiteId::new(SITE).unwrap_or_else(|e| panic!("site: {e}"));
        let uploader = RecordId::new_v7();
        let reader = RecordId::new_v7();
        let stranger = RecordId::new_v7();
        let doc = RecordId::new_v7();
        let access = access
            .replace("{{uploader}}", &uploader.to_string())
            .replace("{{reader}}", &reader.to_string())
            .replace("{{stranger}}", &stranger.to_string());
        let table = TableAttachAccess::from_toml(&access).unwrap_or_else(|e| panic!("access: {e}"));
        let callers = TokenCallers::new()
            .with(UPLOADER, Caller::User(uploader))
            .with(READER, Caller::User(reader))
            .with(STRANGER, Caller::User(stranger));
        let store = Arc::new(MemoryFileStore::new());
        let records = Arc::new(MemoryFileRecordStore::new());
        let clock = Arc::new(ManualClock::new(
            OffsetDateTime::UNIX_EPOCH + Duration::days(20_000),
        ));
        let service = FilesService::new(
            store.clone(),
            records.clone(),
            Arc::new(callers),
            Arc::new(table),
            policy,
            clock.clone(),
        );
        Fixture {
            service,
            store,
            records,
            clock,
            site,
            uploader,
            reader,
            stranger,
            doc,
        }
    }

    /// The request context for a token, or a guest when `None`.
    pub fn ctx(&self, token: Option<&str>) -> FileCtx {
        match token {
            Some(token) => FileCtx::guest(self.site.clone()).with_credential(token),
            None => FileCtx::guest(self.site.clone()),
        }
    }
}

/// The default policy with a small upload limit.
pub fn small_policy() -> FilePolicy {
    FilePolicy {
        max_upload_bytes: 4096,
        ..FilePolicy::default()
    }
}

/// Uploads `bytes` as `request`.
pub async fn upload(
    fixture: &Fixture,
    token: Option<&str>,
    request: UploadRequest,
    bytes: &[u8],
) -> Result<FileView, FileServiceError> {
    fixture
        .service
        .upload(&fixture.ctx(token), request, body_from_vec(bytes.to_vec()))
        .await
}

/// Uploads and expects success.
pub async fn upload_ok(
    fixture: &Fixture,
    token: Option<&str>,
    request: UploadRequest,
    bytes: &[u8],
) -> FileView {
    must(upload(fixture, token, request, bytes).await)
}

/// Uploads and expects a refusal.
pub async fn upload_err(
    fixture: &Fixture,
    token: Option<&str>,
    request: UploadRequest,
    bytes: &[u8],
) -> FileServiceError {
    match upload(fixture, token, request, bytes).await {
        Ok(_) => panic!("expected a refusal"),
        Err(error) => error,
    }
}

/// Downloads and expects success.
pub async fn download_ok(
    fixture: &Fixture,
    token: Option<&str>,
    id: RecordId,
    request: DownloadRequest,
) -> Download {
    must(
        fixture
            .service
            .download(&fixture.ctx(token), id, request)
            .await,
    )
}

/// Downloads and expects a refusal.
pub async fn download_err(
    fixture: &Fixture,
    token: Option<&str>,
    id: RecordId,
    request: DownloadRequest,
) -> FileServiceError {
    match fixture
        .service
        .download(&fixture.ctx(token), id, request)
        .await
    {
        Ok(_) => panic!("expected a refusal"),
        Err(error) => error,
    }
}

/// Reads a download body to the end.
pub async fn read_body(download: Download) -> Vec<u8> {
    let mut body: Body = must_some(download.body);
    let mut out = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut body, &mut out)
        .await
        .unwrap_or_else(|e| panic!("reading body: {e}"));
    out
}

/// The SHA-256 hex of `hello`, used as an independent digest check.
pub const SHA256_HELLO: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

/// A byte sequence with a PDF magic number.
pub const PDF_HEAD: &[u8] = b"%PDF-1.7\nbody";
/// A byte sequence with a PNG magic number.
pub const PNG_HEAD: &[u8] = b"\x89PNG\r\n\x1a\nrest";
