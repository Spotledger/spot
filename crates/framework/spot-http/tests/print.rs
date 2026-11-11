//! `print_get` over the wire (WP-PLT-13.T2, contract 0.7.0): the PDF response and its headers,
//! omitted and named formats, the one indistinguishable refusal, authentication, the
//! unconfigured service, and errors that never carry template text or document content.

#[allow(dead_code)]
mod support;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, header};
use serde_json::Value as Json;
use spot_core::{ManualClock, SiteId};
use spot_http::api::codec::parse_record_id;
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::print::{MemoryPrintPort, PrintPortError};
use spot_http::api::router::{ApiServices, router};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_storage_contract::RecordId;
use support::Must;
use support::fake::{FakeApp, TOKEN_ALICE, TOKEN_BOB};
use time::OffsetDateTime;
use tower::ServiceExt;

const HOST: &str = "erp.acme.com";
const DOC: &str = "01890a5d-ac96-774b-bcce-b302099a8057";
const OTHER_DOC: &str = "01890a5d-ac96-774b-bcce-b302099a8058";

struct Print {
    router: axum::Router,
    port: Arc<MemoryPrintPort>,
}

fn id(raw: &str) -> RecordId {
    parse_record_id(raw).must()
}

fn build_with(port: Arc<MemoryPrintPort>, configured: bool) -> Print {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    let mut services = ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store);
    if configured {
        services = services.with_print(port.clone());
    }
    let site = SiteId::new("acme").must();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build();
    Print { router, port }
}

/// A port with an invoice only alice may read; its formats are `standard` (the default) and
/// `compact`.
fn printing() -> Print {
    let port = Arc::new(MemoryPrintPort::new());
    port.add_document("sales_invoice", id(DOC), &["standard", "compact"]);
    port.allow(TOKEN_ALICE, "sales_invoice", id(DOC));
    build_with(port, true)
}

fn unconfigured() -> Print {
    build_with(Arc::new(MemoryPrintPort::new()), false)
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
    /// The error envelope without the per-request id, for comparing two refusals.
    fn error_shape(&self) -> Json {
        let mut error = self.json()["error"].clone();
        if let Some(map) = error.as_object_mut() {
            map.remove("request_id");
        }
        error
    }
}

async fn get(app: &Print, path: &str, token: Option<&str>, extra: &[(&str, &str)]) -> Reply {
    let mut builder = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let response = app
        .router
        .clone()
        .oneshot(builder.body(Body::empty()).must())
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

fn path(doctype: &str, doc: &str) -> String {
    format!("/api/v1/print/{doctype}/{doc}")
}

#[tokio::test]
async fn a_print_is_a_pdf_with_the_contract_headers() {
    let app = printing();
    let reply = get(&app, &path("sales_invoice", DOC), Some(TOKEN_ALICE), &[]).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert!(
        reply.bytes.starts_with(b"%PDF-"),
        "the body is PDF bytes, not JSON or base64"
    );
    assert_eq!(reply.header("content-type"), Some("application/pdf"));
    assert_eq!(
        reply.header("content-disposition"),
        Some(format!("inline; filename=\"sales_invoice-{DOC}.pdf\"").as_str())
    );
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    assert!(reply.header("x-request-id").is_some());
    for forbidden in ["etag", "accept-ranges", "content-range", "last-modified"] {
        assert!(
            reply.header(forbidden).is_none(),
            "this version has no {forbidden}"
        );
    }
}

#[tokio::test]
async fn omitting_format_prints_the_default_and_a_name_selects_that_format() {
    let app = printing();
    let default = get(&app, &path("sales_invoice", DOC), Some(TOKEN_ALICE), &[]).await;
    assert!(default.text().contains("format=standard"));
    let compact = get(
        &app,
        &format!("{}?format=compact", path("sales_invoice", DOC)),
        Some(TOKEN_ALICE),
        &[],
    )
    .await;
    assert_eq!(compact.status, 200);
    assert!(compact.text().contains("format=compact"));
    assert_eq!(
        compact.header("content-disposition"),
        default.header("content-disposition"),
        "the file name never depends on the format"
    );
}

#[tokio::test]
async fn an_unknown_format_is_print_format_unknown() {
    let app = printing();
    let reply = get(
        &app,
        &format!("{}?format=nonesuch", path("sales_invoice", DOC)),
        Some(TOKEN_ALICE),
        &[],
    )
    .await;
    assert_eq!(reply.status, 404);
    assert_eq!(reply.code(), "print.format_unknown");
    assert_eq!(reply.json()["error"]["retryable"], false);
}

#[tokio::test]
async fn missing_forbidden_and_unknown_doctype_are_the_same_answer() {
    let app = printing();
    // Bob cannot read the invoice; OTHER_DOC does not exist; `ghost` is no DocType.
    let forbidden = get(&app, &path("sales_invoice", DOC), Some(TOKEN_BOB), &[]).await;
    let missing = get(
        &app,
        &path("sales_invoice", OTHER_DOC),
        Some(TOKEN_ALICE),
        &[],
    )
    .await;
    let unknown = get(&app, &path("ghost", DOC), Some(TOKEN_ALICE), &[]).await;
    for reply in [&forbidden, &missing, &unknown] {
        assert_eq!(reply.status, 404, "{}", reply.text());
        assert_eq!(reply.code(), "document.not_found");
        assert!(
            reply
                .header("content-type")
                .is_some_and(|t| t.starts_with("application/json"))
        );
    }
    assert_eq!(forbidden.error_shape(), missing.error_shape());
    assert_eq!(forbidden.error_shape(), unknown.error_shape());
    assert_eq!(forbidden.status, missing.status);
}

#[tokio::test]
async fn the_permission_check_comes_before_the_format_check() {
    let app = printing();
    // Bob may not read the invoice: an unknown format must not turn the refusal into a
    // format error that would show the document exists.
    let reply = get(
        &app,
        &format!("{}?format=nonesuch", path("sales_invoice", DOC)),
        Some(TOKEN_BOB),
        &[],
    )
    .await;
    assert_eq!(reply.status, 404);
    assert_eq!(reply.code(), "document.not_found");
    assert_eq!(app.port.render_count(), 0);
}

#[tokio::test]
async fn a_request_without_a_credential_is_401() {
    let app = printing();
    let reply = get(&app, &path("sales_invoice", DOC), None, &[]).await;
    assert_eq!(reply.status, 401);
    assert_eq!(reply.code(), "auth.required");
    assert_eq!(
        app.port.render_count(),
        0,
        "nothing rendered unauthenticated"
    );
}

#[tokio::test]
async fn an_unconfigured_service_answers_501_service_not_configured() {
    let app = unconfigured();
    let reply = get(&app, &path("sales_invoice", DOC), Some(TOKEN_ALICE), &[]).await;
    assert_eq!(reply.status, 501);
    assert_eq!(reply.code(), "service.not_configured");
    assert_eq!(reply.json()["error"]["retryable"], false);
    assert!(reply.json()["error"]["request_id"].is_string());
    // Authentication still comes first.
    let anonymous = get(&app, &path("sales_invoice", DOC), None, &[]).await;
    assert_eq!(anonymous.status, 401);
}

#[tokio::test]
async fn a_bad_path_or_query_is_an_invalid_request() {
    let app = printing();
    for uri in [
        path("Sales_Invoice", DOC),
        path("sales_invoice", "not-a-uuid"),
        format!("{}?format=Has-Caps", path("sales_invoice", DOC)),
        format!("{}?format=", path("sales_invoice", DOC)),
        format!(
            "{}?format=standard&format=compact",
            path("sales_invoice", DOC)
        ),
        format!("{}?paper=a3", path("sales_invoice", DOC)),
        format!("{}?format=standard&copies=2", path("sales_invoice", DOC)),
    ] {
        let reply = get(&app, &uri, Some(TOKEN_ALICE), &[]).await;
        assert_eq!(reply.status, 400, "{uri}: {}", reply.text());
        assert_eq!(reply.code(), "api.invalid_request", "{uri}");
    }
    assert_eq!(app.port.render_count(), 0);
}

#[tokio::test]
async fn conditional_and_range_headers_are_ignored() {
    let app = printing();
    let reply = get(
        &app,
        &path("sales_invoice", DOC),
        Some(TOKEN_ALICE),
        &[("if-none-match", "*"), ("range", "bytes=0-3")],
    )
    .await;
    assert_eq!(
        reply.status, 200,
        "no 304 and no 206: a print is a fresh render"
    );
    assert!(reply.bytes.len() > 4);
}

#[tokio::test]
async fn a_failed_render_is_500_and_names_neither_template_nor_content() {
    let app = printing();
    app.port.fail_next(PrintPortError::new(
        500,
        "print.render_failed",
        "template {{ doc.customer_phone }} failed on ACME Holdings 0300-1234567",
        false,
    ));
    let reply = get(&app, &path("sales_invoice", DOC), Some(TOKEN_ALICE), &[]).await;
    assert_eq!(reply.status, 500);
    assert_eq!(reply.code(), "print.render_failed");
    assert_eq!(reply.json()["error"]["retryable"], false);
    let text = reply.text();
    for leaked in ["{{", "doc.customer_phone", "ACME", "0300"] {
        assert!(
            !text.contains(leaked),
            "the error leaked `{leaked}`: {text}"
        );
    }
    assert!(reply.json()["error"]["details"].is_null());

    // An unregistered code from the port is also a failed render, with no port text.
    app.port.fail_next(PrintPortError::new(
        500,
        "engine.secret_detail",
        "row {customer: ACME} could not be laid out",
        true,
    ));
    let other = get(&app, &path("sales_invoice", DOC), Some(TOKEN_ALICE), &[]).await;
    assert_eq!(other.status, 500);
    assert_eq!(other.code(), "print.render_failed");
    assert!(!other.text().contains("ACME"));
    assert!(!other.text().contains("engine.secret_detail"));
}

#[tokio::test]
async fn a_port_refusal_with_its_own_message_is_still_the_one_not_found() {
    let app = printing();
    app.port.fail_next(PrintPortError::new(
        403,
        "permission.denied",
        "alice may not read field customer_phone of ACME",
        false,
    ));
    let reply = get(&app, &path("sales_invoice", DOC), Some(TOKEN_ALICE), &[]).await;
    assert_eq!(reply.status, 404);
    assert_eq!(reply.code(), "document.not_found");
    assert!(!reply.text().contains("ACME"));
}

#[tokio::test]
async fn a_port_that_returns_something_that_is_not_a_pdf_is_a_failed_render() {
    let port = Arc::new(MemoryPrintPort::new());
    let app = build_with_raw(port);
    let reply = get(&app, &path("sales_invoice", DOC), Some(TOKEN_ALICE), &[]).await;
    assert_eq!(reply.status, 500);
    assert_eq!(reply.code(), "print.render_failed");
    assert!(!reply.text().contains("<html>"));
}

/// A port whose output is not a PDF.
fn build_with_raw(_port: Arc<MemoryPrintPort>) -> Print {
    use spot_http::api::print::{PdfBytes, PrintPort};
    use spot_http::api::service::{BoxFuture, RequestCtx};

    struct NotPdf;
    impl PrintPort for NotPdf {
        fn render<'a>(
            &'a self,
            _ctx: &'a RequestCtx,
            _doctype: &'a str,
            _id: RecordId,
            _format: Option<&'a str>,
        ) -> BoxFuture<'a, Result<PdfBytes, PrintPortError>> {
            Box::pin(async { Ok(b"<html>secret</html>".to_vec()) })
        }
    }
    let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    let services = ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store)
        .with_print(Arc::new(NotPdf));
    let site = SiteId::new("acme").must();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build();
    Print {
        router,
        port: Arc::new(MemoryPrintPort::new()),
    }
}

#[tokio::test]
async fn an_open_port_prints_any_document_for_an_authenticated_caller() {
    let app = build_with(Arc::new(MemoryPrintPort::open()), true);
    let reply = get(&app, &path("customer", OTHER_DOC), Some(TOKEN_BOB), &[]).await;
    assert_eq!(reply.status, 200);
    assert!(reply.bytes.starts_with(b"%PDF-"));
    let anonymous = get(&app, &path("customer", OTHER_DOC), None, &[]).await;
    assert_eq!(anonymous.status, 401);
}

#[tokio::test]
async fn hostile_format_values_and_extra_parameters_are_refused_before_the_port() {
    let app = printing();
    for query in [
        "format=../../etc/passwd",
        "format=Standard",
        "format=a%20b",
        "format=standard%0d%0aX-Evil:1",
        "format=standard%22",
        "format=standard&format=compact",
        "format=standard&paper=a3",
        "template=%7B%7B%20x%20%7D%7D",
        "flag",
    ] {
        let reply = get(
            &app,
            &format!("{}?{query}", path("sales_invoice", DOC)),
            Some(TOKEN_ALICE),
            &[],
        )
        .await;
        assert_eq!(reply.status, 400, "{query}");
        assert_eq!(reply.code(), "api.invalid_request", "{query}");
        assert!(reply.header("content-disposition").is_none(), "{query}");
    }
    assert_eq!(app.port.render_count(), 0);
}

#[tokio::test]
async fn path_names_that_could_inject_a_file_name_are_refused() {
    let app = printing();
    for (doctype, doc) in [
        ("Sales_Invoice", DOC),
        ("sales-invoice", DOC),
        ("sales_invoice", "not-a-uuid"),
        ("sales_invoice", "01890A5D-AC96-774B-BCCE-B302099A8057"),
        ("sales_invoice%22%0d%0a", DOC),
        ("..", DOC),
    ] {
        let reply = get(&app, &path(doctype, doc), Some(TOKEN_ALICE), &[]).await;
        assert!(
            reply.status == 400 || reply.status == 404,
            "{doctype} {doc}: {}",
            reply.status
        );
        assert!(reply.header("content-disposition").is_none());
        assert!(!reply.bytes.starts_with(b"%PDF"));
    }
    assert_eq!(app.port.render_count(), 0);
}

#[tokio::test]
async fn an_unrecognised_token_is_the_same_not_found_and_renders_nothing() {
    let app = printing();
    let known = get(&app, &path("sales_invoice", DOC), Some(TOKEN_BOB), &[]).await;
    let unknown = get(&app, &path("sales_invoice", DOC), Some("not-a-token"), &[]).await;
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.code(), "document.not_found");
    assert_eq!(unknown.error_shape(), known.error_shape());
    assert!(!unknown.bytes.starts_with(b"%PDF"));
    assert_eq!(app.port.render_count(), 0);
}
