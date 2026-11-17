//! The `/api/v1/governance/*` routes (WP-IAM-08.T8, contract 0.19.0) over a fake
//! `GovernancePort`, driven through the real site layer without a socket.
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
use spot_http::api::ports::governance::{
    Area, GovernanceCall, GovernancePort, GovernanceReply, Op,
};
use spot_http::api::router::routes::{RouteState, governance, mount};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const U1: &str = "0190f6c0-0000-7000-8000-000000000001";
const U2: &str = "0190f6c0-0000-7000-8000-000000000002";

// --- the fake port ----------------------------------------------------------------------------

struct FakePort {
    unserved: Mutex<BTreeSet<Area>>,
    step_up: Mutex<bool>,
    calls: Mutex<Vec<(Op, GovernanceCall, String)>>,
    deny_with: Mutex<Option<ApiError>>,
}

impl FakePort {
    fn new() -> Arc<Self> {
        Arc::new(FakePort {
            unserved: Mutex::new(BTreeSet::new()),
            step_up: Mutex::new(true),
            calls: Mutex::new(Vec::new()),
            deny_with: Mutex::new(None),
        })
    }
    fn ops(&self) -> Vec<Op> {
        self.calls.lock().unwrap().iter().map(|c| c.0).collect()
    }
    fn last(&self) -> GovernanceCall {
        self.calls.lock().unwrap().last().unwrap().1.clone()
    }
}

fn caller_of(ctx: &RequestCtx) -> String {
    ctx.credential
        .as_ref()
        .map(|c| c.token.clone())
        .unwrap_or_default()
}

impl GovernancePort for FakePort {
    fn serves(&self, area: Area) -> bool {
        !self.unserved.lock().unwrap().contains(&area)
    }
    fn step_up_current<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<bool, ApiError>> {
        Box::pin(async move { Ok(*self.step_up.lock().unwrap()) })
    }
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        call: &'a GovernanceCall,
    ) -> BoxFuture<'a, Result<GovernanceReply, ApiError>> {
        Box::pin(async move {
            let caller = caller_of(ctx);
            self.calls
                .lock()
                .unwrap()
                .push((call.op, call.clone(), caller.clone()));
            if let Some(error) = self.deny_with.lock().unwrap().clone() {
                return Err(error);
            }
            let mut etag = None;
            if call.op == Op::SodRulesGet {
                etag = Some("\"2\"".to_owned());
            }
            let Some(page) = &call.page else {
                return Ok(GovernanceReply {
                    data: json!({"ok": true}),
                    next_cursor: None,
                    etag,
                });
            };
            // A cursor is bound to the caller and the query digest.
            let bound = format!("{caller}|{}", page.query_digest);
            if let Some(cursor) = &page.cursor
                && cursor != &bound
            {
                return Err(ApiError::from_code(Code::GovernanceInvalid));
            }
            let next = page.cursor.is_none().then_some(bound);
            Ok(GovernanceReply {
                data: json!([]),
                next_cursor: Some(next),
                etag,
            })
        })
    }
}

// --- harness ----------------------------------------------------------------------------------

struct Fixture {
    router: Router,
    port: Arc<FakePort>,
}

fn fixture() -> Fixture {
    let port = FakePort::new();
    let mut ports = Ports::new();
    ports.provide::<dyn GovernancePort>(port.clone());
    let state = RouteState::new(
        ports
            .subset(governance::governance_required_ports())
            .unwrap(),
    );
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(governance::governance_routes().with_state(state))
    .build();
    Fixture { router, port }
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

async fn send(
    f: &Fixture,
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
    let response = f.router.clone().oneshot(request).await.unwrap();
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

const KEY: (&str, &str) = ("idempotency-key", "k-1");

fn reason(n: usize) -> String {
    "r".repeat(n)
}

/// One valid request per operation: (op, method, path, headers, body, status).
#[allow(clippy::type_complexity)]
fn valid_requests() -> Vec<(
    Op,
    &'static str,
    String,
    Vec<(&'static str, &'static str)>,
    Option<String>,
    u16,
)> {
    let rule = json!({"rules": [{"id": "r1", "name": "n", "roles": [U1], "kind": "exclusive",
        "severity": "high", "enabled": true}], "version": 1});
    vec![
        (Op::GrantRequestList, "GET", "/api/v1/governance/grant-requests?status=pending&limit=10".into(), vec![], None, 200),
        (Op::GrantRequestCreate, "POST", "/api/v1/governance/grant-requests".into(), vec![KEY],
            Some(json!({"user_id": U1, "role_id": U2, "reason": "need it"}).to_string()), 201),
        (Op::GrantRequestDecide, "POST", "/api/v1/governance/grant-requests/g1/decide".into(), vec![KEY],
            Some(json!({"decision": "approve", "reason": "ok"}).to_string()), 200),
        (Op::SodRulesGet, "GET", "/api/v1/governance/sod/rules".into(), vec![], None, 200),
        (Op::SodRulesPut, "PUT", "/api/v1/governance/sod/rules".into(), vec![("if-match", "\"2\"")], Some(rule.to_string()), 200),
        (Op::SodAnalysis, "GET", "/api/v1/governance/sod/analysis?rule=r1".into(), vec![], None, 200),
        (Op::SodOverrideList, "GET", "/api/v1/governance/sod/overrides".into(), vec![], None, 200),
        (Op::SodOverrideCreate, "POST", "/api/v1/governance/sod/overrides".into(), vec![KEY],
            Some(json!({"rule_id": "r1", "user_id": U1, "reason": "x", "mitigating_control": "y",
                "expires_at": "2026-12-01T00:00:00Z"}).to_string()), 201),
        (Op::ReviewList, "GET", "/api/v1/governance/access-reviews".into(), vec![], None, 200),
        (Op::ReviewCreate, "POST", "/api/v1/governance/access-reviews".into(), vec![KEY],
            Some(json!({"name": "Q4", "due_at": "2026-12-01T00:00:00Z", "reviewers": "manager"}).to_string()), 201),
        (Op::ReviewGet, "GET", "/api/v1/governance/access-reviews/rv1".into(), vec![], None, 200),
        (Op::ReviewItems, "GET", "/api/v1/governance/access-reviews/rv1/items".into(), vec![], None, 200),
        (Op::ReviewItemDecide, "POST", "/api/v1/governance/access-reviews/rv1/items/it1/decide".into(), vec![KEY],
            Some(json!({"decision": "keep"}).to_string()), 200),
        (Op::ReviewClose, "POST", "/api/v1/governance/access-reviews/rv1/close".into(), vec![], None, 200),
        (Op::ReviewReport, "GET", "/api/v1/governance/access-reviews/rv1/report".into(), vec![], None, 200),
        (Op::QuestionUserPermissions, "GET", format!("/api/v1/governance/questions/user-permissions?user={U1}"), vec![], None, 200),
        (Op::QuestionWhoCan, "GET", "/api/v1/governance/questions/who-can?permission=selling.read".into(), vec![], None, 200),
        (Op::QuestionWhoCanSee, "GET", format!("/api/v1/governance/questions/who-can-see?doctype=Customer&id={U1}"), vec![], None, 200),
        (Op::QuestionChanges, "GET", "/api/v1/governance/questions/changes?from=2026-01-01T00:00:00Z&to=2026-02-01T00:00:00Z".into(), vec![], None, 200),
        (Op::EmergencyCreate, "POST", "/api/v1/governance/emergency-access".into(), vec![KEY],
            Some(json!({"reason": "outage", "roles": [U1], "controller_id": U2, "duration_minutes": 60}).to_string()), 201),
        (Op::EmergencyList, "GET", "/api/v1/governance/emergency-access".into(), vec![], None, 200),
        (Op::EmergencyReview, "POST", "/api/v1/governance/emergency-access/e1/review".into(), vec![],
            Some(json!({"outcome": "justified"}).to_string()), 200),
    ]
}

#[tokio::test]
async fn all_22_operations_are_mounted_and_reach_the_port() {
    let f = fixture();
    let requests = valid_requests();
    assert_eq!(requests.len(), Op::ALL.len());
    for (op, method, path, headers, body, status) in &requests {
        let r = send(
            &f,
            method,
            path,
            Some("tok-alice"),
            headers,
            body.as_deref(),
        )
        .await;
        assert_eq!(r.status, *status, "{} {}: {}", method, path, r.raw);
        assert_eq!(
            r.headers.get(header::CACHE_CONTROL).unwrap(),
            "private, no-store",
            "{}",
            op.id()
        );
        assert_eq!(f.port.ops().last(), Some(op), "{path}");
    }
    let mut seen = f.port.ops();
    seen.sort();
    let mut all = Op::ALL.to_vec();
    all.sort();
    assert_eq!(seen, all);
    let ids: BTreeSet<&str> = Op::ALL.iter().map(|o| o.id()).collect();
    let served: BTreeSet<&str> = governance::GOVERNANCE_SERVES.iter().copied().collect();
    assert_eq!(ids, served);
}

#[tokio::test]
async fn path_ids_and_the_etag_reach_the_port_and_the_reply() {
    let f = fixture();
    let h = [KEY];
    let body = json!({"decision": "remove", "comment": "left the team"}).to_string();
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/access-reviews/rv9/items/it7/decide",
        Some("t"),
        &h,
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 200);
    let call = f.port.last();
    assert_eq!(call.path, ["rv9", "it7"]);
    assert_eq!(call.idempotency_key.as_deref(), Some("k-1"));
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/sod/rules",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.headers.get(header::ETAG).unwrap(), "\"2\"");
}

#[tokio::test]
async fn an_unset_area_answers_501_and_the_others_keep_working() {
    let f = fixture();
    f.port.unserved.lock().unwrap().insert(Area::Sod);
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/sod/rules",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 501);
    assert_eq!(r.code(), "service.not_configured");
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/sod/overrides",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 501);
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/access-reviews",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(!f.port.ops().contains(&Op::SodRulesGet));
}

#[tokio::test]
async fn the_group_is_not_mounted_without_its_port() {
    let mounted = mount(&[governance::registration()], &Ports::new(), &[]);
    assert!(mounted.report.mounted.is_empty());
    assert_eq!(mounted.report.skipped.len(), 1);
    let mut ports = Ports::new();
    ports.provide::<dyn GovernancePort>(FakePort::new());
    let mounted = mount(&[governance::registration()], &ports, &[]);
    assert_eq!(mounted.report.mounted, ["governance"]);
    assert_eq!(mounted.report.served.len(), 22);
}

#[tokio::test]
async fn page_bounds_and_unknown_parameters_are_refused_before_the_port() {
    let f = fixture();
    for q in [
        "limit=0",
        "limit=101",
        "limit=x",
        "limit=1&limit=2",
        "cursor=",
        "other=1",
        "status=bogus",
    ] {
        let r = send(
            &f,
            "GET",
            &format!("/api/v1/governance/grant-requests?{q}"),
            Some("t"),
            &[],
            None,
        )
        .await;
        assert_eq!(r.status, 400, "{q}");
    }
    let long = "c".repeat(2049);
    let r = send(
        &f,
        "GET",
        &format!("/api/v1/governance/grant-requests?cursor={long}"),
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 400);
    // A listing that takes no filter refuses one.
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/access-reviews?status=pending",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 400);
    // Required filters.
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/questions/user-permissions",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 400);
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/questions/user-permissions?user=not-a-uuid",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(f.port.ops().is_empty());
}

#[tokio::test]
async fn a_cursor_is_bound_to_the_caller_and_the_query() {
    let f = fixture();
    let path = "/api/v1/governance/grant-requests?status=pending";
    let first = send(&f, "GET", path, Some("alice"), &[], None).await;
    let cursor = first.json()["next_cursor"].as_str().unwrap().to_owned();
    let page2 = format!("{path}&cursor={}", cursor.replace('|', "%7C"));
    let ok = send(&f, "GET", &page2, Some("alice"), &[], None).await;
    assert_eq!(ok.status, 200, "{}", ok.raw);
    assert_eq!(ok.json()["next_cursor"], Json::Null);
    // Another user replaying it answers like a garbage cursor.
    let other = send(&f, "GET", &page2, Some("bob"), &[], None).await;
    let garbage = send(
        &f,
        "GET",
        &format!("{path}&cursor=garbage"),
        Some("alice"),
        &[],
        None,
    )
    .await;
    assert_eq!(
        (other.status, other.code()),
        (422, "governance.invalid".to_owned())
    );
    assert_eq!(
        other.json()["error"]["message"],
        garbage.json()["error"]["message"]
    );
    assert_eq!(other.status, garbage.status);
    // The same cursor on other filters answers the same way.
    let other_query = format!(
        "/api/v1/governance/grant-requests?status=approved&cursor={}",
        cursor.replace('|', "%7C")
    );
    let r = send(&f, "GET", &other_query, Some("alice"), &[], None).await;
    assert_eq!(r.code(), "governance.invalid");
}

#[tokio::test]
async fn bodies_take_no_actor_and_unknown_fields_are_refused_without_echo() {
    let f = fixture();
    let h = [KEY];
    for extra in ["approver", "reviewer", "actor", "tenant", "zzz-secret-key"] {
        let body = json!({"decision": "approve", "reason": "ok", extra: U1}).to_string();
        let r = send(
            &f,
            "POST",
            "/api/v1/governance/grant-requests/g1/decide",
            Some("t"),
            &h,
            Some(&body),
        )
        .await;
        assert_eq!(r.status, 400, "{extra}");
        assert!(!r.raw.contains(extra), "the key is echoed: {}", r.raw);
    }
    let body = json!({"decision": "keep", "reviewer": U1}).to_string();
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/access-reviews/rv1/items/it1/decide",
        Some("t"),
        &h,
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(f.port.ops().is_empty());
}

#[tokio::test]
async fn text_is_bounded_and_never_echoed() {
    let f = fixture();
    let h = [KEY];
    let long = reason(501);
    let body = json!({"decision": "approve", "reason": long}).to_string();
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/grant-requests/g1/decide",
        Some("t"),
        &h,
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(!r.raw.contains("rrrrr"));
    let body = json!({"decision": "approve", "reason": ""}).to_string();
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/grant-requests/g1/decide",
        Some("t"),
        &h,
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 400);
    // A 500 character reason passes and reaches the port unchanged.
    let exact = reason(500);
    let body = json!({"decision": "approve", "reason": exact}).to_string();
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/grant-requests/g1/decide",
        Some("t"),
        &h,
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(f.port.last().body.unwrap()["reason"], exact.as_str());
    // A body larger than 64 KiB is 413.
    let huge = format!(
        "{{\"decision\":\"approve\",\"reason\":\"{}\"}}",
        reason(70_000)
    );
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/grant-requests/g1/decide",
        Some("t"),
        &h,
        Some(&huge),
    )
    .await;
    assert_eq!(r.status, 413);
}

#[tokio::test]
async fn a_decision_or_write_without_a_recent_step_up_never_reaches_the_operation() {
    let f = fixture();
    *f.port.step_up.lock().unwrap() = false;
    for (op, method, path, headers, body, _) in valid_requests() {
        let r = send(&f, method, &path, Some("t"), &headers, body.as_deref()).await;
        let needs = !matches!(method, "GET") && op != Op::GrantRequestCreate;
        if needs {
            assert_eq!(r.status, 403, "{}", op.id());
            assert_eq!(r.code(), "auth.step_up_required");
        } else {
            assert_eq!(r.status / 100, 2, "{}: {}", op.id(), r.raw);
        }
    }
    let called = f.port.ops();
    assert!(!called.contains(&Op::GrantRequestDecide));
    assert!(!called.contains(&Op::SodRulesPut));
    assert!(!called.contains(&Op::EmergencyCreate));
    assert!(called.contains(&Op::GrantRequestCreate));
}

#[tokio::test]
async fn required_headers_and_the_bearer_are_checked_first() {
    let f = fixture();
    let body = json!({"user_id": U1, "role_id": U2, "reason": "x"}).to_string();
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/grant-requests",
        Some("t"),
        &[],
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 400, "missing Idempotency-Key");
    let long_key = "k".repeat(129);
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/grant-requests",
        Some("t"),
        &[("idempotency-key", &long_key)],
        Some(&body),
    )
    .await;
    assert_eq!(r.status, 400);
    let rules = json!({"rules": []}).to_string();
    let r = send(
        &f,
        "PUT",
        "/api/v1/governance/sod/rules",
        Some("t"),
        &[],
        Some(&rules),
    )
    .await;
    assert_eq!(r.status, 428);
    let r = send(
        &f,
        "PUT",
        "/api/v1/governance/sod/rules",
        Some("t"),
        &[("if-match", "W/\"1\"")],
        Some(&rules),
    )
    .await;
    assert_eq!(r.status, 428);
    let r = send(&f, "GET", "/api/v1/governance/sod/rules", None, &[], None).await;
    assert_eq!(r.status, 401);
    let r = send(
        &f,
        "POST",
        "/api/v1/governance/grant-requests",
        Some("t"),
        &[KEY],
        Some("[]"),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(f.port.ops().is_empty());
}

#[tokio::test]
async fn a_port_error_is_rendered_only_for_the_codes_of_the_contract() {
    let f = fixture();
    *f.port.deny_with.lock().unwrap() = Some(ApiError::from_code(Code::GovernanceSelfApproval));
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/access-reviews/rv1",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (403, "governance.self_approval")
    );
    *f.port.deny_with.lock().unwrap() =
        Some(ApiError::new(Code::PrintRenderFailed, "secret detail"));
    let r = send(
        &f,
        "GET",
        "/api/v1/governance/access-reviews/rv1",
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (500, "http.internal"));
    assert!(!r.raw.contains("secret detail"));
}

#[tokio::test]
async fn an_id_that_is_not_visible_ascii_is_a_not_found() {
    let f = fixture();
    let long = "a".repeat(201);
    let r = send(
        &f,
        "GET",
        &format!("/api/v1/governance/access-reviews/{long}"),
        Some("t"),
        &[],
        None,
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (404, "governance.not_found"));
    assert!(f.port.ops().is_empty());
}

#[tokio::test]
async fn the_questions_pass_the_correlation_id_and_the_filters_only() {
    let f = fixture();
    let path = format!("/api/v1/governance/questions/who-can-see?doctype=Customer&id={U1}&limit=5");
    let r = send(&f, "GET", &path, Some("t"), &[], None).await;
    assert_eq!(r.status, 200);
    let call = f.port.last();
    assert_eq!(call.query["doctype"], "Customer");
    assert_eq!(call.query["id"], U1);
    assert_eq!(call.page.as_ref().unwrap().limit, 5);
    assert!(r.headers.get("x-request-id").is_some());
}
