//! The `/api/v1/report*` catalogue routes (`report_list` and `report_describe`, WP-PLT-14.T2)
//! over a fake [`ReportCatalogPort`]: every status and error code the contract names for the two
//! operations, the shape of every request the routes accept or refuse, and the JSON the contract
//! specifies for the list and the description.
//!
//! The fake decides nothing the real adapter decides; it answers what a test scripts and keeps
//! what the routes passed it, so the tests prove the routes' half of the bargain: what is checked
//! before the port, what is passed to it and how its answer becomes the response.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::meta::FieldType;
use spot_http::api::ports::report::{
    CatalogResult, ParameterOptionsOut, ReportCatalogPort, ReportDescriptionOut,
    ReportParameterOut, ReportSummaryOut,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{RouteState, report};
use spot_http::api::service::{ApiError, BoxFuture, ReportColumn, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const TOKEN: &str = "tok-alice";

#[derive(Default)]
struct Script {
    /// The next call fails with this.
    fail: Option<ApiError>,
    /// What `list` answers.
    list: Option<Vec<ReportSummaryOut>>,
    /// What `describe` answers.
    described: Option<ReportDescriptionOut>,
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

    fn record(&self, ctx: &RequestCtx, op: &'static str, detail: String) -> CatalogResult<()> {
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

fn summary(name: &str, label: &str) -> ReportSummaryOut {
    ReportSummaryOut {
        name: name.to_owned(),
        label: label.to_owned(),
        app: "sample".to_owned(),
        base_doctype: "time_entry".to_owned(),
    }
}

fn description() -> ReportDescriptionOut {
    ReportDescriptionOut {
        name: "hours_by_task".to_owned(),
        label: "Hours by task".to_owned(),
        app: "sample".to_owned(),
        base_doctype: "time_entry".to_owned(),
        parameters: vec![
            ReportParameterOut {
                name: "from_date".to_owned(),
                label: "From date".to_owned(),
                kind: "date".to_owned(),
                required: true,
                default: None,
                options: None,
            },
            ReportParameterOut {
                name: "status".to_owned(),
                label: "Status".to_owned(),
                kind: "select".to_owned(),
                required: false,
                default: Some(json!("posted")),
                options: Some(ParameterOptionsOut::Values(vec![
                    "draft".to_owned(),
                    "posted".to_owned(),
                ])),
            },
            ReportParameterOut {
                name: "task".to_owned(),
                label: "Task".to_owned(),
                kind: "link".to_owned(),
                required: false,
                default: None,
                options: Some(ParameterOptionsOut::Link("task".to_owned())),
            },
        ],
        columns: vec![
            ReportColumn {
                name: "task".to_owned(),
                label: "Task".to_owned(),
                field_type: FieldType::Link,
            },
            ReportColumn {
                name: "hours".to_owned(),
                label: "Hours".to_owned(),
                field_type: FieldType::Decimal { scale: 2 },
            },
        ],
        max_rows: 10_000,
    }
}

impl ReportCatalogPort for Fake {
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, CatalogResult<Vec<ReportSummaryOut>>> {
        Box::pin(async move {
            self.record(ctx, "list", String::new())?;
            Ok(self.lock().list.clone().unwrap_or_default())
        })
    }

    fn describe<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
    ) -> BoxFuture<'a, CatalogResult<ReportDescriptionOut>> {
        Box::pin(async move {
            self.record(ctx, "describe", report.to_owned())?;
            Ok(self.lock().described.clone().unwrap_or_else(description))
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
    ports.provide::<dyn ReportCatalogPort>(fake.clone());
    let state = RouteState::new(ports.subset(report::required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(report::routes().with_state(state))
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
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn get(f: &Fixture, path: &str, token: Option<&str>) -> Reply {
    let mut builder = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).unwrap();
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

async fn call(f: &Fixture, path: &str) -> Reply {
    get(f, path, Some(TOKEN)).await
}

fn fail_next(f: &Fixture, error: ApiError) {
    f.fake.lock().fail = Some(error);
}

fn calls(f: &Fixture) -> usize {
    f.fake.lock().calls.len()
}

// ------------------------------------------------------------------------------------------
// report_list

#[tokio::test]
async fn list_answers_the_summaries_in_the_port_order() {
    let f = fixture();
    f.fake.lock().list = Some(vec![
        summary("hours_by_task", "Hours by task"),
        summary("open_tasks", "Open tasks"),
    ]);
    let r = call(&f, "/api/v1/report").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert!(r.header("x-request-id").is_some());
    assert_eq!(
        r.json(),
        json!({"data": [
            {"name": "hours_by_task", "label": "Hours by task", "app": "sample", "base_doctype": "time_entry"},
            {"name": "open_tasks", "label": "Open tasks", "app": "sample", "base_doctype": "time_entry"},
        ]})
    );
    assert_eq!(f.fake.lock().tokens, vec![TOKEN.to_owned()]);
}

#[tokio::test]
async fn an_empty_list_is_an_empty_array_not_an_error() {
    let f = fixture();
    let r = call(&f, "/api/v1/report").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), json!({"data": []}));
}

#[tokio::test]
async fn list_refuses_a_request_without_a_credential_before_the_port() {
    let f = fixture();
    let r = get(&f, "/api/v1/report", None).await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(r.code(), "auth.required");
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert_eq!(calls(&f), 0, "the port is not asked");
}

#[tokio::test]
async fn list_relays_the_ports_refusals_with_their_codes() {
    for (error, status, code) in [
        (
            ApiError::from_code(Code::AuthRequired),
            401,
            "auth.required",
        ),
        (
            ApiError::from_code(Code::PermissionDenied),
            403,
            "permission.denied",
        ),
        (
            ApiError::from_code(Code::ServerUnavailable),
            503,
            "server.unavailable",
        ),
    ] {
        let f = fixture();
        fail_next(&f, error);
        let r = call(&f, "/api/v1/report").await;
        assert_eq!(r.status, status, "{}", r.text());
        assert_eq!(r.code(), code);
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
    }
}

// ------------------------------------------------------------------------------------------
// report_describe

#[tokio::test]
async fn describe_answers_the_contract_shape() {
    let f = fixture();
    let r = call(&f, "/api/v1/report/hours_by_task").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert_eq!(
        r.json(),
        json!({"data": {
            "name": "hours_by_task",
            "label": "Hours by task",
            "app": "sample",
            "base_doctype": "time_entry",
            "parameters": [
                {"name": "from_date", "label": "From date", "type": "date", "required": true},
                {"name": "status", "label": "Status", "type": "select", "required": false,
                 "default": "posted", "options": {"source": "values", "values": ["draft", "posted"]}},
                {"name": "task", "label": "Task", "type": "link", "required": false,
                 "options": {"source": "link", "doctype": "task"}},
            ],
            "columns": [
                {"name": "task", "label": "Task", "type": "link"},
                {"name": "hours", "label": "Hours", "type": "decimal"},
            ],
            "max_rows": 10000,
        }})
    );
    let (op, detail) = f.fake.lock().calls.last().cloned().unwrap();
    assert_eq!(op, "describe");
    assert_eq!(detail, "hours_by_task");
}

#[tokio::test]
async fn a_required_parameter_has_no_default_and_a_plain_one_has_no_options() {
    let f = fixture();
    let body = call(&f, "/api/v1/report/hours_by_task").await.json();
    let params = body["data"]["parameters"].as_array().unwrap();
    assert!(params[0].get("default").is_none());
    assert!(params[0].get("options").is_none());
}

#[tokio::test]
async fn describe_relays_not_found_for_an_unknown_and_for_a_not_admitted_report_alike() {
    let f = fixture();
    // The port answers one `report.not_found` for both; the route adds nothing to tell them apart.
    fail_next(&f, ApiError::from_code(Code::ReportNotFound));
    let unknown = call(&f, "/api/v1/report/no_such_report").await;
    fail_next(&f, ApiError::from_code(Code::ReportNotFound));
    let hidden = call(&f, "/api/v1/report/payroll_summary").await;
    assert_eq!(unknown.status, 404, "{}", unknown.text());
    assert_eq!(hidden.status, 404);
    assert_eq!(unknown.code(), "report.not_found");
    assert_eq!(hidden.code(), "report.not_found");
    let strip = |r: &Reply| {
        let mut body = r.json();
        if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
            error.remove("request_id");
        }
        body
    };
    assert_eq!(strip(&unknown), strip(&hidden));
}

#[tokio::test]
async fn describe_refuses_a_name_that_is_not_a_storage_name_before_the_port() {
    let f = fixture();
    for path in [
        "/api/v1/report/Hours%20By%20Task",
        "/api/v1/report/1report",
        "/api/v1/report/a-b",
    ] {
        let r = call(&f, path).await;
        assert_eq!(r.status, 400, "{path}: {}", r.text());
        assert_eq!(r.code(), "api.invalid_request", "{path}");
    }
    assert_eq!(calls(&f), 0, "the port is not asked");
}

#[tokio::test]
async fn describe_refuses_a_request_without_a_credential_before_the_port() {
    let f = fixture();
    let r = get(&f, "/api/v1/report/hours_by_task", None).await;
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(r.code(), "auth.required");
    assert_eq!(calls(&f), 0);
}

#[tokio::test]
async fn describe_relays_the_ports_other_refusals() {
    for (error, status, code) in [
        (
            ApiError::from_code(Code::PermissionDenied),
            403,
            "permission.denied",
        ),
        (
            ApiError::from_code(Code::PermissionFieldDenied),
            403,
            "permission.field_denied",
        ),
        (
            ApiError::from_code(Code::ServerUnavailable),
            503,
            "server.unavailable",
        ),
    ] {
        let f = fixture();
        fail_next(&f, error);
        let r = call(&f, "/api/v1/report/hours_by_task").await;
        assert_eq!(r.status, status, "{}", r.text());
        assert_eq!(r.code(), code);
    }
}

#[tokio::test]
async fn only_get_is_served() {
    let f = fixture();
    for path in ["/api/v1/report", "/api/v1/report/hours_by_task"] {
        let request = Request::builder()
            .method("DELETE")
            .uri(path)
            .header(header::HOST, HOST)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let response = f.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 405, "{path}");
    }
    assert_eq!(calls(&f), 0);
}

#[test]
fn the_group_declares_its_operations_and_needs_the_catalogue_port() {
    assert_eq!(report::SERVES, &["report_list", "report_describe"]);
    assert_eq!(report::required_ports().len(), 1);
    let registration = report::registration();
    assert_eq!(registration.name, "report");
    assert!(registration.public.is_none(), "the group is authenticated");
}
