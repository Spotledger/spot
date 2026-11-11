//! Upload policy, streaming limits, the digest, the server-generated key and the scan seam.

mod support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use spot_files::key::FileKeyPrefix;
use spot_files::store::{Body, FileStore};
use spot_files_service::{
    ByteLimitQuota, FakeScanner, FilePolicy, FileRecordStore, FileServiceError, FilesApi,
    ScanVerdict, Scanner, UploadRequest, storage_key,
};
use support::{
    DEFAULT_ACCESS, Fixture, PDF_HEAD, SHA256_HELLO, UPLOADER, must, must_some, small_policy,
    upload_err, upload_ok,
};
use tokio::io::{AsyncRead, ReadBuf};

/// An endless upload that counts how many bytes were pulled from it.
struct CountingReader {
    served: Arc<AtomicUsize>,
}

impl AsyncRead for CountingReader {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let count = buf.remaining().min(8192);
        let fill = vec![b'x'; count];
        buf.put_slice(&fill);
        self.served.fetch_add(count, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn digest_is_the_sha256_of_the_bytes() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("hello.txt"),
        b"hello",
    )
    .await;
    assert_eq!(view.digest, SHA256_HELLO);
    assert_eq!(view.size, 5);
    assert!(view.private);
}

#[tokio::test]
async fn streaming_limit_stops_early_and_stores_nothing() {
    let fixture = Fixture::new(
        DEFAULT_ACCESS,
        FilePolicy {
            max_upload_bytes: 1024,
            ..FilePolicy::default()
        },
    );
    let served = Arc::new(AtomicUsize::new(0));
    let body: Body = Box::pin(CountingReader {
        served: served.clone(),
    });
    let error = match fixture
        .service
        .upload(
            &fixture.ctx(Some(UPLOADER)),
            UploadRequest::new("big.bin"),
            body,
        )
        .await
    {
        Ok(_) => panic!("expected files.too_large"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "file.too_large");
    assert_eq!(error.status(), 413);
    // The stream stopped soon after the limit, not after a whole hypothetical file.
    let pulled = served.load(Ordering::SeqCst);
    assert!(pulled < 200_000, "pulled {pulled} bytes before stopping");
    // Nothing was stored: no record and no bytes.
    assert!(
        must(fixture.records.known_keys(&fixture.site).await).is_empty(),
        "a failed upload left a record"
    );
    let listed = must(
        fixture
            .store
            .list(&fixture.site, &FileKeyPrefix::all(), None, 100)
            .await,
    );
    assert!(listed.is_empty(), "a failed upload left bytes");
}

#[tokio::test]
async fn declared_type_must_match_the_bytes() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let error = upload_err(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("lie.png").with_content_type("image/png"),
        PDF_HEAD,
    )
    .await;
    assert_eq!(error.code(), "fileservice.type_mismatch");
    assert_eq!(error.status(), 422);
    assert!(must(fixture.records.known_keys(&fixture.site).await).is_empty());
    // An unknown declared type is not sniffed, so it is accepted.
    upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("blob.bin").with_content_type("application/octet-stream"),
        PDF_HEAD,
    )
    .await;
}

#[tokio::test]
async fn policy_can_deny_a_content_type() {
    let fixture = Fixture::new(
        DEFAULT_ACCESS,
        FilePolicy {
            denied_types: vec!["application/zip".to_owned()],
            ..small_policy()
        },
    );
    let error = upload_err(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("a.zip").with_content_type("application/zip"),
        b"PK\x03\x04zip",
    )
    .await;
    assert_eq!(error.code(), "api.invalid_request");
    assert_eq!(error.status(), 400);
}

#[tokio::test]
async fn policy_can_allow_only_some_types() {
    let fixture = Fixture::new(
        DEFAULT_ACCESS,
        FilePolicy {
            allowed_types: vec!["image/*".to_owned()],
            ..small_policy()
        },
    );
    upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("a.png").with_content_type("image/png"),
        b"\x89PNG\r\n\x1a\nrest",
    )
    .await;
    let error = upload_err(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("a.txt").with_content_type("text/plain"),
        b"hello",
    )
    .await;
    assert_eq!(error.code(), "api.invalid_request");
}

#[tokio::test]
async fn the_key_is_server_generated_and_never_from_the_name() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("../../etc/passwd"),
        b"hello",
    )
    .await;
    // The display name is the base name; the original is kept.
    assert_eq!(view.name, "passwd");
    let record = must_some(must(fixture.records.get(&fixture.site, view.id).await));
    assert_eq!(record.original_name, "../../etc/passwd");
    assert!(!record.key.as_str().contains("passwd"), "{}", record.key);
    assert!(!record.key.as_str().contains("etc"));
    // The key is exactly what the record derives.
    let expected = must(storage_key(record.id, record.created));
    assert_eq!(record.key, expected);
    assert!(record.key.as_str().starts_with("private/"));
}

#[tokio::test]
async fn an_upload_for_a_destination_is_public_and_pending() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let target = spot_files_service::AttachTarget::field("note", fixture.doc, "attachment");
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("spec.pdf")
            .with_content_type("application/pdf")
            .with_destination(target),
        PDF_HEAD,
    )
    .await;
    assert!(!view.private);
    let record = must_some(must(fixture.records.get(&fixture.site, view.id).await));
    assert!(record.key.as_str().starts_with("private/"));
    assert_eq!(record.state, spot_files_service::FileState::Pending);
}

#[tokio::test]
async fn a_guest_cannot_upload() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let error = upload_err(&fixture, None, UploadRequest::new("x.bin"), b"hello").await;
    assert_eq!(error.code(), "permission.denied");
    assert_eq!(error.status(), 403);
}

#[tokio::test]
async fn uploading_to_a_destination_needs_the_write_right() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let target = spot_files_service::AttachTarget::field("note", fixture.doc, "attachment");
    // The reader may read `note` but not write it.
    let error = upload_err(
        &fixture,
        Some(support::READER),
        UploadRequest::new("x.bin").with_destination(target),
        b"hello",
    )
    .await;
    assert_eq!(error.code(), "permission.denied");
}

#[tokio::test]
async fn the_quota_hook_can_refuse_an_upload() {
    let mut fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    fixture.service.set_quota(Arc::new(ByteLimitQuota {
        site_bytes: 1024,
        user_bytes: 8,
    }));
    let error = upload_err(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"much more than eight bytes",
    )
    .await;
    assert_eq!(error.code(), "file.too_large");
    assert!(must(fixture.records.known_keys(&fixture.site).await).is_empty());
    let listed = must(
        fixture
            .store
            .list(&fixture.site, &FileKeyPrefix::all(), None, 100)
            .await,
    );
    assert!(listed.is_empty(), "a quota refusal left bytes");
}

#[tokio::test]
async fn quarantine_keeps_the_record_and_refuses_downloads() {
    let mut fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    fixture
        .service
        .set_scanner(Arc::new(FakeScanner::new(ScanVerdict::Quarantine {
            reason: "review".to_owned(),
        })));
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    let record = must_some(must(fixture.records.get(&fixture.site, view.id).await));
    assert!(record.quarantined);
    let error = support::download_err(
        &fixture,
        Some(UPLOADER),
        view.id,
        spot_files_service::DownloadRequest::new(),
    )
    .await;
    assert_eq!(error.code(), "fileservice.quarantined");
    assert_eq!(error.status(), 423);
}

#[tokio::test]
async fn an_infected_upload_is_refused_and_stores_nothing() {
    let mut fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    fixture
        .service
        .set_scanner(Arc::new(FakeScanner::new(ScanVerdict::Infected {
            reason: "eicar".to_owned(),
        })));
    let error = upload_err(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    assert_eq!(error.code(), "file.virus_detected");
    assert_eq!(error.status(), 422);
    assert!(must(fixture.records.known_keys(&fixture.site).await).is_empty());
    let listed = must(
        fixture
            .store
            .list(&fixture.site, &FileKeyPrefix::all(), None, 100)
            .await,
    );
    assert!(listed.is_empty());
}

/// A scanner that always fails.
struct FailingScanner;

impl Scanner for FailingScanner {
    fn scan<'a>(
        &'a self,
        _site: &'a spot_core::SiteId,
        _id: spot_storage_contract::RecordId,
        _body: Body,
    ) -> spot_files_service::BoxFuture<'a, Result<ScanVerdict, FileServiceError>> {
        Box::pin(async {
            Err(FileServiceError::ScanFailed {
                message: "boom".to_owned(),
            })
        })
    }
}

#[tokio::test]
async fn a_failing_scan_stores_nothing() {
    let mut fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    fixture.service.set_scanner(Arc::new(FailingScanner));
    let error = upload_err(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    assert_eq!(error.code(), "file.scan_failed");
    assert_eq!(error.status(), 502);
    assert!(must(fixture.records.known_keys(&fixture.site).await).is_empty());
    let listed = must(
        fixture
            .store
            .list(&fixture.site, &FileKeyPrefix::all(), None, 100)
            .await,
    );
    assert!(listed.is_empty());
}

#[tokio::test]
async fn a_guest_upload_is_refused_before_any_storage() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let error = upload_err(&fixture, None, UploadRequest::new("x.bin"), b"hello").await;
    assert_eq!(error.code(), "permission.denied");
    assert!(must(fixture.records.known_keys(&fixture.site).await).is_empty());
}

#[tokio::test]
async fn an_empty_body_is_stored() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("empty.bin"),
        b"",
    )
    .await;
    assert_eq!(view.size, 0);
    // The digest of an empty input is the well-known constant.
    assert_eq!(
        view.digest,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}
