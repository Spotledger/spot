//! The `/api/v1/export*` routes (contract 0.4.0, WP-INT-03.T2) over a fake [`BulkPort`]: every
//! status and error code the contract names for `export_start`, `export_next` and
//! `export_extract`, the shape of every request the routes accept or refuse, and the framing of
//! a chunk (plain, and `multipart/mixed` for child tables).
//!
//! The fake decides nothing the real adapter decides; it answers what a test scripts and keeps
//! the arguments the routes passed, so the tests prove the routes' half of the bargain: what is
//! checked before the port, what is passed to it and how its answer becomes the response.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::ports::export::{
    BulkPort, BulkResult, ExportChunkOut, ExportFormatIn, ExportPartOut, ExportStartIn, ExtractIn,
    ExtractPageOut, OrderKeyIn,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{RouteState, export};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const TOKEN: &str = "tok-alice";
const CURSOR: &str = "AQIDBAUGBwgJCg-_AAECAwQFBgcICQo";
const WATERMARK: &str = "wm1.eyJ2IjoxfQ.c2lnbmVk";

#[derive(Default)]
struct Script {
    /// The next call fails with this.
    fail: Option<ApiError>,
    /// What `start` and `next` answer.
    chunk: Option<ExportChunkOut>,
    /// What `extract` answers.
    page: Option<ExtractPageOut>,
    /// Calls the port received: `(operation, detail)`.
    calls: Vec<(&'static str, String)>,
    /// The credential token of each call.
    tokens: Vec<String>,
}

struct Fake {
    script: Mutex<Script>,
}

impl Fake {
    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn record(&self, ctx: &RequestCtx, op: &'static str, detail: String) -> BulkResult<()> {
        let mut script = self.lock();
        script.tokens.push(
            ctx.credential
                .as_ref()
                .map(|c| c.token.clone())
                .unwrap_or_default(),
        );
        script.calls.push((op, detail));
        match script.fail.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn plain(rows: u64, cursor: Option<&str>) -> ExportChunkOut {
    ExportChunkOut {
        format: ExportFormatIn::Ndjson,
        multipart: false,
        parts: vec![ExportPartOut {
            name: "project".to_owned(),
            bytes: b"{\"code\":\"A\"}\n{\"code\":\"B\"}\n".to_vec(),
        }],
        cursor: cursor.map(str::to_owned),
        rows,
        watermark: WATERMARK.to_owned(),
    }
}

impl BulkPort for Fake {
    fn start<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        spec: ExportStartIn,
    ) -> BoxFuture<'a, BulkResult<ExportChunkOut>> {
        Box::pin(async move {
            self.record(ctx, "start", format!("{spec:?}"))?;
            Ok(self
                .lock()
                .chunk
                .clone()
                .unwrap_or_else(|| plain(2, Some(CURSOR))))
        })
    }

    fn next<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        cursor: &'a str,
    ) -> BoxFuture<'a, BulkResult<ExportChunkOut>> {
        Box::pin(async move {
            self.record(ctx, "next", cursor.to_owned())?;
            Ok(self.lock().chunk.clone().unwrap_or_else(|| plain(2, None)))
        })
    }

    fn extract<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: ExtractIn,
    ) -> BoxFuture<'a, BulkResult<ExtractPageOut>> {
        Box::pin(async move {
            self.record(ctx, "extract", format!("{request:?}"))?;
            Ok(self.lock().page.clone().unwrap_or_else(|| ExtractPageOut {
                upserts: vec![json!({"id": "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f", "version": 4, "code": "A"})],
                deletes: vec!["0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e99".to_owned()],
                new_watermark: WATERMARK.to_owned(),
                has_more: false,
            }))
        })
    }
}

struct Fixture {
    router: axum::Router,
    fake: Arc<Fake>,
}

fn fixture() -> Fixture {
    let fake = Arc::new(Fake {
        script: Mutex::new(Script::default()),
    });
    let mut ports = Ports::new();
    ports.provide::<dyn BulkPort>(fake.clone());
    let state = RouteState::new(ports.subset(export::export_required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(export::export_routes().with_state(state))
    .build();
    Fixture { router, fake }
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    bytes: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_slice(&self.bytes).unwrap_or(Json::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
    fn text(&self) -> String {
        String::from_utf8(self.bytes.clone()).unwrap()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn send(
    f: &Fixture,
    path: &str,
    token: Option<&str>,
    content_type: Option<&str>,
    body: Option<Json>,
) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    let request = builder
        .body(match body {
            Some(json) => Body::from(json.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let response = f.router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
    }
}

async fn call(f: &Fixture, path: &str, body: Json) -> Reply {
    send(f, path, Some(TOKEN), Some("application/json"), Some(body)).await
}

fn start_body() -> Json {
    json!({
        "doctype": "project",
        "columns": ["code", "status"],
        "filter": {"field": "status", "op": "eq", "value": "Active"},
        "order": [{"field": "code", "dir": "desc", "nulls": "first"}, {"field": "title"}],
        "format": "ndjson",
        "include_children": false,
        "max_rows": 1000,
    })
}

fn fail_next(f: &Fixture, error: ApiError) {
    f.fake.lock().fail = Some(error);
}

fn calls(f: &Fixture) -> usize {
    f.fake.lock().calls.len()
}

// ------------------------------------------------------------------------------------------
// export_start

#[tokio::test]
async fn start_passes_the_parsed_request_and_answers_the_chunk_with_its_headers() {
    let f = fixture();
    let r = call(&f, "/api/v1/export", start_body()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("content-type"), Some("application/x-ndjson"));
    assert_eq!(r.header("x-export-rows"), Some("2"));
    assert_eq!(r.header("x-export-cursor"), Some(CURSOR));
    assert_eq!(r.header("x-export-watermark"), Some(WATERMARK));
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert!(r.header("x-request-id").is_some());
    assert_eq!(r.text(), "{\"code\":\"A\"}\n{\"code\":\"B\"}\n");
    let (op, detail) = f.fake.lock().calls.last().cloned().unwrap();
    assert_eq!(op, "start");
    for needle in [
        "doctype: \"project\"",
        "columns: [\"code\", \"status\"]",
        "descending: true",
        "nulls_first: Some(true)",
        "format: Ndjson",
        "max_rows: Some(1000)",
    ] {
        assert!(detail.contains(needle), "{needle} missing from {detail}");
    }
    assert_eq!(f.fake.lock().tokens, vec![TOKEN.to_owned()]);
}

#[tokio::test]
async fn the_last_chunk_has_no_cursor_and_a_csv_chunk_is_csv() {
    let f = fixture();
    f.fake.lock().chunk = Some(ExportChunkOut {
        format: ExportFormatIn::Csv,
        multipart: false,
        parts: vec![ExportPartOut {
            name: "project".to_owned(),
            bytes: b"code\r\nA\r\n".to_vec(),
        }],
        cursor: None,
        rows: 1,
        watermark: WATERMARK.to_owned(),
    });
    let r = call(
        &f,
        "/api/v1/export",
        json!({"doctype": "project", "format": "csv"}),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("content-type"), Some("text/csv; charset=utf-8"));
    assert!(r.header("x-export-cursor").is_none());
    assert_eq!(r.text(), "code\r\nA\r\n");
}

#[tokio::test]
async fn child_tables_come_as_multipart_with_a_boundary_no_cell_contains() {
    let f = fixture();
    // A cell that holds the boundary an attacker might guess still cannot end a part early.
    let hostile = b"{\"code\":\"--spot-export-0\"}\n".to_vec();
    let chunk = ExportChunkOut {
        format: ExportFormatIn::Ndjson,
        multipart: true,
        parts: vec![
            ExportPartOut {
                name: "task".to_owned(),
                bytes: hostile,
            },
            ExportPartOut {
                name: "checklist".to_owned(),
                bytes: b"{\"parent\":\"x\"}\n".to_vec(),
            },
        ],
        cursor: Some(CURSOR.to_owned()),
        rows: 1,
        watermark: WATERMARK.to_owned(),
    };
    f.fake.lock().chunk = Some(chunk);
    let body = json!({"doctype": "task", "format": "ndjson", "include_children": true});
    let first = call(&f, "/api/v1/export", body.clone()).await;
    let second = call(&f, "/api/v1/export", body).await;
    assert_eq!(first.status, 200, "{}", first.text());
    assert_eq!(
        first.bytes, second.bytes,
        "the same chunk is the same bytes"
    );
    let content_type = first.header("content-type").unwrap().to_owned();
    let boundary = content_type
        .strip_prefix("multipart/mixed; boundary=")
        .expect("multipart/mixed")
        .to_owned();
    let text = first.text();
    assert_eq!(text.matches(&format!("--{boundary}\r\n")).count(), 2);
    assert!(text.ends_with(&format!("--{boundary}--\r\n")));
    assert!(text.contains("Content-Disposition: attachment; name=\"task\"\r\n"));
    assert!(text.contains("Content-Disposition: attachment; name=\"checklist\"\r\n"));
    assert!(text.contains("Content-Type: application/x-ndjson\r\n"));
    // The parts hold no boundary line of their own.
    for part in ["{\"code\":\"--spot-export-0\"}", "{\"parent\":\"x\"}"] {
        assert!(!part.contains(&boundary));
    }
}

#[tokio::test]
async fn a_malformed_start_never_reaches_the_port() {
    let f = fixture();
    let many: Vec<String> = (0..257).map(|n| format!("c{n}")).collect();
    let keys: Vec<Json> = (0..9).map(|_| json!({"field": "code"})).collect();
    let bodies = [
        json!({"format": "csv"}),
        json!({"doctype": "project"}),
        json!({"doctype": "project", "format": "xml"}),
        json!({"doctype": "Not A Name", "format": "csv"}),
        json!({"doctype": "project", "format": "csv", "surprise": 1}),
        json!({"doctype": "project", "format": "csv", "columns": "code"}),
        json!({"doctype": "project", "format": "csv", "columns": many}),
        json!({"doctype": "project", "format": "csv", "columns": ["code", "code"]}),
        json!({"doctype": "project", "format": "csv", "columns": ["bad name"]}),
        json!({"doctype": "project", "format": "csv", "columns": [1]}),
        json!({"doctype": "project", "format": "csv", "order": keys}),
        json!({"doctype": "project", "format": "csv", "order": [{"field": "code", "dir": "sideways"}]}),
        json!({"doctype": "project", "format": "csv", "order": [{"field": "code", "nulls": "middle"}]}),
        json!({"doctype": "project", "format": "csv", "order": [{"dir": "asc"}]}),
        json!({"doctype": "project", "format": "csv", "order": [{"field": "code", "extra": 1}]}),
        json!({"doctype": "project", "format": "csv", "filter": "status = Active"}),
        json!({"doctype": "project", "format": "csv", "max_rows": 0}),
        json!({"doctype": "project", "format": "csv", "max_rows": "many"}),
        json!({"doctype": "project", "format": "csv", "include_children": "yes"}),
        json!([]),
    ];
    for body in bodies {
        let r = call(&f, "/api/v1/export", body.clone()).await;
        assert_eq!(r.status, 400, "{body}: {}", r.text());
        assert_eq!(r.code(), "api.invalid_request", "{body}");
    }
    let no_body = send(
        &f,
        "/api/v1/export",
        Some(TOKEN),
        Some("application/json"),
        None,
    )
    .await;
    assert_eq!(no_body.status, 400);
    let wrong_type = send(
        &f,
        "/api/v1/export",
        Some(TOKEN),
        Some("text/plain"),
        Some(start_body()),
    )
    .await;
    assert_eq!(wrong_type.status, 415);
    assert_eq!(calls(&f), 0, "the port was reached by a malformed request");
}

#[tokio::test]
async fn every_error_the_contract_names_has_its_status() {
    let f = fixture();
    let limit = ApiError::from_code(Code::BulkLimit).detail("retry_after", json!(3));
    let cases: Vec<(ApiError, u16, &str)> = vec![
        (
            ApiError::from_code(Code::PermissionDenied),
            403,
            "permission.denied",
        ),
        (
            ApiError::new(
                Code::PermissionFieldDenied,
                "permission.field_denied: details",
            ),
            403,
            "permission.field_denied",
        ),
        (
            ApiError::from_code(Code::BulkCursorInvalid),
            422,
            "bulk.cursor_invalid",
        ),
        (
            ApiError::from_code(Code::BulkWatermarkExpired),
            410,
            "bulk.watermark_expired",
        ),
        (limit, 429, "bulk.limit"),
        (
            ApiError::from_code(Code::ServerUnavailable).detail("retry_after", json!(5)),
            503,
            "server.unavailable",
        ),
        (
            ApiError::from_code(Code::MetaUnknownDoctype),
            404,
            "meta.unknown_doctype",
        ),
        (
            ApiError::from_code(Code::DocumentUnknownField),
            422,
            "document.unknown_field",
        ),
        (
            ApiError::new(Code::ApiInvalidRequest, "max_rows").detail("max_rows", json!(10)),
            400,
            "api.invalid_request",
        ),
        (
            ApiError::from_code(Code::AuthRequired),
            401,
            "auth.required",
        ),
    ];
    for (error, status, code) in cases {
        fail_next(&f, error);
        let r = call(&f, "/api/v1/export", start_body()).await;
        assert_eq!(
            (r.status, r.code().as_str()),
            (status, code),
            "{}",
            r.text()
        );
        assert!(r.header("x-export-cursor").is_none());
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
        if status == 429 {
            assert_eq!(r.header("retry-after"), Some("3"));
        }
    }
    fail_next(
        &f,
        ApiError::new(Code::ApiInvalidRequest, "max_rows").detail("max_rows", json!(10)),
    );
    let capped = call(&f, "/api/v1/export", start_body()).await;
    assert_eq!(capped.json()["error"]["details"]["max_rows"], 10);
}

// ------------------------------------------------------------------------------------------
// export_next

#[tokio::test]
async fn next_passes_the_cursor_verbatim_and_answers_the_next_chunk() {
    let f = fixture();
    let r = call(&f, "/api/v1/export/next", json!({"cursor": CURSOR})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("x-export-watermark"), Some(WATERMARK));
    assert!(r.header("x-export-cursor").is_none());
    assert_eq!(
        f.fake.lock().calls.last().cloned().unwrap(),
        ("next", CURSOR.to_owned())
    );
}

#[tokio::test]
async fn a_cursor_of_the_wrong_shape_is_the_same_answer_as_a_forged_one() {
    let f = fixture();
    let long = "A".repeat(2049);
    for cursor in [
        json!(""),
        json!("has space"),
        json!("quote\""),
        json!("sl/ash"),
        json!(long),
        json!(7),
        Json::Null,
    ] {
        let r = call(&f, "/api/v1/export/next", json!({"cursor": cursor})).await;
        assert_eq!(r.status, 422, "{cursor}: {}", r.text());
        assert_eq!(r.code(), "bulk.cursor_invalid");
    }
    for body in [json!({}), json!({"cursor": CURSOR, "x": 1}), json!("x")] {
        let r = call(&f, "/api/v1/export/next", body.clone()).await;
        assert_eq!(r.status, 400, "{body}: {}", r.text());
    }
    assert_eq!(calls(&f), 0);
    // A foreign or expired cursor is the port's call and is the same 422.
    fail_next(&f, ApiError::from_code(Code::BulkCursorInvalid));
    let r = call(&f, "/api/v1/export/next", json!({"cursor": CURSOR})).await;
    assert_eq!((r.status, r.code().as_str()), (422, "bulk.cursor_invalid"));
    assert!(!r.text().contains(CURSOR), "an error never echoes a cursor");
    // A caller who lost the right is a 403 from the port.
    fail_next(&f, ApiError::from_code(Code::PermissionDenied));
    let r = call(&f, "/api/v1/export/next", json!({"cursor": CURSOR})).await;
    assert_eq!(r.status, 403);
    // The concurrency limit.
    fail_next(
        &f,
        ApiError::from_code(Code::BulkLimit).detail("retry_after", json!(1)),
    );
    let r = call(&f, "/api/v1/export/next", json!({"cursor": CURSOR})).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.header("retry-after"), Some("1"));
}

// ------------------------------------------------------------------------------------------
// export_extract

#[tokio::test]
async fn extract_answers_a_page_in_the_contract_shape() {
    let f = fixture();
    let r = call(
        &f,
        "/api/v1/export/extract",
        json!({"doctype": "project", "columns": ["code"], "watermark": WATERMARK, "limit": 500}),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let data = &r.json()["data"];
    assert_eq!(data["upserts"][0]["version"], 4);
    assert_eq!(data["upserts"][0]["code"], "A");
    assert_eq!(data["deletes"][0], "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e99");
    assert_eq!(data["new_watermark"], WATERMARK);
    assert_eq!(data["has_more"], false);
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    let (op, detail) = f.fake.lock().calls.last().cloned().unwrap();
    assert_eq!(op, "extract");
    assert!(
        detail.contains("limit: 500") && detail.contains("columns: [\"code\"]"),
        "{detail}"
    );
    // The default limit is 1,000.
    call(
        &f,
        "/api/v1/export/extract",
        json!({"doctype": "project", "watermark": WATERMARK}),
    )
    .await;
    assert!(
        f.fake
            .lock()
            .calls
            .last()
            .unwrap()
            .1
            .contains("limit: 1000")
    );
}

#[tokio::test]
async fn a_malformed_extract_never_reaches_the_port() {
    let f = fixture();
    for body in [
        json!({"watermark": WATERMARK}),
        json!({"doctype": "project"}),
        json!({"doctype": "project", "watermark": WATERMARK, "limit": 0}),
        json!({"doctype": "project", "watermark": WATERMARK, "limit": 5001}),
        json!({"doctype": "project", "watermark": WATERMARK, "limit": "many"}),
        json!({"doctype": "Bad Name", "watermark": WATERMARK}),
        json!({"doctype": "project", "watermark": WATERMARK, "columns": "code"}),
        json!({"doctype": "project", "watermark": WATERMARK, "since": 1}),
    ] {
        let r = call(&f, "/api/v1/export/extract", body.clone()).await;
        assert!(r.status == 400 || r.status == 422, "{body}: {}", r.status);
    }
    for mark in [
        json!(""),
        json!("has space"),
        json!("A".repeat(2049)),
        json!(3),
    ] {
        let r = call(
            &f,
            "/api/v1/export/extract",
            json!({"doctype": "project", "watermark": mark}),
        )
        .await;
        assert_eq!(
            (r.status, r.code().as_str()),
            (422, "bulk.cursor_invalid"),
            "{mark}"
        );
    }
    assert_eq!(calls(&f), 0);
    for (error, status, code) in [
        (
            ApiError::from_code(Code::BulkWatermarkExpired),
            410,
            "bulk.watermark_expired",
        ),
        (
            ApiError::from_code(Code::BulkCursorInvalid),
            422,
            "bulk.cursor_invalid",
        ),
        (
            ApiError::from_code(Code::PermissionDenied),
            403,
            "permission.denied",
        ),
        (
            ApiError::from_code(Code::BulkLimit).detail("retry_after", json!(2)),
            429,
            "bulk.limit",
        ),
    ] {
        fail_next(&f, error);
        let r = call(
            &f,
            "/api/v1/export/extract",
            json!({"doctype": "project", "watermark": WATERMARK}),
        )
        .await;
        assert_eq!((r.status, r.code().as_str()), (status, code));
    }
}

#[tokio::test]
async fn every_operation_needs_a_bearer_and_never_reaches_the_port_without_one() {
    let f = fixture();
    for (path, body) in [
        ("/api/v1/export", start_body()),
        ("/api/v1/export/next", json!({"cursor": CURSOR})),
        (
            "/api/v1/export/extract",
            json!({"doctype": "project", "watermark": WATERMARK}),
        ),
    ] {
        let r = send(&f, path, None, Some("application/json"), Some(body)).await;
        assert_eq!(r.status, 401, "{path}");
    }
    assert_eq!(calls(&f), 0, "the port was reached without a credential");
}

#[tokio::test]
async fn the_order_key_shape_reaches_the_port_as_typed_fields() {
    // The typed shape, for a host that reads it: ascending and the default nulls are `None`.
    let key = OrderKeyIn {
        field: "code".to_owned(),
        descending: false,
        nulls_first: None,
    };
    assert_eq!(key.field, "code");
    let f = fixture();
    call(
        &f,
        "/api/v1/export",
        json!({"doctype": "project", "format": "csv", "order": [{"field": "code"}]}),
    )
    .await;
    let detail = f.fake.lock().calls.last().unwrap().1.clone();
    assert!(
        detail.contains("descending: false") && detail.contains("nulls_first: None"),
        "{detail}"
    );
}
