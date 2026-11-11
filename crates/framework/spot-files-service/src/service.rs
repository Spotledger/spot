//! [`FilesService`]: upload, download and orphan cleanup over a [`FileStore`].
//!
//! The service implements the native API's `/api/v1/files` operations (the [`FilesApi`] trait,
//! provisionally declared here because `spot-http`'s service layer does not have it yet) and
//! the document engine's attach lifecycle ([`FileAttachHook`]). It owns no storage detail: the
//! bytes live behind `spot-files`' [`FileStore`], the records behind the injected
//! [`FileRecordStore`], and the permission, quota and scan questions behind the injected
//! [`AttachAccess`], [`QuotaCheck`] and [`Scanner`] seams.
//!
//! # Upload
//!
//! `upload` streams the body straight to the store (never buffering it), with the site's
//! limit enforced while streaming, a running SHA-256 taken from the store, and a
//! **server-generated** key that never derives from the client's name. The declared content
//! type is checked against the site policy and against a magic-number sniff of the first
//! bytes. The record is written only after the bytes, so a crash between the two leaves a
//! purgeable orphan and never a record pointing at nothing.
//!
//! # Download
//!
//! `download` decides access first — document read (and protected field) for an attached
//! file, uploader or public for an unattached one — and answers `document.not_found` for
//! every refusal, so an id alone reveals nothing. Then it serves the bytes with an
//! `ETag` from the digest, a safely encoded `Content-Disposition`, `X-Content-Type-Options:
//! nosniff`, byte ranges and `If-None-Match`.

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use spot_core::{Clock, SiteId};
use spot_files::error::FileError;
use spot_files::key::{FileKey, FileKeyPrefix, Visibility};
use spot_files::store::{Body, FileStore, PutOptions};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

use crate::BoxFuture;
use crate::access::{AttachAccess, Caller, CallerSource, QuotaCheck, UnlimitedQuota};
use crate::ctx::FileCtx;
use crate::error::FileServiceError;
use crate::hex_lower;
use crate::hook::FileAttachHook;
use crate::policy::{
    FilePolicy, check_declared, content_disposition, normalise, sanitise_display_name,
};
use crate::range::{ContentRange, RangeOutcome, etag_matches, parse_range, range_body};
use crate::record::{AttachTarget, FileRecord, FileRecordStore, FileState, FileView};
use crate::scan::{NoopScanner, ScanVerdict, Scanner};

/// How many leading bytes are sniffed for the magic-number check.
const SNIFF_BYTES: usize = 512;

/// An upload request: the client's display name, the declared content type, and the document
/// (and field) it is destined for, when it names one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadRequest {
    /// The client's file name, from `Content-Disposition`. Sanitised for display, never used
    /// to build a key.
    pub name: String,
    /// The declared content type; `None` means `application/octet-stream`.
    pub content_type: Option<String>,
    /// The destination document and `attach` field, when the upload is for one.
    pub destination: Option<AttachTarget>,
}

impl UploadRequest {
    /// A request for a file with `name` and no declared destination.
    pub fn new(name: impl Into<String>) -> Self {
        UploadRequest {
            name: name.into(),
            content_type: None,
            destination: None,
        }
    }

    /// Sets the declared content type.
    pub fn with_content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Sets the destination document and field.
    pub fn with_destination(mut self, destination: AttachTarget) -> Self {
        self.destination = Some(destination);
        self
    }
}

/// A download request: the optional `Range` and `If-None-Match` headers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DownloadRequest {
    /// The raw `Range` header, when present.
    pub range: Option<String>,
    /// The raw `If-None-Match` header, when present.
    pub if_none_match: Option<String>,
}

impl DownloadRequest {
    /// An unconditional, full download.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the `Range` header.
    pub fn with_range(mut self, range: impl Into<String>) -> Self {
        self.range = Some(range.into());
        self
    }

    /// Sets the `If-None-Match` header.
    pub fn with_if_none_match(mut self, value: impl Into<String>) -> Self {
        self.if_none_match = Some(value.into());
        self
    }
}

/// What a download answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadStatus {
    /// `200`: the whole file.
    Full,
    /// `206`: the requested range.
    Partial,
    /// `304`: the validator matched.
    NotModified,
    /// `416`: the range cannot be satisfied.
    RangeNotSatisfiable,
}

impl DownloadStatus {
    /// The HTTP status.
    pub fn status_code(self) -> u16 {
        match self {
            DownloadStatus::Full => 200,
            DownloadStatus::Partial => 206,
            DownloadStatus::NotModified => 304,
            DownloadStatus::RangeNotSatisfiable => 416,
        }
    }
}

/// A download result: the status, the response headers and, when there are bytes, the body.
pub struct Download {
    /// The status the host must answer with.
    pub status: DownloadStatus,
    /// The stored content type.
    pub content_type: String,
    /// The safely encoded `Content-Disposition`.
    pub content_disposition: String,
    /// The `ETag`, `"<digest>"`.
    pub etag: String,
    /// The bytes, for `200` and `206`.
    pub body: Option<Body>,
    /// The `Content-Range`, for `206` and `416`.
    pub content_range: Option<ContentRange>,
}

impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("status", &self.status)
            .field("content_type", &self.content_type)
            .field("content_disposition", &self.content_disposition)
            .field("etag", &self.etag)
            .field("has_body", &self.body.is_some())
            .field("content_range", &self.content_range)
            .finish()
    }
}

impl Download {
    /// The response headers, in the order a host should set them: `ETag`,
    /// `X-Content-Type-Options: nosniff`, `Accept-Ranges: bytes`, and — when there are bytes —
    /// `Content-Type`, `Content-Disposition` and, for a range, `Content-Range`.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut headers = vec![
            ("ETag", self.etag.clone()),
            ("X-Content-Type-Options", "nosniff".to_owned()),
            ("Accept-Ranges", "bytes".to_owned()),
        ];
        if matches!(self.status, DownloadStatus::Full | DownloadStatus::Partial) {
            headers.push(("Content-Type", self.content_type.clone()));
            headers.push(("Content-Disposition", self.content_disposition.clone()));
        }
        if let Some(range) = self.content_range {
            let value = match self.status {
                DownloadStatus::RangeNotSatisfiable => ContentRange::unsatisfiable(range.size),
                _ => range.to_header(),
            };
            headers.push(("Content-Range", value));
        }
        headers
    }
}

/// What one [`FilesService::purge_orphans`] run did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// Orphaned records whose bytes and record were both removed.
    pub purged: usize,
    /// Bytes with no record at all that were swept.
    pub swept: usize,
    /// Operations that failed and will be retried by a later run.
    pub failed: usize,
}

impl PurgeReport {
    /// The total number of things removed.
    pub fn removed(&self) -> usize {
        self.purged + self.swept
    }
}

/// The native API's files operations.
pub trait FilesApi: Send + Sync + 'static {
    /// Uploads one file: the body streams to the store under a server-generated key, the
    /// declared type is checked, and the `File` record is returned in the native API's shape.
    fn upload<'a>(
        &'a self,
        ctx: &'a FileCtx,
        request: UploadRequest,
        body: Body,
    ) -> BoxFuture<'a, Result<FileView, FileServiceError>>;

    /// Downloads one file by id, honouring `Range` and `If-None-Match`, or refuses with the
    /// same `document.not_found` an unknown id gets.
    fn download<'a>(
        &'a self,
        ctx: &'a FileCtx,
        id: RecordId,
        request: DownloadRequest,
    ) -> BoxFuture<'a, Result<Download, FileServiceError>>;
}

/// The file service.
pub struct FilesService {
    /// The byte store.
    store: Arc<dyn FileStore>,
    /// The `File` records.
    records: Arc<dyn FileRecordStore>,
    /// Credential to caller.
    callers: Arc<dyn CallerSource>,
    /// The document (and protected field) read/write question.
    access: Arc<dyn AttachAccess>,
    /// The quota hook.
    quota: Arc<dyn QuotaCheck>,
    /// The scan seam.
    scanner: Arc<dyn Scanner>,
    /// The clock record timestamps come from.
    clock: Arc<dyn Clock>,
    /// The site's file policy.
    policy: FilePolicy,
}

impl FilesService {
    /// Assembles the service. The quota is [`UnlimitedQuota`] and the scanner
    /// [`NoopScanner`] until [`FilesService::with_quota`] and [`FilesService::with_scanner`]
    /// replace them.
    pub fn new(
        store: Arc<dyn FileStore>,
        records: Arc<dyn FileRecordStore>,
        callers: Arc<dyn CallerSource>,
        access: Arc<dyn AttachAccess>,
        policy: FilePolicy,
        clock: Arc<dyn Clock>,
    ) -> Self {
        FilesService {
            store,
            records,
            callers,
            access,
            quota: Arc::new(UnlimitedQuota),
            scanner: Arc::new(NoopScanner),
            clock,
            policy,
        }
    }

    /// Sets the quota hook.
    pub fn with_quota(mut self, quota: Arc<dyn QuotaCheck>) -> Self {
        self.quota = quota;
        self
    }

    /// Sets the scan seam.
    pub fn with_scanner(mut self, scanner: Arc<dyn Scanner>) -> Self {
        self.scanner = scanner;
        self
    }

    /// Replaces the quota hook in place.
    pub fn set_quota(&mut self, quota: Arc<dyn QuotaCheck>) {
        self.quota = quota;
    }

    /// Replaces the scan seam in place.
    pub fn set_scanner(&mut self, scanner: Arc<dyn Scanner>) {
        self.scanner = scanner;
    }

    /// The policy in force.
    pub fn policy(&self) -> &FilePolicy {
        &self.policy
    }

    /// Deletes orphaned files older than `older_than`, up to `limit` operations, then sweeps
    /// bytes that no record refers to (the crash-between-writes case).
    ///
    /// Bytes are deleted before the record, so a failure between the two leaves a record that
    /// the next run retries; an attached file is never touched. The sweep only removes keys
    /// with **no** record at all and older than `older_than`, so a concurrent upload's bytes
    /// are safe.
    pub async fn purge_orphans(
        &self,
        site: &SiteId,
        older_than: OffsetDateTime,
        limit: usize,
    ) -> Result<PurgeReport, FileServiceError> {
        let mut report = PurgeReport::default();
        let orphans = self.records.orphans(site, older_than, limit).await?;
        for record in orphans {
            match self.store.delete(site, record.key.clone()).await {
                Ok(_) => {}
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(file = %record.id, error = %error, "orphan bytes not deleted; will retry");
                    continue;
                }
            }
            match self.records.delete(site, record.id).await {
                Ok(_) => report.purged += 1,
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(file = %record.id, error = %error, "orphan record not deleted; will retry");
                }
            }
        }

        let budget = limit.saturating_sub(report.purged + report.failed);
        if budget > 0 {
            let known: BTreeSet<FileKey> =
                self.records.known_keys(site).await?.into_iter().collect();
            let listed = self
                .store
                .list(site, &FileKeyPrefix::all(), None, usize::MAX)
                .await
                .map_err(map_store_error)?;
            for meta in listed {
                if report.swept >= budget {
                    break;
                }
                if known.contains(&meta.key) || meta.modified > older_than {
                    continue;
                }
                match self.store.delete(site, meta.key.clone()).await {
                    Ok(_) => report.swept += 1,
                    Err(error) => {
                        report.failed += 1;
                        tracing::warn!(error = %error, "unreferenced bytes not deleted; will retry");
                    }
                }
            }
        }
        Ok(report)
    }

    /// The attach-save step the [`FileAttachHook`] exposes, for direct calls.
    pub async fn attach_saved(
        &self,
        site: &SiteId,
        doctype: &str,
        id: RecordId,
        attachments: &[(String, RecordId)],
    ) -> Result<(), FileServiceError> {
        // Orphan anything attached to this document that the new value set no longer names.
        for record in self.records.by_document(site, doctype, id).await? {
            let still = record
                .attached_to
                .as_ref()
                .is_some_and(|target| match &target.field {
                    Some(field) => attachments
                        .iter()
                        .any(|(name, file)| name == field && *file == record.id),
                    None => false,
                });
            if !still {
                self.orphan(record).await?;
            }
        }
        // Attach the named files, replacing any other file on the same exact target.
        for (field, file_id) in attachments {
            let target = AttachTarget::field(doctype, id, field.clone());
            let Some(mut record) = self.records.get(site, *file_id).await? else {
                continue; // the engine validates link targets; an unknown id is ignored
            };
            for other in self.records.by_target(site, &target).await? {
                if other.id != record.id {
                    self.orphan(other).await?;
                }
            }
            record.state = FileState::Attached;
            record.attached_to = Some(target);
            record.private = false;
            self.records.update(record).await?;
        }
        Ok(())
    }

    /// The attach-delete step the [`FileAttachHook`] exposes, for direct calls.
    pub async fn attach_deleted(
        &self,
        site: &SiteId,
        doctype: &str,
        id: RecordId,
    ) -> Result<(), FileServiceError> {
        for record in self.records.by_document(site, doctype, id).await? {
            self.orphan(record).await?;
        }
        Ok(())
    }

    /// Marks one record orphaned and detaches it.
    async fn orphan(&self, mut record: FileRecord) -> Result<(), FileServiceError> {
        record.state = FileState::Orphaned;
        record.attached_to = None;
        self.records.update(record).await
    }

    /// Whether `caller` may read `record`.
    async fn may_read(
        &self,
        caller: &Caller,
        record: &FileRecord,
    ) -> Result<bool, FileServiceError> {
        if record.state == FileState::Attached
            && let Some(target) = &record.attached_to
        {
            // The document's permission governs, and a protected field follows its grant.
            return self.access.may_read(caller, target).await;
        }
        if caller.user_id() == Some(record.uploaded_by) {
            return Ok(true);
        }
        if !record.private {
            return Ok(match caller {
                Caller::User(_) => true,
                Caller::Guest => self.policy.allow_guest_public,
            });
        }
        Ok(false)
    }

    async fn upload_inner(
        &self,
        ctx: &FileCtx,
        request: UploadRequest,
        body: Body,
    ) -> Result<FileView, FileServiceError> {
        let caller = self.callers.caller(ctx).await?;
        let Some(user) = caller.user_id() else {
            return Err(FileServiceError::PermissionDenied);
        };
        if let Some(target) = &request.destination
            && !self.access.may_upload(&caller, target).await?
        {
            return Err(FileServiceError::PermissionDenied);
        }

        let original_name = request.name.clone();
        let name = sanitise_display_name(&request.name, self.policy.max_name_chars());
        let declared = normalise(
            request
                .content_type
                .as_deref()
                .filter(|t| !t.trim().is_empty())
                .unwrap_or("application/octet-stream"),
        );
        if !self.policy.type_allowed(&declared) {
            return Err(FileServiceError::TypeDenied {
                content_type: declared,
            });
        }

        let id = RecordId::new_v7();
        let created = self.clock.now_utc();
        let private = request.destination.is_none();
        let key = storage_key(id, created)?;

        let (head, body) = read_head(body, SNIFF_BYTES).await?;
        if let Err(sniffed) = check_declared(&declared, &head) {
            return Err(FileServiceError::TypeMismatch { declared, sniffed });
        }
        let body: Body = Box::pin(Prefixed::new(head, body));

        let stored = self
            .store
            .put(
                &ctx.site,
                key.clone(),
                body,
                PutOptions::new(declared.clone(), self.policy.max_upload_bytes, false),
            )
            .await
            .map_err(map_store_error)?;

        // The quota is checked with the stored size: a refusal leaves no bytes and no record.
        if let Err(error) = self.quota.check(&ctx.site, &user, stored.size).await {
            let _ = self.store.delete(&ctx.site, key.clone()).await;
            return Err(error);
        }

        let record = FileRecord {
            id,
            site: ctx.site.clone(),
            name,
            original_name,
            content_type: declared,
            size: stored.size,
            digest: stored.sha256,
            private,
            attached_to: None,
            uploaded_by: user,
            created,
            state: FileState::Pending,
            quarantined: false,
            key,
        };
        if let Err(error) = self.records.insert(record.clone()).await {
            self.cleanup(&ctx.site, &record).await;
            return Err(error);
        }

        match self.scan_stored(&ctx.site, id, &record).await {
            Ok(ScanVerdict::Clean) => {}
            Ok(ScanVerdict::Quarantine { reason }) => {
                let mut quarantined = record.clone();
                quarantined.quarantined = true;
                self.records.update(quarantined.clone()).await?;
                tracing::warn!(file = %id, %reason, "upload quarantined");
                return Ok(quarantined.view());
            }
            Ok(ScanVerdict::Infected { reason }) => {
                self.cleanup(&ctx.site, &record).await;
                return Err(FileServiceError::VirusDetected { message: reason });
            }
            Err(error) => {
                self.cleanup(&ctx.site, &record).await;
                return Err(error);
            }
        }

        tracing::info!(file = %id, size = stored.size, "upload stored");
        Ok(record.view())
    }

    /// Reads the stored bytes back for the scanner.
    async fn scan_stored(
        &self,
        site: &SiteId,
        id: RecordId,
        record: &FileRecord,
    ) -> Result<ScanVerdict, FileServiceError> {
        let (_, body) = self
            .store
            .get(site, record.key.clone())
            .await
            .map_err(map_store_error)?;
        self.scanner.scan(site, id, body).await
    }

    /// Best-effort removal of a failed upload's bytes and record.
    async fn cleanup(&self, site: &SiteId, record: &FileRecord) {
        if let Err(error) = self.store.delete(site, record.key.clone()).await {
            tracing::warn!(file = %record.id, error = %error, "could not delete failed upload's bytes");
        }
        if let Err(error) = self.records.delete(site, record.id).await {
            tracing::warn!(file = %record.id, error = %error, "could not delete failed upload's record");
        }
    }

    async fn download_inner(
        &self,
        ctx: &FileCtx,
        id: RecordId,
        request: DownloadRequest,
    ) -> Result<Download, FileServiceError> {
        let caller = self.callers.caller(ctx).await?;
        let Some(record) = self.records.get(&ctx.site, id).await? else {
            return Err(FileServiceError::NotFound);
        };
        if !self.may_read(&caller, &record).await? {
            return Err(FileServiceError::NotFound);
        }
        if record.quarantined {
            return Err(FileServiceError::Quarantined {
                reason: "quarantined by the file scan".to_owned(),
            });
        }

        let etag = format!("\"{}\"", hex_lower(&record.digest));
        let disposition = self.policy.disposition(&record.content_type);
        let content_disposition = content_disposition(disposition, &record.name);
        if let Some(if_none_match) = &request.if_none_match
            && etag_matches(if_none_match, &etag)
        {
            return Ok(Download {
                status: DownloadStatus::NotModified,
                content_type: record.content_type,
                content_disposition,
                etag,
                body: None,
                content_range: None,
            });
        }

        let (meta, body) = match self.store.get(&ctx.site, record.key.clone()).await {
            Ok(pair) => pair,
            Err(FileError::NotFound { .. }) => return Err(FileServiceError::NotFound),
            Err(error) => return Err(map_store_error(error)),
        };

        let outcome = request
            .range
            .as_deref()
            .map_or(RangeOutcome::Full, |header| parse_range(header, meta.size));
        match outcome {
            RangeOutcome::Unsatisfiable => Ok(Download {
                status: DownloadStatus::RangeNotSatisfiable,
                content_type: record.content_type,
                content_disposition,
                etag,
                body: None,
                content_range: Some(ContentRange {
                    start: 0,
                    end: 0,
                    size: meta.size,
                }),
            }),
            RangeOutcome::Partial(range) => Ok(Download {
                status: DownloadStatus::Partial,
                content_type: record.content_type,
                content_disposition,
                etag,
                body: Some(range_body(body, range)),
                content_range: Some(ContentRange {
                    start: range.start,
                    end: range.end,
                    size: meta.size,
                }),
            }),
            RangeOutcome::Full => Ok(Download {
                status: DownloadStatus::Full,
                content_type: record.content_type,
                content_disposition,
                etag,
                body: Some(body),
                content_range: None,
            }),
        }
    }
}

impl FilesApi for FilesService {
    fn upload<'a>(
        &'a self,
        ctx: &'a FileCtx,
        request: UploadRequest,
        body: Body,
    ) -> BoxFuture<'a, Result<FileView, FileServiceError>> {
        Box::pin(self.upload_inner(ctx, request, body))
    }

    fn download<'a>(
        &'a self,
        ctx: &'a FileCtx,
        id: RecordId,
        request: DownloadRequest,
    ) -> BoxFuture<'a, Result<Download, FileServiceError>> {
        Box::pin(self.download_inner(ctx, id, request))
    }
}

impl FileAttachHook for FilesService {
    fn on_document_saved<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
        attachments: &'a [(String, RecordId)],
    ) -> BoxFuture<'a, Result<(), FileServiceError>> {
        Box::pin(self.attach_saved(site, doctype, id, attachments))
    }

    fn on_document_deleted<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<(), FileServiceError>> {
        Box::pin(self.attach_deleted(site, doctype, id))
    }
}

/// Builds the server-generated storage key: the `private/` segment, the upload's year and
/// month, and the record id. Nothing here comes from the client's name.
///
/// The key is a function of `id` and `created` only, both immutable, so it is the same for the
/// life of the file. The record's `private` flag means only "who may read" (the uploader until
/// the file is attached); it flips on attach while the bytes stay where they were uploaded, so
/// it must never select the key. A host's record store can therefore persist a record without a
/// key column and recompute the key on load. Access is decided by the access check, never by
/// the prefix. There is no legacy-key reader: nothing has been released with `public/` keys
/// from this service.
pub fn storage_key(id: RecordId, created: OffsetDateTime) -> Result<FileKey, FileServiceError> {
    let rest = format!("{:04}/{:02}/{}", created.year(), created.month() as u8, id);
    FileKey::new(Visibility::Private, &rest).map_err(|error| FileServiceError::Internal {
        message: error.to_string(),
    })
}

/// Reads up to `max` leading bytes, returning them and the rest of the body.
async fn read_head(mut body: Body, max: usize) -> Result<(Vec<u8>, Body), FileServiceError> {
    let mut head = Vec::with_capacity(max.min(4096));
    let mut buf = [0u8; 1024];
    while head.len() < max {
        let want = (max - head.len()).min(buf.len());
        let read =
            body.read(&mut buf[..want])
                .await
                .map_err(|error| FileServiceError::StoreFailed {
                    message: format!("reading the upload: {error}"),
                })?;
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buf[..read]);
    }
    Ok((head, body))
}

/// A body that serves a leading buffer and then the wrapped body.
struct Prefixed<R> {
    /// The already-read leading bytes.
    head: std::io::Cursor<Vec<u8>>,
    /// The rest of the upload.
    rest: R,
}

impl<R> Prefixed<R> {
    fn new(head: Vec<u8>, rest: R) -> Self {
        Prefixed {
            head: std::io::Cursor::new(head),
            rest,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Prefixed<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let position = this.head.position() as usize;
        let head = this.head.get_ref();
        if position < head.len() {
            let count = (head.len() - position).min(buf.remaining());
            let slice = head[position..position + count].to_vec();
            buf.put_slice(&slice);
            this.head.set_position((position + count) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.rest).poll_read(cx, buf)
    }
}

/// Maps a store failure onto the service's vocabulary.
fn map_store_error(error: FileError) -> FileServiceError {
    match error {
        FileError::TooLarge { max_bytes } => FileServiceError::TooLarge { max_bytes },
        FileError::InvalidKey { reason } => FileServiceError::Internal {
            message: reason.to_owned(),
        },
        FileError::Io { source } => FileServiceError::StoreFailed {
            message: source.to_string(),
        },
        FileError::NotFound { .. } => FileServiceError::StoreFailed {
            message: "the stored bytes are missing".to_owned(),
        },
        FileError::AlreadyExists { .. } => FileServiceError::StoreFailed {
            message: "the server-generated key already exists".to_owned(),
        },
    }
}
