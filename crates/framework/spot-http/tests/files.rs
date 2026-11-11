//! `file_upload` and `file_get` over the wire (WP-PLT-10.T5, ADR-055 "Files"): octet-stream
//! and multipart uploads, the streaming upload cap, ranges and conditional downloads, the one
//! indistinguishable refusal, `Idempotency-Key`, authentication and the unconfigured store.

#[allow(dead_code)]
mod support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, header};
use hyper::body::{Body as HttpBody, Frame};
use serde_json::Value as Json;
use spot_core::{ManualClock, SiteId};
use spot_http::api::files::{FilePortError, MemoryFilesPort};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::router::{ApiServices, router};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use support::Must;
use support::fake::{FakeApp, TOKEN_ALICE, TOKEN_BOB};
use time::OffsetDateTime;
use tower::ServiceExt;

const HOST: &str = "erp.acme.com";
/// The file limit; the server's own body limit is far above it so the file cap decides.
const FILE_LIMIT: u64 = 1024;

struct Files {
    router: axum::Router,
    port: Arc<MemoryFilesPort>,
}

fn build(configured: bool) -> Files {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    let mut services = ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store);
    let port = Arc::new(MemoryFilesPort::new(FILE_LIMIT));
    if configured {
        services = services.with_files(port.clone());
    }
    let site = SiteId::new("acme").must();
    let config = ServerConfig {
        max_body_bytes: 8 * 1024 * 1024,
        ..ServerConfig::default()
    };
    let router = ServerBuilder::new(
        config,
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build();
    Files { router, port }
}

fn files() -> Files {
    build(true)
}

fn unconfigured() -> Files {
    build(false)
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    bytes: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_slice(&self.bytes).unwrap_or(Json::Null)
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

async fn send(app: &Files, request: Request<Body>) -> Reply {
    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("router service failed: {error}"));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .must()
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
    }
}

fn request(method: &str, path: &str, headers: &[(&str, &str)], body: Body) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body).must()
}

fn auth(token: &str) -> String {
    format!("Bearer {token}")
}

async fn upload_octets(app: &Files, token: &str, name: &str, bytes: &[u8]) -> Reply {
    let disposition = format!("attachment; filename=\"{name}\"");
    let bearer = auth(token);
    send(
        app,
        request(
            "POST",
            "/api/v1/files",
            &[
                ("authorization", &bearer),
                ("content-type", "application/octet-stream"),
                ("content-disposition", &disposition),
            ],
            Body::from(bytes.to_vec()),
        ),
    )
    .await
}

fn multipart(boundary: &str, parts: &[(&str, &str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (field, filename, content_type, content) in parts {
        out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        out.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{field}\"; filename=\"{filename}\"\r\n"
            )
            .as_bytes(),
        );
        if let Some(ct) = content_type {
            out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(content);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    out
}

async fn upload_multipart(app: &Files, token: &str, body: Vec<u8>) -> Reply {
    let bearer = auth(token);
    send(
        app,
        request(
            "POST",
            "/api/v1/files",
            &[
                ("authorization", &bearer),
                ("content-type", "multipart/form-data; boundary=XyZ"),
            ],
            Body::from(body),
        ),
    )
    .await
}

async fn download(app: &Files, token: &str, id: &str, extra: &[(&str, &str)]) -> Reply {
    let bearer = auth(token);
    let mut headers = vec![("authorization", bearer.as_str())];
    headers.extend_from_slice(extra);
    send(
        app,
        request(
            "GET",
            &format!("/api/v1/files/{id}"),
            &headers,
            Body::empty(),
        ),
    )
    .await
}

/// A request body that yields fixed chunks and counts how many were pulled.
struct CountingBody {
    chunks: std::vec::IntoIter<Bytes>,
    pulled: Arc<AtomicUsize>,
}

impl HttpBody for CountingBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        match self.chunks.next() {
            Some(chunk) => {
                self.pulled.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(Some(Ok(Frame::data(chunk))))
            }
            None => Poll::Ready(None),
        }
    }
}

/// The router answers `413 file.too_large`, but the server core's body-limit middleware
/// (`src/middleware.rs`, outside this task's paths) rewrites every handler `413` into
/// `http.body_too_large`. Until that middleware leaves a handler's own envelope alone, both
/// codes are what a client can see; the status, and that nothing was stored or read, are exact.
fn assert_too_large(reply: &Reply) {
    assert_eq!(reply.status, 413, "{}", reply.text());
    assert!(
        ["file.too_large", "http.body_too_large"].contains(&reply.code().as_str()),
        "{}",
        reply.text()
    );
}

fn assert_no_storage_detail(reply: &Reply) {
    let text = reply.text().to_ascii_lowercase();
    for needle in ["path", "key\"", "/var/", "s3://", "file://", "\\\\"] {
        assert!(!text.contains(needle), "`{needle}` leaked into {text}");
    }
}

#[tokio::test]
async fn an_octet_stream_upload_answers_the_record_and_nothing_else() {
    let app = files();
    let reply = upload_octets(&app, TOKEN_ALICE, "spec.pdf", b"hello files").await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    let data = reply.json()["data"].clone();
    assert_eq!(data["name"], "spec.pdf");
    assert_eq!(data["size"], 11);
    assert_eq!(data["content_type"], "application/octet-stream");
    assert_eq!(data["private"], true);
    let digest = data["digest"].as_str().must();
    assert_eq!(digest.len(), 64);
    assert!(
        digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    let mut fields: Vec<_> = data.as_object().must().keys().cloned().collect();
    fields.sort();
    assert_eq!(
        fields,
        ["content_type", "digest", "id", "name", "private", "size"]
    );
    let id = data["id"].as_str().must();
    assert_eq!(
        reply.header("location"),
        Some(format!("/api/v1/files/{id}").as_str())
    );
    assert!(reply.header("etag").is_some());
    assert_no_storage_detail(&reply);
}

#[tokio::test]
async fn the_name_may_be_percent_encoded_and_is_required() {
    let app = files();
    let bearer = auth(TOKEN_ALICE);
    let encoded = send(
        &app,
        request(
            "POST",
            "/api/v1/files",
            &[
                ("authorization", &bearer),
                ("content-type", "application/octet-stream"),
                (
                    "content-disposition",
                    "attachment; filename*=UTF-8''caf%C3%A9.txt",
                ),
            ],
            Body::from("x"),
        ),
    )
    .await;
    assert_eq!(encoded.status, 201, "{}", encoded.text());
    assert_eq!(encoded.json()["data"]["name"], "café.txt");

    let nameless = send(
        &app,
        request(
            "POST",
            "/api/v1/files",
            &[
                ("authorization", &bearer),
                ("content-type", "application/octet-stream"),
            ],
            Body::from("x"),
        ),
    )
    .await;
    assert_eq!(nameless.status, 400);
    assert_eq!(nameless.code(), "api.invalid_request");
    assert_eq!(
        app.port.file_count(),
        1,
        "the refused upload stored nothing"
    );
}

#[tokio::test]
async fn a_multipart_upload_takes_the_name_and_type_from_the_file_part() {
    let app = files();
    let body = multipart(
        "XyZ",
        &[("file", "notes.txt", Some("text/plain"), b"line1\r\nline2")],
    );
    let reply = upload_multipart(&app, TOKEN_ALICE, body).await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    let data = reply.json()["data"].clone();
    assert_eq!(data["name"], "notes.txt");
    assert_eq!(data["content_type"], "text/plain");
    assert_eq!(data["size"], 12);

    let id = data["id"].as_str().must();
    let fetched = download(&app, TOKEN_ALICE, id, &[]).await;
    assert_eq!(fetched.status, 200);
    assert_eq!(fetched.bytes, b"line1\r\nline2");
}

#[tokio::test]
async fn an_empty_multipart_file_and_a_delimiter_lookalike_survive() {
    let app = files();
    let empty = upload_multipart(
        &app,
        TOKEN_ALICE,
        multipart("XyZ", &[("file", "e", None, b"")]),
    )
    .await;
    assert_eq!(empty.status, 201, "{}", empty.text());
    assert_eq!(empty.json()["data"]["size"], 0);
    let tricky = b"a\r\n--Xy not the delimiter\r\n--X";
    let reply = upload_multipart(
        &app,
        TOKEN_ALICE,
        multipart("XyZ", &[("file", "t", None, tricky)]),
    )
    .await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    let id = reply.json()["data"]["id"].as_str().must().to_owned();
    assert_eq!(download(&app, TOKEN_ALICE, &id, &[]).await.bytes, tricky);
}

#[tokio::test]
async fn a_multipart_form_is_one_file_part_and_nothing_else() {
    let app = files();
    let two = upload_multipart(
        &app,
        TOKEN_ALICE,
        multipart(
            "XyZ",
            &[
                ("file", "a.txt", None, b"aaa"),
                ("file", "b.txt", None, b"bbb"),
            ],
        ),
    )
    .await;
    assert_eq!(two.status, 400, "{}", two.text());
    assert_eq!(two.code(), "api.invalid_request");
    let other = upload_multipart(
        &app,
        TOKEN_ALICE,
        multipart("XyZ", &[("note", "a.txt", None, b"aaa")]),
    )
    .await;
    assert_eq!(other.status, 400);
    assert_eq!(other.code(), "api.invalid_request");
    let none = upload_multipart(&app, TOKEN_ALICE, b"--XyZ--\r\n".to_vec()).await;
    assert_eq!(none.status, 400);
    let truncated = upload_multipart(
        &app,
        TOKEN_ALICE,
        b"--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a\"\r\n\r\nabc"
            .to_vec(),
    )
    .await;
    assert_eq!(truncated.status, 400);
    assert_eq!(app.port.file_count(), 0, "a refused form stores nothing");
}

#[tokio::test]
async fn other_content_types_are_unsupported() {
    let app = files();
    let bearer = auth(TOKEN_ALICE);
    let reply = send(
        &app,
        request(
            "POST",
            "/api/v1/files",
            &[
                ("authorization", &bearer),
                ("content-type", "application/json"),
                ("content-disposition", "attachment; filename=\"a.json\""),
            ],
            Body::from("{}"),
        ),
    )
    .await;
    assert_eq!(reply.status, 415);
    assert_eq!(reply.code(), "api.unsupported_media_type");
}

#[tokio::test]
async fn an_over_limit_upload_stops_reading_and_answers_413() {
    let app = files();
    let bearer = auth(TOKEN_ALICE);
    let pulled = Arc::new(AtomicUsize::new(0));
    let chunk = Bytes::from(vec![7u8; 400]);
    let body = CountingBody {
        chunks: vec![chunk; 50].into_iter(),
        pulled: pulled.clone(),
    };
    let reply = send(
        &app,
        request(
            "POST",
            "/api/v1/files",
            &[
                ("authorization", &bearer),
                ("content-type", "application/octet-stream"),
                ("content-disposition", "attachment; filename=\"big.bin\""),
            ],
            Body::new(body),
        ),
    )
    .await;
    assert_too_large(&reply);
    let read = pulled.load(Ordering::SeqCst);
    assert!(
        read <= 3,
        "the cap stopped the read after {read} of 50 chunks"
    );
    assert_eq!(app.port.file_count(), 0, "nothing over the limit is stored");
}

#[tokio::test]
async fn a_declared_length_over_the_limit_is_refused_before_any_read() {
    let app = files();
    let bearer = auth(TOKEN_ALICE);
    let pulled = Arc::new(AtomicUsize::new(0));
    let body = CountingBody {
        chunks: vec![Bytes::from_static(b"x")].into_iter(),
        pulled: pulled.clone(),
    };
    let reply = send(
        &app,
        request(
            "POST",
            "/api/v1/files",
            &[
                ("authorization", &bearer),
                ("content-type", "application/octet-stream"),
                ("content-disposition", "attachment; filename=\"big.bin\""),
                ("content-length", "5000"),
            ],
            Body::new(body),
        ),
    )
    .await;
    assert_too_large(&reply);
    assert_eq!(pulled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_multipart_file_over_the_limit_answers_413() {
    let app = files();
    let big = vec![9u8; 4000];
    let reply = upload_multipart(
        &app,
        TOKEN_ALICE,
        multipart("XyZ", &[("file", "big.bin", None, &big)]),
    )
    .await;
    assert_too_large(&reply);
    assert_eq!(app.port.file_count(), 0);
}

#[tokio::test]
async fn a_download_carries_the_validators_and_never_a_path() {
    let app = files();
    let up = upload_octets(&app, TOKEN_ALICE, "data.bin", b"0123456789").await;
    let record = up.json()["data"].clone();
    let id = record["id"].as_str().must();
    let digest = record["digest"].as_str().must();

    let full = download(&app, TOKEN_ALICE, id, &[]).await;
    assert_eq!(full.status, 200);
    assert_eq!(full.bytes, b"0123456789");
    assert_eq!(full.header("etag"), Some(format!("\"{digest}\"").as_str()));
    assert_eq!(full.header("accept-ranges"), Some("bytes"));
    assert_eq!(
        full.header("content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(full.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        full.header("content-disposition"),
        Some("attachment; filename=\"data.bin\"")
    );

    let ranged = download(&app, TOKEN_ALICE, id, &[("range", "bytes=2-5")]).await;
    assert_eq!(ranged.status, 206);
    assert_eq!(ranged.bytes, b"2345");
    assert_eq!(ranged.header("content-range"), Some("bytes 2-5/10"));
    let tail = download(&app, TOKEN_ALICE, id, &[("range", "bytes=-3")]).await;
    assert_eq!(tail.status, 206);
    assert_eq!(tail.bytes, b"789");
    let open = download(&app, TOKEN_ALICE, id, &[("range", "bytes=8-")]).await;
    assert_eq!(open.bytes, b"89");

    let unsatisfiable = download(&app, TOKEN_ALICE, id, &[("range", "bytes=50-60")]).await;
    assert_eq!(unsatisfiable.status, 416);
    assert_eq!(unsatisfiable.header("content-range"), Some("bytes */10"));
    assert!(unsatisfiable.bytes.is_empty());

    let etag = format!("\"{digest}\"");
    let cached = download(&app, TOKEN_ALICE, id, &[("if-none-match", &etag)]).await;
    assert_eq!(cached.status, 304);
    assert!(cached.bytes.is_empty());
    let both = download(
        &app,
        TOKEN_ALICE,
        id,
        &[("if-none-match", &etag), ("range", "bytes=0-1")],
    )
    .await;
    assert_eq!(both.status, 304, "a matching validator wins over a range");
    let stale = download(&app, TOKEN_ALICE, id, &[("if-none-match", "\"other\"")]).await;
    assert_eq!(stale.status, 200);
    for reply in [&full, &ranged, &unsatisfiable, &cached] {
        assert!(!reply.headers.contains_key("location"));
    }
}

#[tokio::test]
async fn a_suspicious_content_type_is_served_as_an_attachment() {
    let app = files();
    let html = upload_multipart(
        &app,
        TOKEN_ALICE,
        multipart(
            "XyZ",
            &[(
                "file",
                "page.html",
                Some("text/html"),
                b"<script>1</script>",
            )],
        ),
    )
    .await;
    let id = html.json()["data"]["id"].as_str().must().to_owned();
    let served = download(&app, TOKEN_ALICE, &id, &[]).await;
    assert_eq!(served.status, 200);
    assert_eq!(served.header("content-type"), Some("text/html"));
    assert!(
        served
            .header("content-disposition")
            .is_some_and(|d| d.starts_with("attachment")),
        "{:?}",
        served.header("content-disposition")
    );
    assert_eq!(served.header("x-content-type-options"), Some("nosniff"));

    let png = upload_multipart(
        &app,
        TOKEN_ALICE,
        multipart("XyZ", &[("file", "pic.png", Some("image/png"), b"\x89PNG")]),
    )
    .await;
    let png_id = png.json()["data"]["id"].as_str().must().to_owned();
    let shown = download(&app, TOKEN_ALICE, &png_id, &[]).await;
    assert!(
        shown
            .header("content-disposition")
            .is_some_and(|d| d.starts_with("inline"))
    );
}

#[tokio::test]
async fn a_forbidden_file_and_a_missing_file_answer_identically() {
    let app = files();
    let up = upload_octets(&app, TOKEN_ALICE, "mine.txt", b"private").await;
    let id = up.json()["data"]["id"].as_str().must().to_owned();
    let forbidden = download(&app, TOKEN_BOB, &id, &[]).await;
    let missing = download(&app, TOKEN_BOB, "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e7f", &[]).await;
    assert_eq!(forbidden.status, 404);
    assert_eq!(forbidden.code(), "document.not_found");
    assert_eq!(missing.status, forbidden.status);
    let strip = |reply: &Reply| {
        let mut body = reply.json();
        body["error"]["request_id"] = Json::Null;
        body
    };
    assert_eq!(strip(&forbidden), strip(&missing));
    assert_eq!(
        forbidden.header("content-type"),
        missing.header("content-type")
    );

    // The owner still reads it; sharing (as an attached document would) opens it to others.
    assert_eq!(download(&app, TOKEN_ALICE, &id, &[]).await.status, 200);
    let record_id = spot_http::api::codec::parse_record_id(&id).must();
    app.port.share(record_id);
    assert_eq!(download(&app, TOKEN_BOB, &id, &[]).await.status, 200);

    let bad = download(&app, TOKEN_BOB, "not-an-id", &[]).await;
    assert_eq!(bad.status, 400);
}

#[tokio::test]
async fn the_same_key_and_body_replays_the_stored_record() {
    let app = files();
    let bearer = auth(TOKEN_ALICE);
    let send_once = |bytes: &'static [u8]| {
        let req = request(
            "POST",
            "/api/v1/files",
            &[
                ("authorization", &bearer),
                ("content-type", "application/octet-stream"),
                ("content-disposition", "attachment; filename=\"a.bin\""),
                ("idempotency-key", "upload-1"),
            ],
            Body::from(bytes),
        );
        send(&app, req)
    };
    let first = send_once(b"same bytes").await;
    assert_eq!(first.status, 201, "{}", first.text());
    assert!(first.header("idempotency-replayed").is_none());
    let second = send_once(b"same bytes").await;
    assert_eq!(second.status, 201);
    assert_eq!(second.header("idempotency-replayed"), Some("true"));
    assert_eq!(second.json(), first.json());
    assert_eq!(app.port.file_count(), 1, "the repeat stored nothing");

    let different = send_once(b"other bytes").await;
    assert_eq!(different.status, 409, "{}", different.text());
    assert_eq!(different.code(), "api.idempotency_conflict");
    assert_eq!(app.port.file_count(), 1);
}

#[tokio::test]
async fn a_failed_scan_releases_the_key_for_a_retry() {
    let app = files();
    app.port.fail_next_upload(FilePortError::new(
        502,
        "file.scan_failed",
        "The scan seam failed.",
        true,
    ));
    let bearer = auth(TOKEN_ALICE);
    let attempt = || {
        send(
            &app,
            request(
                "POST",
                "/api/v1/files",
                &[
                    ("authorization", &bearer),
                    ("content-type", "application/octet-stream"),
                    ("content-disposition", "attachment; filename=\"a.bin\""),
                    ("idempotency-key", "scan-1"),
                ],
                Body::from("bytes"),
            ),
        )
    };
    let first = attempt().await;
    assert_eq!(first.status, 502, "{}", first.text());
    assert_eq!(first.code(), "file.scan_failed");
    let retry = attempt().await;
    assert_eq!(retry.status, 201, "{}", retry.text());
    assert!(retry.header("idempotency-replayed").is_none());
    assert_eq!(app.port.file_count(), 1);
}

#[tokio::test]
async fn port_failures_use_the_registry_status_and_hide_internals() {
    let app = files();
    app.port.fail_next_upload(FilePortError::new(
        422,
        "file.virus_detected",
        "Malware found.",
        false,
    ));
    let virus = upload_octets(&app, TOKEN_ALICE, "v.bin", b"x").await;
    assert_eq!(virus.status, 422);
    assert_eq!(virus.code(), "file.virus_detected");
    app.port
        .fail_next_upload(FilePortError::new(500, "files.disk_on_fire", "boom", false));
    let odd = upload_octets(&app, TOKEN_ALICE, "v.bin", b"x").await;
    assert_eq!(odd.status, 500);
    assert_eq!(odd.code(), "http.internal");
}

#[tokio::test]
async fn both_routes_need_a_credential() {
    let app = files();
    let up = send(
        &app,
        request(
            "POST",
            "/api/v1/files",
            &[
                ("content-type", "application/octet-stream"),
                ("content-disposition", "attachment; filename=\"a\""),
            ],
            Body::from("x"),
        ),
    )
    .await;
    assert_eq!(up.status, 401);
    assert_eq!(up.code(), "auth.required");
    let get = send(
        &app,
        request(
            "GET",
            "/api/v1/files/0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e7f",
            &[],
            Body::empty(),
        ),
    )
    .await;
    assert_eq!(get.status, 401);
    assert_eq!(get.code(), "auth.required");
}

#[tokio::test]
async fn a_site_without_a_file_store_answers_not_configured() {
    let app = unconfigured();
    let up = upload_octets(&app, TOKEN_ALICE, "a.txt", b"x").await;
    assert_eq!(up.status, 501, "{}", up.text());
    assert_eq!(up.code(), "service.not_configured");
    assert!(up.header("x-request-id").is_some());
    let get = download(
        &app,
        TOKEN_ALICE,
        "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e7f",
        &[],
    )
    .await;
    assert_eq!(get.status, 501);
    assert_eq!(get.code(), "service.not_configured");
    // The rest of the API is unaffected.
    let session = send(
        &app,
        request(
            "GET",
            "/api/v1/auth/session",
            &[("authorization", &auth(TOKEN_ALICE))],
            Body::empty(),
        ),
    )
    .await;
    assert_eq!(session.status, 200);
}

// ---------------------------------------------------------------------------
// Retry-After from the port (WP-PLT-01.T7)

const LEAKS: [&str; 4] = ["/var/lib", "s3://", "postgres", "5432"];

fn unavailable(wait: Option<u64>) -> FilePortError {
    let error = FilePortError::new(
        503,
        "server.unavailable",
        "The service is busy; try again shortly.",
        true,
    );
    match wait {
        Some(secs) => error.with_retry_after(secs),
        None => error,
    }
}

fn assert_no_leak(reply: &Reply) {
    let text = reply.text();
    for leak in LEAKS {
        assert!(!text.contains(leak), "leaks `{leak}`: {text}");
    }
}

#[test]
fn the_builder_sets_the_wait_and_never_stores_zero() {
    assert_eq!(unavailable(None).retry_after_secs, None);
    assert_eq!(unavailable(Some(7)).retry_after_secs, Some(7));
    assert_eq!(unavailable(Some(0)).retry_after_secs, Some(1));
}

#[tokio::test]
async fn an_upload_503_carries_the_port_wait() {
    let app = build(true);
    app.port.fail_next_upload(unavailable(Some(7)));
    let reply = upload_octets(&app, TOKEN_ALICE, "a.bin", b"x").await;
    assert_eq!(reply.status, 503, "{}", reply.text());
    assert_eq!(reply.code(), "server.unavailable");
    assert_eq!(reply.header("retry-after"), Some("7"));
    assert_no_leak(&reply);
}

#[tokio::test]
async fn a_download_503_carries_the_port_wait() {
    let app = build(true);
    let id = upload_octets(&app, TOKEN_ALICE, "a.bin", b"x").await.json()["data"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    app.port.fail_next_download(unavailable(Some(7)));
    let reply = download(&app, TOKEN_ALICE, &id, &[]).await;
    assert_eq!(reply.status, 503, "{}", reply.text());
    assert_eq!(reply.code(), "server.unavailable");
    assert_eq!(reply.header("retry-after"), Some("7"));
    assert_no_leak(&reply);
}

#[tokio::test]
async fn a_503_without_a_wait_keeps_the_default_of_one_second() {
    let app = build(true);
    app.port.fail_next_upload(unavailable(None));
    let up = upload_octets(&app, TOKEN_ALICE, "a.bin", b"x").await;
    assert_eq!(up.status, 503);
    assert_eq!(up.header("retry-after"), Some("1"));
    app.port.fail_next_download(unavailable(None));
    let down = download(
        &app,
        TOKEN_ALICE,
        "01900000-0000-7000-8000-000000000001",
        &[],
    )
    .await;
    assert_eq!(down.status, 503);
    assert_eq!(down.header("retry-after"), Some("1"));
    assert_no_leak(&up);
    assert_no_leak(&down);
}

#[tokio::test]
async fn a_stray_wait_on_other_statuses_sends_no_header() {
    let app = build(true);
    // A 404 on download (the one indistinguishable refusal).
    app.port
        .fail_next_download(FilePortError::not_found().with_retry_after(9));
    let missing = download(
        &app,
        TOKEN_ALICE,
        "01900000-0000-7000-8000-000000000001",
        &[],
    )
    .await;
    assert_eq!(missing.status, 404, "{}", missing.text());
    assert_eq!(missing.header("retry-after"), None);
    // A known non-throttle code on upload.
    app.port.fail_next_upload(
        FilePortError::new(422, "file.virus_detected", "Malware found.", false).with_retry_after(9),
    );
    let virus = upload_octets(&app, TOKEN_ALICE, "v.bin", b"x").await;
    assert_eq!(virus.status, 422);
    assert_eq!(virus.header("retry-after"), None);
    assert!(!virus.text().contains("retry_after"), "{}", virus.text());
    assert_no_leak(&missing);
    assert_no_leak(&virus);
}

#[tokio::test]
async fn an_unknown_code_drops_the_wait() {
    let app = build(true);
    app.port.fail_next_upload(
        FilePortError::new(503, "files.disk_on_fire", "boom", true).with_retry_after(9),
    );
    let reply = upload_octets(&app, TOKEN_ALICE, "a.bin", b"x").await;
    assert_eq!(reply.status, 500, "{}", reply.text());
    assert_eq!(reply.code(), "http.internal");
    assert_eq!(reply.header("retry-after"), None);
    assert!(!reply.text().contains("retry_after"), "{}", reply.text());
    assert_no_leak(&reply);
}

#[tokio::test]
async fn a_throttle_429_carries_the_wait_too() {
    let app = build(true);
    app.port.fail_next_upload(
        FilePortError::new(429, "api.rate_limited", "Slow down.", true).with_retry_after(12),
    );
    let reply = upload_octets(&app, TOKEN_ALICE, "a.bin", b"x").await;
    assert_eq!(reply.status, 429, "{}", reply.text());
    assert_eq!(reply.header("retry-after"), Some("12"));
}
