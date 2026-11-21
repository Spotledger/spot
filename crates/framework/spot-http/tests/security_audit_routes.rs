//! The audit and support-access routes (WP-SEC-07.T3, contract 0.20.0) over fake `AuditPort`
//! and `SupportAccessPort`, driven through the real site layer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, header};
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::Code;
use spot_http::api::Ports;
use spot_http::api::ports::security_audit::{
    AuditCall, AuditPort, AuditReply, Op, SupportAccessPort,
};
use spot_http::api::router::routes::{RouteState, mount, security_audit};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";

struct Fake {
    step_up: Mutex<bool>,
    calls: Mutex<Vec<AuditCall>>,
    data: Mutex<Option<Json>>,
    export: Mutex<String>,
    deny_with: Mutex<Option<ApiError>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Fake {
            step_up: Mutex::new(true),
            calls: Mutex::new(Vec::new()),
            data: Mutex::new(None),
            export: Mutex::new("{\"id\":1}\n{\"id\":2}\n".to_owned()),
            deny_with: Mutex::new(None),
        })
    }
    fn ops(&self) -> Vec<Op> {
        self.calls.lock().unwrap().iter().map(|c| c.op).collect()
    }
    fn answer(&self, call: &AuditCall, caller: &str) -> Result<AuditReply, ApiError> {
        self.calls.lock().unwrap().push(call.clone());
        if let Some(error) = self.deny_with.lock().unwrap().clone() {
            return Err(error);
        }
        if call.op == Op::AuditExport {
            return Ok(AuditReply {
                data: None,
                next_cursor: None,
                export: Some(self.export.lock().unwrap().clone()),
            });
        }
        let data = self.data.lock().unwrap().clone();
        match &call.page {
            None => Ok(AuditReply {
                data: Some(data.unwrap_or_else(|| json!({"ok": true}))),
                next_cursor: None,
                export: None,
            }),
            Some(page) => {
                let bound = format!("{caller}|{}", page.query_digest);
                if page.cursor.as_ref().is_some_and(|c| c != &bound) {
                    return Err(ApiError::from_code(Code::SecurityInvalid));
                }
                let default = if call.op == Op::SupportSessionRecording {
                    json!([{"at": "2026-11-01T10:00:00Z", "action": "view", "target": null}])
                } else {
                    json!([])
                };
                Ok(AuditReply {
                    data: Some(data.unwrap_or(default)),
                    next_cursor: Some(page.cursor.is_none().then_some(bound)),
                    export: None,
                })
            }
        }
    }
}

fn caller(ctx: &RequestCtx) -> String {
    ctx.credential
        .as_ref()
        .map(|c| c.token.clone())
        .unwrap_or_default()
}

impl AuditPort for Fake {
    fn step_up_current<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<bool, ApiError>> {
        Box::pin(async move { Ok(*self.step_up.lock().unwrap()) })
    }
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a AuditCall,
    ) -> BoxFuture<'a, Result<AuditReply, ApiError>> {
        Box::pin(async move { self.answer(call, &caller(ctx)) })
    }
}

impl SupportAccessPort for Fake {
    fn step_up_current<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<bool, ApiError>> {
        Box::pin(async move { Ok(*self.step_up.lock().unwrap()) })
    }
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a AuditCall,
    ) -> BoxFuture<'a, Result<AuditReply, ApiError>> {
        Box::pin(async move { self.answer(call, &caller(ctx)) })
    }
}

struct Fixture {
    router: Router,
    fake: Arc<Fake>,
}

fn router_over(ports: Ports) -> Router {
    let acme = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(security_audit::security_audit_routes().with_state(RouteState::new(ports)))
    .build()
}

fn fixture() -> Fixture {
    let fake = Fake::new();
    let mut ports = Ports::new();
    ports.provide::<dyn AuditPort>(fake.clone());
    ports.provide::<dyn SupportAccessPort>(fake.clone());
    Fixture {
        router: router_over(ports),
        fake,
    }
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    raw: String,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_str(&self.raw).unwrap_or(Json::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
}

async fn send_to(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<&str>,
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
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned())),
        None => builder.body(Body::empty()),
    }
    .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        raw: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn send(
    f: &Fixture,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> Reply {
    send_to(&f.router, method, path, Some("tok-alice"), headers, body).await
}

const KEY: (&str, &str) = ("idempotency-key", "k-1");
const WINDOW: &str = "from=2026-11-01T00:00:00Z&to=2026-11-10T00:00:00Z";

#[allow(clippy::type_complexity)]
fn valid_requests() -> Vec<(
    Op,
    &'static str,
    String,
    Vec<(&'static str, &'static str)>,
    Option<String>,
    u16,
)> {
    vec![
        (
            Op::AuditChainVerify,
            "POST",
            "/api/v1/security/audit/chain/verify".into(),
            vec![],
            Some("{}".into()),
            200,
        ),
        (
            Op::AuditEntries,
            "GET",
            format!("/api/v1/security/audit/entries?{WINDOW}&limit=5"),
            vec![],
            None,
            200,
        ),
        (
            Op::AuditExport,
            "GET",
            format!("/api/v1/security/audit/export?{WINDOW}"),
            vec![],
            None,
            200,
        ),
        (
            Op::ComplianceEvidencePack,
            "POST",
            "/api/v1/security/compliance/evidence-pack".into(),
            vec![KEY],
            Some(json!({"framework": "soc2"}).to_string()),
            202,
        ),
        (
            Op::ComplianceControls,
            "GET",
            "/api/v1/security/compliance/controls".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::SupportRequestList,
            "GET",
            "/api/v1/security/support-access/requests".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::SupportRequestConsent,
            "POST",
            "/api/v1/security/support-access/requests/r1/consent".into(),
            vec![],
            Some(json!({"consent": "I consent", "signature": "c2lnbmF0dXJl"}).to_string()),
            200,
        ),
        (
            Op::SupportRequestRevoke,
            "POST",
            "/api/v1/security/support-access/requests/r1/revoke".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::SupportSessionList,
            "GET",
            "/api/v1/security/support-access/sessions".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::SupportSessionRecording,
            "GET",
            "/api/v1/security/support-access/sessions/s1/recording".into(),
            vec![],
            None,
            200,
        ),
    ]
}

#[tokio::test]
async fn all_10_operations_are_mounted_and_reach_their_port() {
    let f = fixture();
    for (op, method, path, headers, body, status) in valid_requests() {
        let r = send(&f, method, &path, &headers, body.as_deref()).await;
        assert_eq!(r.status, status, "{method} {path}: {}", r.raw);
        assert_eq!(
            r.headers.get(header::CACHE_CONTROL).unwrap(),
            "private, no-store",
            "{}",
            op.id()
        );
        assert_eq!(f.fake.ops().last(), Some(&op), "{path}");
    }
    let mut seen = f.fake.ops();
    seen.sort();
    let mut all = Op::ALL.to_vec();
    all.sort();
    assert_eq!(seen, all);
    let ids: BTreeSet<&str> = Op::ALL.iter().map(|o| o.id()).collect();
    let served: BTreeSet<&str> = security_audit::SECURITY_AUDIT_SERVES
        .iter()
        .copied()
        .collect();
    assert_eq!(ids, served);
}

#[tokio::test]
async fn the_export_has_fixed_headers_a_bounded_window_and_a_size_cap() {
    let f = fixture();
    let r = send(
        &f,
        "GET",
        &format!("/api/v1/security/audit/export?{WINDOW}"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/x-ndjson"
    );
    assert_eq!(
        r.headers.get(header::CONTENT_DISPOSITION).unwrap(),
        "attachment; filename=\"audit-export.ndjson\""
    );
    assert_eq!(
        r.headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert_eq!(r.raw, "{\"id\":1}\n{\"id\":2}\n");
    f.fake.calls.lock().unwrap().clear();
    // More than 31 days, backwards, missing and malformed windows never reach the port.
    for q in [
        "from=2026-10-01T00:00:00Z&to=2026-11-10T00:00:00Z",
        "from=2026-11-10T00:00:00Z&to=2026-11-01T00:00:00Z",
        "from=2026-11-10T00:00:00Z",
        "from=yesterday&to=today",
    ] {
        let r = send(
            &f,
            "GET",
            &format!("/api/v1/security/audit/export?{q}"),
            &[],
            None,
        )
        .await;
        assert!(r.status == 422 || r.status == 400, "{q}: {}", r.status);
    }
    assert!(f.fake.ops().is_empty());
    // Exactly 31 days passes.
    let r = send(
        &f,
        "GET",
        "/api/v1/security/audit/export?from=2026-10-01T00:00:00Z&to=2026-11-01T00:00:00Z",
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    // A port export over 1 MiB is refused.
    *f.fake.export.lock().unwrap() = "x".repeat(1_048_577);
    let r = send(
        &f,
        "GET",
        &format!("/api/v1/security/audit/export?{WINDOW}"),
        &[],
        None,
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (500, "http.internal"));
}

#[tokio::test]
async fn export_and_recordings_need_a_recent_step_up() {
    let f = fixture();
    *f.fake.step_up.lock().unwrap() = false;
    for (op, method, path, headers, body, _) in valid_requests() {
        let r = send(&f, method, &path, &headers, body.as_deref()).await;
        let needs = matches!(
            op,
            Op::AuditExport
                | Op::SupportRequestConsent
                | Op::SupportRequestRevoke
                | Op::SupportSessionRecording
        );
        if needs {
            assert_eq!(
                (r.status, r.code().as_str()),
                (403, "auth.step_up_required"),
                "{}",
                op.id()
            );
        } else {
            assert_eq!(r.status / 100, 2, "{}: {}", op.id(), r.raw);
        }
    }
    let called = f.fake.ops();
    assert!(!called.contains(&Op::AuditExport));
    assert!(!called.contains(&Op::SupportSessionRecording));
    assert!(!called.contains(&Op::SupportRequestConsent));
}

#[tokio::test]
async fn a_recording_is_json_no_store_and_its_rows_are_checked() {
    let f = fixture();
    let path = "/api/v1/security/support-access/sessions/s1/recording";
    let r = send(&f, "GET", path, &[], None).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    assert_eq!(
        r.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/json"
    );
    for bad in [
        json!([{"at": "2026-11-01T10:00:00Z", "action": "view", "target": null, "html": "<script>"}]),
        json!([{"at": "not a time", "action": "view", "target": null}]),
        json!([{"at": "2026-11-01T10:00:00Z", "action": "a".repeat(201), "target": null}]),
        json!({"not": "a list"}),
    ] {
        *f.fake.data.lock().unwrap() = Some(bad);
        let r = send(&f, "GET", path, &[], None).await;
        assert_eq!((r.status, r.code().as_str()), (500, "http.internal"));
        assert!(!r.raw.contains("<script>"));
    }
}

#[tokio::test]
async fn consent_is_bounded_base64_with_no_user_field_and_no_signature_back() {
    let f = fixture();
    let path = "/api/v1/security/support-access/requests/r1/consent";
    let ok = json!({"consent": "I consent", "signature": "c2lnbmF0dXJl"}).to_string();
    let r = send(&f, "POST", path, &[], Some(&ok)).await;
    assert_eq!(r.status, 200);
    let body = f
        .fake
        .calls
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .body
        .clone()
        .unwrap();
    assert_eq!(body.len(), 2, "only consent and signature reach the port");
    assert_eq!(f.fake.calls.lock().unwrap().last().unwrap().path, ["r1"]);
    f.fake.calls.lock().unwrap().clear();
    for bad in [
        json!({"consent": "c", "signature": "not base64!!"}),
        json!({"consent": "c", "signature": ""}),
        json!({"consent": "c", "signature": "A".repeat(1025)}),
        json!({"consent": "", "signature": "c2ln"}),
        json!({"consent": "c".repeat(8193), "signature": "c2ln"}),
        json!({"consent": "c", "signature": "c2ln", "approved_by": "someone"}),
        json!({"consent": "c"}),
    ] {
        let r = send(&f, "POST", path, &[], Some(&bad.to_string())).await;
        assert_eq!(r.status, 400, "{bad}");
        assert!(!r.raw.contains("approved_by"));
    }
    assert!(f.fake.ops().is_empty());
    // An answer that echoes the signature is refused.
    *f.fake.data.lock().unwrap() = Some(json!({"status": "consented", "signature": "c2ln"}));
    let r = send(&f, "POST", path, &[], Some(&ok)).await;
    assert_eq!((r.status, r.code().as_str()), (500, "http.internal"));
    assert!(!r.raw.contains("c2ln"));
}

#[tokio::test]
async fn entries_are_paged_with_a_cursor_bound_to_caller_and_query() {
    let f = fixture();
    let path = format!("/api/v1/security/audit/entries?{WINDOW}");
    let first = send(&f, "GET", &path, &[], None).await;
    let cursor = first.json()["next_cursor"]
        .as_str()
        .unwrap()
        .replace('|', "%7C");
    let ok = send(&f, "GET", &format!("{path}&cursor={cursor}"), &[], None).await;
    assert_eq!(ok.status, 200);
    let other = send_to(
        &f.router,
        "GET",
        &format!("{path}&cursor={cursor}"),
        Some("tok-bob"),
        &[],
        None,
    )
    .await;
    assert_eq!(
        (other.status, other.code().as_str()),
        (422, "security.invalid")
    );
    let other_query = send(
        &f,
        "GET",
        &format!("/api/v1/security/audit/entries?from=2026-11-02T00:00:00Z&cursor={cursor}"),
        &[],
        None,
    )
    .await;
    assert_eq!(other_query.status, 422);
    for q in [
        "limit=0",
        "limit=101",
        "limit=1&limit=2",
        "cursor=",
        "actor=nope",
        "other=1",
        "from=2026-11-10T00:00:00Z&to=2026-11-01T00:00:00Z",
    ] {
        let r = send(
            &f,
            "GET",
            &format!("/api/v1/security/audit/entries?{q}"),
            &[],
            None,
        )
        .await;
        assert!(r.status == 400 || r.status == 422, "{q}: {}", r.status);
    }
}

#[tokio::test]
async fn compliance_inputs_and_the_verify_body_are_checked() {
    let f = fixture();
    let base = "/api/v1/security/compliance/evidence-pack";
    let r = send(&f, "POST", base, &[], Some(r#"{"framework":"soc2"}"#)).await;
    assert_eq!(r.status, 400, "Idempotency-Key required");
    for body in [
        r#"{}"#,
        r#"{"framework":""}"#,
        r#"{"framework":"soc2","extra":1}"#,
        r#"{"framework":"soc2","from":"2026-11-10T00:00:00Z","to":"2026-11-01T00:00:00Z"}"#,
    ] {
        let r = send(&f, "POST", base, &[KEY], Some(body)).await;
        assert!(r.status == 400 || r.status == 422, "{body}");
    }
    let r = send(
        &f,
        "POST",
        "/api/v1/security/audit/chain/verify",
        &[],
        Some(r#"{"to":"x"}"#),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = send(
        &f,
        "POST",
        "/api/v1/security/audit/chain/verify",
        &[],
        Some("[]"),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(f.fake.ops().is_empty());
}

#[tokio::test]
async fn an_unset_port_answers_501_for_its_own_operations_only() {
    let fake = Fake::new();
    let mut ports = Ports::new();
    ports.provide::<dyn AuditPort>(fake.clone());
    let router = router_over(ports);
    let r = send_to(
        &router,
        "GET",
        "/api/v1/security/support-access/sessions",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (501, "service.not_configured")
    );
    let r = send_to(
        &router,
        "GET",
        "/api/v1/security/compliance/controls",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn the_group_is_not_mounted_without_both_ports() {
    let mut ports = Ports::new();
    ports.provide::<dyn AuditPort>(Fake::new());
    let mounted = mount(&[security_audit::registration()], &ports, &[]);
    assert!(mounted.report.mounted.is_empty());
    ports.provide::<dyn SupportAccessPort>(Fake::new());
    let mounted = mount(&[security_audit::registration()], &ports, &[]);
    assert_eq!(mounted.report.mounted, ["security_audit"]);
    assert_eq!(mounted.report.served.len(), 10);
}

#[tokio::test]
async fn port_errors_are_filtered_and_the_bearer_is_required() {
    let f = fixture();
    *f.fake.deny_with.lock().unwrap() = Some(ApiError::from_code(Code::SecurityNotFound));
    let r = send(
        &f,
        "GET",
        "/api/v1/security/support-access/requests",
        &[],
        None,
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (404, "security.not_found"));
    *f.fake.deny_with.lock().unwrap() =
        Some(ApiError::new(Code::PrintRenderFailed, "secret detail"));
    let r = send(
        &f,
        "GET",
        "/api/v1/security/support-access/requests",
        &[],
        None,
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (500, "http.internal"));
    assert!(!r.raw.contains("secret detail"));
    f.fake.calls.lock().unwrap().clear();
    let r = send_to(
        &f.router,
        "GET",
        "/api/v1/security/compliance/controls",
        None,
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 401);
    let long = "a".repeat(201);
    let r = send(
        &f,
        "GET",
        &format!("/api/v1/security/support-access/sessions/{long}/recording"),
        &[],
        None,
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (404, "security.not_found"));
    assert!(f.fake.ops().is_empty());
}
