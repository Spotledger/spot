//! The `/api/v1/import*` routes (`import_limits`, `import_create`, `import_get`,
//! `import_errors`, `import_run`, `import_cancel`, WP-DOC-09.T4) over the in-memory
//! [`InMemoryImportPort`]: the shape of every request the routes accept or refuse, every code the
//! contract names for the operations, the creator-only rule, idempotent replay and the 256 byte
//! cell cut. The conformance walk (`tests/conformance.rs`) proves the response shapes against the
//! contract.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::ports::import::{ImportPort, InMemoryImportPort, MemoryFile, MemoryRowError};
use spot_http::api::router::routes::{RouteState, import};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const SITE: &str = "acme";
const ALICE: &str = "0192f3c4-0000-7000-8000-00000000a001";
const BOB: &str = "0192f3c4-0000-7000-8000-00000000b001";
const FILE: &str = "0192f3c4-0000-7000-8000-00000000f001";
const BOB_FILE: &str = "0192f3c4-0000-7000-8000-00000000f002";

struct Fixture {
    router: axum::Router,
    port: Arc<InMemoryImportPort>,
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
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

fn file(rows: u64, errors: Vec<MemoryRowError>) -> MemoryFile {
    MemoryFile {
        rows,
        columns: 3,
        bytes: 1024,
        errors,
    }
}

fn row_error(row: u64, cell: &str, writable: bool) -> MemoryRowError {
    MemoryRowError {
        row,
        column: Some("name".to_owned()),
        cell: cell.to_owned(),
        writable,
        code: "import.bad_value".to_owned(),
    }
}

fn fixture_with(port: InMemoryImportPort) -> Fixture {
    let port = Arc::new(port);
    port.grant_token(SITE, "tok-alice", ALICE);
    port.grant_token(SITE, "tok-bob", BOB);
    port.grant_import(SITE, ALICE, "customer");
    port.grant_import(SITE, BOB, "customer");
    let mut ports = Ports::new();
    ports.provide::<dyn ImportPort>(port.clone());
    let state = RouteState::new(ports.subset(import::required_ports()).unwrap());
    let acme = SiteId::new(SITE).unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(import::routes().with_state(state))
    .build();
    Fixture { router, port }
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(
        time::macros::datetime!(2026-11-12 08:00:00 UTC),
    ));
    let f = fixture_with(InMemoryImportPort::new(clock));
    let long = "x".repeat(600);
    f.port.add_file(
        SITE,
        ALICE,
        FILE,
        file(
            5,
            vec![
                row_error(2, &long, true),
                row_error(3, "secret cell", false),
                row_error(4, "plain", true),
            ],
        ),
    );
    f.port.add_file(SITE, BOB, BOB_FILE, file(1, vec![]));
    f
}

async fn send(
    f: &Fixture,
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<Json>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let body = match body {
        Some(json) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&json).unwrap())
        }
        None => Body::empty(),
    };
    let response = f
        .router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
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

fn create_body() -> Json {
    json!({"file_id": FILE, "doctype": "customer", "mode": "insert"})
}

async fn dry_run(f: &Fixture) -> Json {
    let r = send(
        f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[],
        Some(create_body()),
    )
    .await;
    assert_eq!(r.status, 202, "{}", r.text());
    r.json()["data"].clone()
}

#[tokio::test]
async fn limits_are_served_to_a_signed_in_caller_only() {
    let f = fixture();
    let r = send(
        &f,
        "GET",
        "/api/v1/import/limits",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["data"]["max_rows"], 100_000);
    assert_eq!(
        r.json()["data"]["modes"],
        json!(["insert", "update", "upsert"])
    );
    assert_eq!(
        r.headers.get("cache-control").unwrap().to_str().unwrap(),
        "private, no-store"
    );
    let anonymous = send(&f, "GET", "/api/v1/import/limits", None, &[], None).await;
    assert_eq!(anonymous.status, 401);
    assert_eq!(anonymous.code(), "auth.required");
}

#[tokio::test]
async fn create_answers_202_with_a_ready_job_and_its_digest() {
    let f = fixture();
    let job = dry_run(&f).await;
    assert_eq!(job["state"], "ready");
    assert_eq!(job["doctype"], "customer");
    assert_eq!(job["mode"], "insert");
    assert_eq!(job["created_by"], ALICE);
    assert_eq!(
        job["counts"],
        json!({"rows": 5, "ok": 2, "errors": 3, "skipped": 0})
    );
    assert_eq!(job["digest"].as_str().unwrap().len(), 64);
    assert_eq!(job["progress"], 0);
}

#[tokio::test]
async fn create_refuses_each_malformed_request_before_the_port() {
    let f = fixture();
    let cases: Vec<(&str, Json)> = vec![
        ("no body keys", json!({})),
        (
            "unknown key",
            json!({"file_id": FILE, "doctype": "customer", "mode": "insert", "x": 1}),
        ),
        (
            "bad file id",
            json!({"file_id": "not-an-id", "doctype": "customer", "mode": "insert"}),
        ),
        (
            "bad doctype",
            json!({"file_id": FILE, "doctype": "Not A Name", "mode": "insert"}),
        ),
        (
            "bad mode",
            json!({"file_id": FILE, "doctype": "customer", "mode": "merge"}),
        ),
        (
            "submit not bool",
            json!({"file_id": FILE, "doctype": "customer", "mode": "insert", "submit": "yes"}),
        ),
        (
            "mapping without columns",
            json!({"file_id": FILE, "doctype": "customer", "mode": "insert", "mapping": {}}),
        ),
        (
            "mapping field not a name",
            json!({"file_id": FILE, "doctype": "customer", "mode": "insert", "mapping": {"columns": [{"column": "A", "field": "No Good"}]}}),
        ),
        (
            "too many mapping entries",
            json!({"file_id": FILE, "doctype": "customer", "mode": "insert", "mapping": {"columns": vec![json!({"column": "A", "field": "a"}); 501]}}),
        ),
        ("not an object", json!([1])),
    ];
    for (what, body) in cases {
        let r = send(
            &f,
            "POST",
            "/api/v1/import",
            Some("tok-alice"),
            &[],
            Some(body),
        )
        .await;
        assert_eq!(r.status, 400, "{what}: {}", r.text());
        assert_eq!(r.code(), "api.invalid_request", "{what}");
        // The body is never echoed.
        assert!(!r.text().contains("not-an-id"), "{what}");
    }
}

#[tokio::test]
async fn a_malformed_idempotency_key_is_refused_and_a_replay_returns_the_first_answer() {
    let f = fixture();
    let bad = send(
        &f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[("idempotency-key", "has space")],
        Some(create_body()),
    )
    .await;
    assert_eq!(bad.status, 400, "{}", bad.text());
    assert_eq!(bad.code(), "api.invalid_request");
    let first = send(
        &f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[("idempotency-key", "k-1")],
        Some(create_body()),
    )
    .await;
    let second = send(
        &f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[("idempotency-key", "k-1")],
        Some(create_body()),
    )
    .await;
    assert_eq!(first.status, 202);
    assert_eq!(
        first.json(),
        second.json(),
        "the same key and body replays the first answer"
    );
    let other = json!({"file_id": FILE, "doctype": "customer", "mode": "upsert"});
    let conflict = send(
        &f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[("idempotency-key", "k-1")],
        Some(other),
    )
    .await;
    assert_eq!(conflict.status, 409);
    assert_eq!(conflict.code(), "api.idempotency_conflict");
}

#[tokio::test]
async fn create_maps_the_ports_refusals_to_the_contract_codes() {
    let f = fixture();
    // No import right on the DocType.
    let denied = send(
        &f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[],
        Some(json!({"file_id": FILE, "doctype": "invoice", "mode": "insert"})),
    )
    .await;
    assert_eq!(denied.status, 403);
    assert_eq!(denied.code(), "import.right_missing");
    // Submit without the submit right.
    let mut body = create_body();
    body["submit"] = json!(true);
    let no_submit = send(
        &f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[],
        Some(body),
    )
    .await;
    assert_eq!(no_submit.code(), "import.right_missing");
    // Another user's file is the answer for a file that is not there.
    let foreign = send(
        &f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[],
        Some(json!({"file_id": BOB_FILE, "doctype": "customer", "mode": "insert"})),
    )
    .await;
    assert_eq!(foreign.status, 404);
    assert_eq!(foreign.code(), "document.not_found");
    let missing = send(&f, "POST", "/api/v1/import", Some("tok-alice"), &[], Some(json!({"file_id": "0192f3c4-0000-7000-8000-00000000ffff", "doctype": "customer", "mode": "insert"}))).await;
    assert_eq!(
        foreign.bytes.len(),
        missing.bytes.len(),
        "no existence oracle"
    );
    // A run without a dry run.
    let mut run = create_body();
    run["dry_run"] = json!(false);
    let not_ready = send(
        &f,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[],
        Some(run),
    )
    .await;
    assert_eq!(not_ready.status, 409);
    assert_eq!(not_ready.code(), "import.not_ready");
    // A file over the site's limit.
    let tight = fixture_with(
        InMemoryImportPort::new(Arc::new(ManualClock::new(
            time::macros::datetime!(2026-11-12 08:00:00 UTC),
        )))
        .with_limits(2, 1 << 20, 10),
    );
    tight.port.add_file(SITE, ALICE, FILE, file(5, vec![]));
    let big = send(
        &tight,
        "POST",
        "/api/v1/import",
        Some("tok-alice"),
        &[],
        Some(create_body()),
    )
    .await;
    assert_eq!(big.status, 413);
    assert_eq!(big.code(), "import.limit_exceeded");
}

#[tokio::test]
async fn get_is_the_creators_and_a_foreign_id_is_the_same_404_as_a_missing_one() {
    let f = fixture();
    let job = dry_run(&f).await;
    let id = job["id"].as_str().unwrap();
    let own = send(
        &f,
        "GET",
        &format!("/api/v1/import/{id}"),
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(own.status, 200, "{}", own.text());
    assert_eq!(own.json()["data"], job);
    let foreign = send(
        &f,
        "GET",
        &format!("/api/v1/import/{id}"),
        Some("tok-bob"),
        &[],
        None,
    )
    .await;
    let missing = send(
        &f,
        "GET",
        "/api/v1/import/0192f3c4-0000-7000-8000-0000000fffff",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(foreign.status, 404);
    assert_eq!(foreign.code(), "document.not_found");
    assert_eq!(missing.status, 404);
    assert_eq!(
        foreign.json()["error"]["message"],
        missing.json()["error"]["message"]
    );
    // A malformed id never reaches the port.
    let bad = send(
        &f,
        "GET",
        "/api/v1/import/a%2Fb",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(bad.status, 400);
}

#[tokio::test]
async fn errors_page_with_a_cursor_cut_cells_and_hide_what_the_caller_cannot_write() {
    let f = fixture();
    let id = dry_run(&f).await["id"].as_str().unwrap().to_owned();
    let first = send(
        &f,
        "GET",
        &format!("/api/v1/import/{id}/errors?limit=2"),
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(first.status, 200, "{}", first.text());
    let page = first.json();
    let rows = page["data"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["row"], 2);
    assert_eq!(
        rows[0]["cell_text"].as_str().unwrap().len(),
        256,
        "cut to 256 bytes"
    );
    assert!(
        rows[1].get("cell_text").is_none(),
        "a cell of a field the caller cannot write is absent"
    );
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    let second = send(
        &f,
        "GET",
        &format!("/api/v1/import/{id}/errors?limit=2&cursor={cursor}"),
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(second.json()["data"].as_array().unwrap().len(), 1);
    assert_eq!(second.json()["next_cursor"], Json::Null);
    for bad in [
        "limit=0",
        "limit=201",
        "limit=abc",
        "cursor=a%20b",
        "other=1",
    ] {
        let r = send(
            &f,
            "GET",
            &format!("/api/v1/import/{id}/errors?{bad}"),
            Some("tok-alice"),
            &[],
            None,
        )
        .await;
        assert_eq!(r.status, 400, "{bad}: {}", r.text());
        assert_eq!(r.code(), "api.invalid_request", "{bad}");
    }
    let foreign = send(
        &f,
        "GET",
        &format!("/api/v1/import/{id}/errors"),
        Some("tok-bob"),
        &[],
        None,
    )
    .await;
    assert_eq!(foreign.status, 404);
}

#[tokio::test]
async fn run_needs_the_digest_and_the_ready_state() {
    let f = fixture();
    let job = dry_run(&f).await;
    let id = job["id"].as_str().unwrap();
    let path = format!("/api/v1/import/{id}/run");
    for (what, body) in [
        ("no digest", json!({})),
        ("short digest", json!({"digest": "abc"})),
        ("non hex digest", json!({"digest": "z".repeat(64)})),
    ] {
        let r = send(&f, "POST", &path, Some("tok-alice"), &[], Some(body)).await;
        assert_eq!(r.status, 400, "{what}: {}", r.text());
    }
    let wrong = send(
        &f,
        "POST",
        &path,
        Some("tok-alice"),
        &[],
        Some(json!({"digest": "0".repeat(64)})),
    )
    .await;
    assert_eq!(wrong.status, 409);
    assert_eq!(wrong.code(), "import.digest_changed");
    let foreign = send(
        &f,
        "POST",
        &path,
        Some("tok-bob"),
        &[],
        Some(json!({"digest": job["digest"]})),
    )
    .await;
    assert_eq!(foreign.status, 404);
    let ok = send(
        &f,
        "POST",
        &path,
        Some("tok-alice"),
        &[("idempotency-key", "run-1")],
        Some(json!({"digest": job["digest"]})),
    )
    .await;
    assert_eq!(ok.status, 202, "{}", ok.text());
    assert_eq!(ok.json()["data"]["state"], "running");
    let replay = send(
        &f,
        "POST",
        &path,
        Some("tok-alice"),
        &[("idempotency-key", "run-1")],
        Some(json!({"digest": job["digest"]})),
    )
    .await;
    assert_eq!(replay.json(), ok.json(), "the replay is the first answer");
    let again = send(
        &f,
        "POST",
        &path,
        Some("tok-alice"),
        &[],
        Some(json!({"digest": job["digest"]})),
    )
    .await;
    assert_eq!(again.status, 409);
    assert_eq!(again.code(), "import.not_ready");
}

#[tokio::test]
async fn cancel_is_the_creators_stops_the_job_and_a_run_after_it_is_cancelled() {
    let f = fixture();
    let job = dry_run(&f).await;
    let id = job["id"].as_str().unwrap();
    let path = format!("/api/v1/import/{id}/cancel");
    let foreign = send(&f, "POST", &path, Some("tok-bob"), &[], None).await;
    assert_eq!(foreign.status, 404);
    let ok = send(&f, "POST", &path, Some("tok-alice"), &[], None).await;
    assert_eq!(ok.status, 200, "{}", ok.text());
    assert_eq!(ok.json()["data"]["state"], "cancelled");
    let repeat = send(&f, "POST", &path, Some("tok-alice"), &[], None).await;
    assert_eq!(repeat.status, 200, "a repeat is a repeat of the action");
    let run = send(
        &f,
        "POST",
        &format!("/api/v1/import/{id}/run"),
        Some("tok-alice"),
        &[],
        Some(json!({"digest": job["digest"]})),
    )
    .await;
    assert_eq!(run.status, 409);
    assert_eq!(run.code(), "import.cancelled");
}

#[tokio::test]
async fn a_port_error_outside_the_contract_is_an_internal_error_with_no_detail() {
    use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
    struct Broken;
    impl ImportPort for Broken {
        fn limits<'a>(
            &'a self,
            _: &'a RequestCtx,
        ) -> BoxFuture<'a, Result<spot_http::api::ports::import::ImportLimitsOut, ApiError>>
        {
            Box::pin(async { Err(ApiError::app("boom.secret_detail", "db password hunter2")) })
        }
        fn create<'a>(
            &'a self,
            _: &'a RequestCtx,
            _: spot_http::api::ports::import::ImportCreateIn,
        ) -> BoxFuture<'a, Result<spot_http::api::ports::import::ImportJobOut, ApiError>> {
            Box::pin(async { Err(ApiError::app("boom.secret_detail", "x")) })
        }
        fn get<'a>(
            &'a self,
            _: &'a RequestCtx,
            _: &'a str,
        ) -> BoxFuture<'a, Result<spot_http::api::ports::import::ImportJobOut, ApiError>> {
            Box::pin(async { Err(ApiError::app("boom.secret_detail", "x")) })
        }
        fn errors<'a>(
            &'a self,
            _: &'a RequestCtx,
            _: &'a str,
            _: Option<&'a str>,
            _: u32,
        ) -> BoxFuture<'a, Result<spot_http::api::ports::import::ImportErrorsPageOut, ApiError>>
        {
            Box::pin(async { Err(ApiError::app("boom.secret_detail", "x")) })
        }
        fn run<'a>(
            &'a self,
            _: &'a RequestCtx,
            _: &'a str,
            _: &'a str,
        ) -> BoxFuture<'a, Result<spot_http::api::ports::import::ImportJobOut, ApiError>> {
            Box::pin(async { Err(ApiError::app("boom.secret_detail", "x")) })
        }
        fn cancel<'a>(
            &'a self,
            _: &'a RequestCtx,
            _: &'a str,
        ) -> BoxFuture<'a, Result<spot_http::api::ports::import::ImportJobOut, ApiError>> {
            Box::pin(async { Err(ApiError::app("boom.secret_detail", "x")) })
        }
    }
    let mut ports = Ports::new();
    ports.provide::<dyn ImportPort>(Arc::new(Broken));
    let state = RouteState::new(ports.subset(import::required_ports()).unwrap());
    let acme = SiteId::new(SITE).unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(import::routes().with_state(state))
    .build();
    let f = Fixture {
        router,
        port: Arc::new(InMemoryImportPort::new(Arc::new(ManualClock::new(
            time::macros::datetime!(2026-11-12 08:00:00 UTC),
        )))),
    };
    let r = send(
        &f,
        "GET",
        "/api/v1/import/limits",
        Some("tok-alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 500, "{}", r.text());
    assert_eq!(r.code(), "http.internal");
    assert!(!r.text().contains("hunter2") && !r.text().contains("boom"));
}
