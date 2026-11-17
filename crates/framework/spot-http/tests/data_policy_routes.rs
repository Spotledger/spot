//! The data-policy and protected-group routes (WP-SEC-05.T6, contract 0.20.0) over fake
//! `DataPolicyPort` and `ProtectedGroupPort`, driven through the real site layer.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, header};
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::Code;
use spot_http::api::Ports;
use spot_http::api::ports::data_policy::{
    DataPolicyPort, Op, PolicyCall, PolicyReply, ProtectedGroupPort,
};
use spot_http::api::router::routes::{RouteState, data_policy, mount};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Fake {
    step_up: Mutex<bool>,
    calls: Mutex<Vec<PolicyCall>>,
    data: Mutex<Option<Json>>,
    deny_with: Mutex<Option<ApiError>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Fake {
            step_up: Mutex::new(true),
            calls: Mutex::new(Vec::new()),
            data: Mutex::new(None),
            deny_with: Mutex::new(None),
        })
    }
    fn ops(&self) -> Vec<Op> {
        self.calls.lock().unwrap().iter().map(|c| c.op).collect()
    }
    fn answer(&self, call: &PolicyCall, caller: &str) -> Result<PolicyReply, ApiError> {
        self.calls.lock().unwrap().push(call.clone());
        if let Some(error) = self.deny_with.lock().unwrap().clone() {
            return Err(error);
        }
        if call.op == Op::RetentionHoldDelete {
            return Ok(PolicyReply {
                data: None,
                next_cursor: None,
                etag: None,
            });
        }
        let data = self
            .data
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| json!({"ok": true, "key_version": 3, "fingerprint": "ab:cd"}));
        match &call.page {
            None => Ok(PolicyReply {
                data: Some(data),
                next_cursor: None,
                etag: (call.op == Op::DataPolicyGet).then(|| "\"4\"".to_owned()),
            }),
            Some(page) => {
                let bound = format!("{caller}|{}", page.query_digest);
                if page.cursor.as_ref().is_some_and(|c| c != &bound) {
                    return Err(ApiError::from_code(Code::SecurityInvalid));
                }
                Ok(PolicyReply {
                    data: Some(json!([])),
                    next_cursor: Some(page.cursor.is_none().then_some(bound)),
                    etag: None,
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

impl DataPolicyPort for Fake {
    fn step_up_current<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<bool, ApiError>> {
        Box::pin(async move { Ok(*self.step_up.lock().unwrap()) })
    }
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a PolicyCall,
    ) -> BoxFuture<'a, Result<PolicyReply, ApiError>> {
        Box::pin(async move { self.answer(call, &caller(ctx)) })
    }
}

impl ProtectedGroupPort for Fake {
    fn step_up_current<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<bool, ApiError>> {
        Box::pin(async move { Ok(*self.step_up.lock().unwrap()) })
    }
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a PolicyCall,
    ) -> BoxFuture<'a, Result<PolicyReply, ApiError>> {
        Box::pin(async move { self.answer(call, &caller(ctx)) })
    }
}

struct Fixture {
    router: Router,
    fake: Arc<Fake>,
}

fn router_over(ports: &Ports) -> Router {
    let state = RouteState::new(
        ports
            .subset(data_policy::data_policy_required_ports())
            .unwrap_or_else(|_| Ports::new()),
    );
    let acme = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(data_policy::data_policy_routes().with_state(state))
    .build()
}

fn fixture() -> Fixture {
    let fake = Fake::new();
    let mut ports = Ports::new();
    ports.provide::<dyn DataPolicyPort>(fake.clone());
    ports.provide::<dyn ProtectedGroupPort>(fake.clone());
    Fixture {
        router: router_over(&ports),
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

#[allow(clippy::type_complexity)]
fn valid_requests() -> Vec<(
    Op,
    &'static str,
    String,
    Vec<(&'static str, &'static str)>,
    Option<String>,
    u16,
)> {
    let policy = json!({"region": "eu", "residency": ["eu"], "version": 4,
        "classes": [{"class": "audit", "retention_days": 365, "legal_hold_allowed": true},
                    {"class": "logs", "retention_days": null, "legal_hold_allowed": false}]});
    vec![
        (
            Op::DataPolicyGet,
            "GET",
            "/api/v1/security/data-policy".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::DataPolicyPut,
            "PUT",
            "/api/v1/security/data-policy".into(),
            vec![("if-match", "\"4\"")],
            Some(policy.to_string()),
            200,
        ),
        (
            Op::RetentionRunList,
            "GET",
            "/api/v1/security/retention/runs?limit=10".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::RetentionRunCreate,
            "POST",
            "/api/v1/security/retention/runs".into(),
            vec![KEY],
            Some(json!({"dry_run": true}).to_string()),
            202,
        ),
        (
            Op::RetentionHoldList,
            "GET",
            "/api/v1/security/retention/holds".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::RetentionHoldCreate,
            "POST",
            "/api/v1/security/retention/holds".into(),
            vec![KEY],
            Some(json!({"scope": "customer:42", "reason": "litigation"}).to_string()),
            201,
        ),
        (
            Op::RetentionHoldDelete,
            "DELETE",
            "/api/v1/security/retention/holds/h1".into(),
            vec![],
            None,
            204,
        ),
        (
            Op::RetentionProofList,
            "GET",
            "/api/v1/security/retention/proofs?run=0190f6c0-0000-7000-8000-000000000001".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::ResidencyCheck,
            "POST",
            "/api/v1/security/residency/check".into(),
            vec![],
            Some(json!({"region": "eu", "doctypes": ["Customer"]}).to_string()),
            200,
        ),
        (
            Op::ProtectedGroupList,
            "GET",
            "/api/v1/security/protected-groups".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::ProtectedGroupSyncClass,
            "PUT",
            "/api/v1/security/protected-groups/pii/sync-class".into(),
            vec![],
            Some(json!({"sync_class": "named_nodes", "nodes": ["n1"]}).to_string()),
            200,
        ),
        (
            Op::NodeKeyList,
            "GET",
            "/api/v1/security/node-keys".into(),
            vec![],
            None,
            200,
        ),
        (
            Op::NodeKeyRevoke,
            "POST",
            "/api/v1/security/node-keys/n1/revoke".into(),
            vec![],
            None,
            200,
        ),
    ]
}

#[tokio::test]
async fn all_13_operations_are_mounted_and_reach_their_port() {
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
    let ids: std::collections::BTreeSet<&str> = Op::ALL.iter().map(|o| o.id()).collect();
    let served: std::collections::BTreeSet<&str> = data_policy::SECURITY_DATA_POLICY_SERVES
        .iter()
        .copied()
        .collect();
    assert_eq!(ids, served);
}

async fn post_run(f: &Fixture, body: &str) -> Reply {
    send(
        f,
        "POST",
        "/api/v1/security/retention/runs",
        &[KEY],
        Some(body),
    )
    .await
}

#[tokio::test]
async fn a_real_retention_run_without_its_digest_is_refused_before_the_port() {
    let f = fixture();
    let r = post_run(&f, r#"{"dry_run": false}"#).await;
    assert_eq!(r.status, 422, "{}", r.raw);
    assert_eq!(r.code(), "security.plan_required");
    assert!(f.fake.ops().is_empty(), "the port was called");
    // A dry run may not carry a digest; a malformed digest is refused.
    let r = post_run(
        &f,
        &format!("{{\"dry_run\": true, \"plan_digest\": \"{DIGEST}\"}}"),
    )
    .await;
    assert_eq!(r.status, 400);
    for bad in [
        "abc",
        "ZZ23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ] {
        let r = post_run(
            &f,
            &format!("{{\"dry_run\": false, \"plan_digest\": \"{bad}\"}}"),
        )
        .await;
        assert_eq!(r.status, 400, "{bad}");
    }
    assert!(f.fake.ops().is_empty());
    // A real run with the digest reaches the port with exactly that body.
    let r = post_run(
        &f,
        &format!("{{\"dry_run\": false, \"plan_digest\": \"{DIGEST}\"}}"),
    )
    .await;
    assert_eq!(r.status, 202, "{}", r.raw);
    let call = f.fake.calls.lock().unwrap().last().unwrap().clone();
    assert_eq!(call.body.unwrap()["plan_digest"], DIGEST);
    // `dry_run` defaults to true.
    let r = post_run(&f, "{}").await;
    assert_eq!(r.status, 202);
    let call = f.fake.calls.lock().unwrap().last().unwrap().clone();
    assert_eq!(call.body.unwrap()["dry_run"], true);
}

#[tokio::test]
async fn an_answer_that_carries_key_material_is_refused() {
    let f = fixture();
    for bad in [
        json!({"group": "pii", "wrapped_key": "AAAA"}),
        json!({"nodes": [{"name": "n1", "signature_seed": "x"}]}),
        json!({"secret": "x"}),
        json!({"key": "x"}),
        json!({"data_key": "x"}),
    ] {
        *f.fake.data.lock().unwrap() = Some(bad);
        let r = send(
            &f,
            "POST",
            "/api/v1/security/node-keys/n1/revoke",
            &[],
            None,
        )
        .await;
        assert_eq!((r.status, r.code().as_str()), (500, "http.internal"));
        assert!(!r.raw.contains("AAAA"));
    }
    // The contract's `key_version` and `fingerprint` pass.
    *f.fake.data.lock().unwrap() =
        Some(json!([{"name": "n1", "fingerprint": "ab:cd", "key_version": 2}]));
    let r = send(
        &f,
        "POST",
        "/api/v1/security/node-keys/n1/revoke",
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
}

#[test]
fn no_security_response_schema_of_the_contract_has_a_key_material_field() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    );
    let text = std::fs::read_to_string(path).unwrap();
    let root: Json = serde_norway::from_str(&text).unwrap();
    let schemas = root["components"]["schemas"].as_object().unwrap();
    fn names(value: &Json, out: &mut Vec<String>) {
        match value {
            Json::Object(map) => {
                if let Some(Json::Object(props)) = map.get("properties") {
                    out.extend(props.keys().cloned());
                }
                map.values().for_each(|v| names(v, out));
            }
            Json::Array(items) => items.iter().for_each(|v| names(v, out)),
            _ => {}
        }
    }
    let mut checked = 0;
    for (name, schema) in schemas {
        if !name.starts_with("Security") || !name.ends_with("Response") {
            continue;
        }
        checked += 1;
        let mut fields = Vec::new();
        names(schema, &mut fields);
        for field in fields {
            let lower = field.to_ascii_lowercase();
            let bad = [
                "secret",
                "seed",
                "wrapped",
                "private",
                "material",
                "signature",
            ]
            .iter()
            .any(|b| lower.contains(b))
                || lower == "key"
                || lower.ends_with("_key");
            assert!(!bad, "{name} has the field `{field}`");
        }
    }
    assert!(checked >= 10, "{checked} response schemas read");
}

#[tokio::test]
async fn revoke_and_sync_class_are_never_replayed_or_cached() {
    let f = fixture();
    for _ in 0..2 {
        let r = send(
            &f,
            "POST",
            "/api/v1/security/node-keys/n1/revoke",
            &[KEY],
            None,
        )
        .await;
        assert_eq!(r.status, 200);
        assert_eq!(
            r.headers.get(header::CACHE_CONTROL).unwrap(),
            "private, no-store"
        );
        assert!(r.headers.get("idempotency-replayed").is_none());
        let body = json!({"sync_class": "everywhere"}).to_string();
        let r = send(
            &f,
            "PUT",
            "/api/v1/security/protected-groups/pii/sync-class",
            &[KEY],
            Some(&body),
        )
        .await;
        assert_eq!(r.status, 200);
    }
    let calls = f.fake.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 4, "every repeat reaches the port");
    assert!(
        calls.iter().all(|c| c.idempotency_key.is_none()),
        "the key is never passed on"
    );
}

#[tokio::test]
async fn writes_without_a_recent_step_up_never_reach_a_port() {
    let f = fixture();
    *f.fake.step_up.lock().unwrap() = false;
    for (op, method, path, headers, body, _) in valid_requests() {
        let r = send(&f, method, &path, &headers, body.as_deref()).await;
        let write = !matches!(op, Op::ResidencyCheck) && method != "GET";
        if write {
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
    for op in [
        Op::DataPolicyPut,
        Op::RetentionRunCreate,
        Op::RetentionHoldCreate,
        Op::RetentionHoldDelete,
        Op::ProtectedGroupSyncClass,
        Op::NodeKeyRevoke,
    ] {
        assert!(!called.contains(&op), "{}", op.id());
    }
}

#[tokio::test]
async fn an_unset_port_answers_501_for_its_own_operations_only() {
    let fake = Fake::new();
    let mut ports = Ports::new();
    ports.provide::<dyn DataPolicyPort>(fake.clone());
    // Mounted by hand over one port: the other port's operations answer 501.
    let state = RouteState::new(ports);
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(data_policy::data_policy_routes().with_state(state))
    .build();
    let r = send_to(
        &router,
        "GET",
        "/api/v1/security/node-keys",
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
        "/api/v1/security/data-policy",
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
    ports.provide::<dyn DataPolicyPort>(Fake::new());
    let mounted = mount(&[data_policy::registration()], &ports, &[]);
    assert!(mounted.report.mounted.is_empty());
    ports.provide::<dyn ProtectedGroupPort>(Fake::new());
    let mounted = mount(&[data_policy::registration()], &ports, &[]);
    assert_eq!(mounted.report.mounted, ["data_policy"]);
    assert_eq!(mounted.report.served.len(), 13);
}

#[tokio::test]
async fn inputs_are_bounded_and_unknown_fields_refused_without_echo() {
    let f = fixture();
    let h = [KEY];
    let long = "s".repeat(501);
    for body in [
        json!({"scope": long, "reason": "r"}),
        json!({"scope": "s", "reason": "r", "zzz-secret": 1}),
        json!({"scope": "s"}),
        json!({"scope": "s", "reason": "r", "expires_at": "tomorrow"}),
    ] {
        let r = send(
            &f,
            "POST",
            "/api/v1/security/retention/holds",
            &h,
            Some(&body.to_string()),
        )
        .await;
        assert_eq!(r.status, 400, "{body}");
        assert!(!r.raw.contains("zzz-secret"));
    }
    let r = send(
        &f,
        "PUT",
        "/api/v1/security/protected-groups/pii/sync-class",
        &[],
        Some(r#"{"sync_class":"nowhere"}"#),
    )
    .await;
    assert_eq!(r.status, 400);
    let r = send(
        &f,
        "PUT",
        "/api/v1/security/protected-groups/pii/sync-class",
        &[],
        Some(r#"{"sync_class":"everywhere","wrapped_key":"x"}"#),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(!r.raw.contains("wrapped_key"));
    let r = send(&f, "PUT", "/api/v1/security/data-policy", &[("if-match", "\"4\"")], Some(r#"{"region":"eu","classes":[{"class":"a","retention_days":0,"legal_hold_allowed":true}]}"#)).await;
    assert_eq!(r.status, 400, "retention_days below 1");
    let r = send(
        &f,
        "PUT",
        "/api/v1/security/data-policy",
        &[],
        Some(r#"{"region":"eu","classes":[]}"#),
    )
    .await;
    assert_eq!(r.status, 428);
    let r = send(
        &f,
        "POST",
        "/api/v1/security/retention/holds",
        &[],
        Some(r#"{"scope":"s","reason":"r"}"#),
    )
    .await;
    assert_eq!(r.status, 400, "Idempotency-Key required");
    let r = send_to(
        &f.router,
        "GET",
        "/api/v1/security/data-policy",
        None,
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 401);
    let big = format!("{{\"region\":\"{}\"}}", "x".repeat(70_000));
    let r = send(
        &f,
        "POST",
        "/api/v1/security/residency/check",
        &[],
        Some(&big),
    )
    .await;
    assert_eq!(r.status, 413);
    assert!(f.fake.ops().is_empty());
}

#[tokio::test]
async fn cursors_are_bound_and_port_errors_are_filtered() {
    let f = fixture();
    let first = send(&f, "GET", "/api/v1/security/retention/runs", &[], None).await;
    let cursor = first.json()["next_cursor"]
        .as_str()
        .unwrap()
        .replace('|', "%7C");
    let ok = send(
        &f,
        "GET",
        &format!("/api/v1/security/retention/runs?cursor={cursor}"),
        &[],
        None,
    )
    .await;
    assert_eq!(ok.status, 200);
    let other = send_to(
        &f.router,
        "GET",
        &format!("/api/v1/security/retention/runs?cursor={cursor}"),
        Some("tok-bob"),
        &[],
        None,
    )
    .await;
    assert_eq!(other.status, 422);
    for q in ["limit=0", "limit=101", "x=1", "cursor="] {
        let r = send(
            &f,
            "GET",
            &format!("/api/v1/security/retention/runs?{q}"),
            &[],
            None,
        )
        .await;
        assert_eq!(r.status, 400, "{q}");
    }
    *f.fake.deny_with.lock().unwrap() = Some(ApiError::from_code(Code::SecurityNotFound));
    let r = send(&f, "GET", "/api/v1/security/node-keys", &[], None).await;
    assert_eq!((r.status, r.code().as_str()), (404, "security.not_found"));
    *f.fake.deny_with.lock().unwrap() =
        Some(ApiError::new(Code::PrintRenderFailed, "secret detail"));
    let r = send(&f, "GET", "/api/v1/security/node-keys", &[], None).await;
    assert_eq!((r.status, r.code().as_str()), (500, "http.internal"));
    assert!(!r.raw.contains("secret detail"));
}
