//! The access matrix, byte ranges, conditional requests and the response headers.

mod support;

use spot_files_service::{
    DownloadRequest, DownloadStatus, FilePolicy, FileRecordStore, UploadRequest,
};
use support::{
    DEFAULT_ACCESS, Fixture, READER, STRANGER, UPLOADER, download_err, download_ok, must,
    read_body, small_policy, upload_ok,
};

fn bytes_100() -> Vec<u8> {
    (0u8..100).collect()
}

#[tokio::test]
async fn an_unattached_private_file_is_the_uploaders_alone() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    let request = DownloadRequest::new();
    // The uploader reads it.
    let download = download_ok(&fixture, Some(UPLOADER), view.id, request.clone()).await;
    assert_eq!(download.status, DownloadStatus::Full);
    assert_eq!(read_body(download).await, b"hello");
    // Nobody else does, and the answer is the same as an unknown id.
    for token in [Some(READER), Some(STRANGER), None] {
        let error = download_err(&fixture, token, view.id, request.clone()).await;
        assert_eq!(error.code(), "document.not_found", "token {token:?}");
        assert_eq!(error.status(), 404);
    }
    // An unknown id answers identically.
    let unknown = spot_storage_contract::RecordId::new_v7();
    let error = download_err(&fixture, Some(UPLOADER), unknown, request).await;
    assert_eq!(error.code(), "document.not_found");
}

#[tokio::test]
async fn an_unattached_public_file_is_readable_by_any_signed_in_user() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let target = spot_files_service::AttachTarget::field("note", fixture.doc, "attachment");
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin").with_destination(target),
        b"hello",
    )
    .await;
    // Signed-in users read it; the guest does not (the default forbids guests).
    download_ok(&fixture, Some(READER), view.id, DownloadRequest::new()).await;
    download_ok(&fixture, Some(STRANGER), view.id, DownloadRequest::new()).await;
    let error = download_err(&fixture, None, view.id, DownloadRequest::new()).await;
    assert_eq!(error.code(), "document.not_found");
}

#[tokio::test]
async fn a_guest_may_read_a_public_file_when_the_site_allows_it() {
    let fixture = Fixture::new(
        DEFAULT_ACCESS,
        FilePolicy {
            allow_guest_public: true,
            ..small_policy()
        },
    );
    let target = spot_files_service::AttachTarget::field("note", fixture.doc, "attachment");
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin").with_destination(target),
        b"hello",
    )
    .await;
    let download = download_ok(&fixture, None, view.id, DownloadRequest::new()).await;
    assert_eq!(read_body(download).await, b"hello");
}

#[tokio::test]
async fn an_attached_file_follows_the_document_read() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    // Attach it to an `other` document through a field that is not protected.
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
    // The reader may read `other`; the uploader and the stranger may not, even though the
    // uploader uploaded the file: once attached, the document's permission governs.
    let download = download_ok(&fixture, Some(READER), view.id, DownloadRequest::new()).await;
    assert_eq!(read_body(download).await, b"hello");
    for token in [Some(UPLOADER), Some(STRANGER), None] {
        let error = download_err(&fixture, token, view.id, DownloadRequest::new()).await;
        assert_eq!(error.code(), "document.not_found", "token {token:?}");
    }
}

#[tokio::test]
async fn an_attached_file_follows_a_protected_field_grant() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    // Attach through the protected `attachment` field of `note`.
    must(
        fixture
            .service
            .attach_saved(
                &fixture.site,
                "note",
                fixture.doc,
                &[("attachment".to_owned(), view.id)],
            )
            .await,
    );
    // The reader may read `note` but not the protected field, so the file is hidden.
    let error = download_err(&fixture, Some(READER), view.id, DownloadRequest::new()).await;
    assert_eq!(error.code(), "document.not_found");
    // The uploader holds the field grant.
    download_ok(&fixture, Some(UPLOADER), view.id, DownloadRequest::new()).await;
}

#[tokio::test]
async fn range_downloads_return_the_window_and_its_headers() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("numbers.bin"),
        &bytes_100(),
    )
    .await;

    let full = download_ok(&fixture, Some(UPLOADER), view.id, DownloadRequest::new()).await;
    assert_eq!(full.status, DownloadStatus::Full);
    let headers = full.headers();
    assert!(
        headers
            .iter()
            .any(|(k, v)| *k == "X-Content-Type-Options" && v == "nosniff")
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| *k == "Accept-Ranges" && v == "bytes")
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| *k == "Content-Disposition" && v.starts_with("attachment"))
    );
    assert_eq!(read_body(full).await, bytes_100());

    let partial = download_ok(
        &fixture,
        Some(UPLOADER),
        view.id,
        DownloadRequest::new().with_range("bytes=10-19"),
    )
    .await;
    assert_eq!(partial.status, DownloadStatus::Partial);
    assert!(
        partial
            .headers()
            .iter()
            .any(|(k, v)| *k == "Content-Range" && v == "bytes 10-19/100")
    );
    assert_eq!(read_body(partial).await, (10u8..=19).collect::<Vec<u8>>());

    let suffix = download_ok(
        &fixture,
        Some(UPLOADER),
        view.id,
        DownloadRequest::new().with_range("bytes=-10"),
    )
    .await;
    assert_eq!(read_body(suffix).await, (90u8..100).collect::<Vec<u8>>());
}

#[tokio::test]
async fn conditional_requests_short_circuit_to_not_modified() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    let first = download_ok(&fixture, Some(UPLOADER), view.id, DownloadRequest::new()).await;
    let etag = first.etag.clone();
    assert_eq!(etag, format!("\"{}\"", view.digest));

    let cached = download_ok(
        &fixture,
        Some(UPLOADER),
        view.id,
        DownloadRequest::new().with_if_none_match(etag.clone()),
    )
    .await;
    assert_eq!(cached.status, DownloadStatus::NotModified);
    assert!(cached.body.is_none());
    assert!(
        cached
            .headers()
            .iter()
            .any(|(k, v)| *k == "ETag" && *v == etag)
    );

    // A different validator still serves the bytes.
    let fresh = download_ok(
        &fixture,
        Some(UPLOADER),
        view.id,
        DownloadRequest::new().with_if_none_match("\"nope\""),
    )
    .await;
    assert_eq!(fresh.status, DownloadStatus::Full);
}

#[tokio::test]
async fn an_unsatisfiable_range_answers_416_with_the_size() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("numbers.bin"),
        &bytes_100(),
    )
    .await;
    let response = download_ok(
        &fixture,
        Some(UPLOADER),
        view.id,
        DownloadRequest::new().with_range("bytes=1000-"),
    )
    .await;
    assert_eq!(response.status, DownloadStatus::RangeNotSatisfiable);
    assert_eq!(response.status.status_code(), 416);
    assert!(response.body.is_none());
    assert!(
        response
            .headers()
            .iter()
            .any(|(k, v)| *k == "Content-Range" && v == "bytes */100")
    );
}

#[tokio::test]
async fn a_malformed_range_is_ignored() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("numbers.bin"),
        &bytes_100(),
    )
    .await;
    let response = download_ok(
        &fixture,
        Some(UPLOADER),
        view.id,
        DownloadRequest::new().with_range("bytes=0-1,3-4"),
    )
    .await;
    assert_eq!(response.status, DownloadStatus::Full);
    assert_eq!(read_body(response).await, bytes_100());
}

#[tokio::test]
async fn the_response_never_names_a_storage_key() {
    let fixture = Fixture::new(DEFAULT_ACCESS, small_policy());
    let view = upload_ok(
        &fixture,
        Some(UPLOADER),
        UploadRequest::new("x.bin"),
        b"hello",
    )
    .await;
    let record = support::must_some(must(fixture.records.get(&fixture.site, view.id).await));
    let download = download_ok(&fixture, Some(UPLOADER), view.id, DownloadRequest::new()).await;
    let rendered = format!("{:?} {:?}", download.headers(), view);
    assert!(!rendered.contains(record.key.as_str()), "{rendered}");
    assert!(!rendered.contains("private/"), "{rendered}");
}
