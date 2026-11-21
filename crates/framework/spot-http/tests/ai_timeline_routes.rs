//! `GET /api/v1/resource/{doctype}/{id}/ai-timeline` (`resource_ai_timeline`, WP-AI-11.T11): the
//! shape of a request and of an answer, with a fake [`AiTimelinePort`]. Who the caller is, whether
//! the caller may read the document, which values are masked and whether the access can be decided
//! belong to the port and are tested with the host's adapter (`spot-cli`,
//! `tests/ai_audit_timeline_host.rs`).
//!
//! The fake decides nothing the adapter decides; it answers what a test scripts and keeps what the
//! route passed it, so the tests prove the route's half of the bargain: what is checked before the
//! port, what is passed to it (never a viewer id) and how its answer becomes the response.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_core::{Audience, ManualClock, SiteId};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::ports::ai_timeline::{AiTimelinePort, TimelineRow};
use spot_http::api::router::routes::{self, RouteState, ai_timeline};
use spot_http::api::service::BoxFuture;
use spot_http::api::{
    ApiError, ApiServices, AuthService, Code, LoginBody, Ports, RequestCtx, Session, SessionInfo,
    router_for, router_with,
};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

#[allow(dead_code)]
mod support;

use support::fake::FakeApp;

const HOST: &str = "erp.acme.com";
const TOKEN: &str = "tok-alice";
const DOCTYPE: &str = "customer";
const ID: &str = "0190a1b2-c3d4-7e5f-8a6b-1c2d3e4f5a6b";
const SECRET_VALUE: &str = "distinctive-argument-value";
const PROMPT: &str = "distinctive-prompt-text";

/// What the fake answers next.
enum Next {
    Rows(Vec<TimelineRow>),
    Fail(ApiError),
}

/// What the port was asked: the token of the call, the DocType, the id and the limit.
type Asked = (String, String, String, usize);

#[derive(Default)]
struct Script {
    next: Option<Next>,
    calls: Vec<Asked>,
}

struct Fake {
    script: Mutex<Script>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Fake {
            script: Mutex::new(Script::default()),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn calls(&self) -> Vec<Asked> {
        self.lock().calls.clone()
    }

    fn answer(&self, rows: Vec<TimelineRow>) {
        self.lock().next = Some(Next::Rows(rows));
    }

    fn refuse(&self, error: ApiError) {
        self.lock().next = Some(Next::Fail(error));
    }
}

fn row(seq: u64, value: &str) -> TimelineRow {
    TimelineRow {
        seq,
        at: "2026-11-21T09:05:00.123456Z".to_owned(),
        kind: "tool_call".to_owned(),
        agent: "agent-1".to_owned(),
        acting_user: "user-1".to_owned(),
        tool: "update_doc".to_owned(),
        outcome: "ok".to_owned(),
        arguments: vec![("title".to_owned(), value.to_owned())],
    }
}

impl AiTimelinePort for Fake {
    fn timeline<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<TimelineRow>, ApiError>> {
        Box::pin(async move {
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .unwrap_or_default();
            let mut script = self.lock();
            script
                .calls
                .push((token, doctype.to_owned(), id.to_owned(), limit));
            match script.next.take() {
                Some(Next::Fail(error)) => Err(error),
                Some(Next::Rows(rows)) => Ok(rows),
                None => Ok(Vec::new()),
            }
        })
    }
}

struct Fixture {
    router: Router,
    fake: Arc<Fake>,
}

fn fixture() -> Fixture {
    let fake = Fake::new();
    let mut ports = Ports::new();
    ports.provide::<dyn AiTimelinePort>(fake.clone());
    let state = RouteState::new(ports.subset(ai_timeline::required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(ai_timeline::routes().with_state(state))
    .build();
    Fixture { router, fake }
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    text: String,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_str(&self.text).unwrap_or(Json::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn get(router: &Router, token: Option<&str>, uri: &str) -> Reply {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn uri(query: &str) -> String {
    let base = format!("/api/v1/resource/{DOCTYPE}/{ID}/ai-timeline");
    if query.is_empty() {
        base
    } else {
        format!("{base}?{query}")
    }
}

async fn timeline(f: &Fixture, query: &str) -> Reply {
    get(&f.router, Some(TOKEN), &uri(query)).await
}

#[tokio::test]
async fn the_answer_is_exactly_the_contracts_shape_and_is_never_cached() {
    let f = fixture();
    f.fake.answer(vec![row(9, "Q3 plan"), row(4, "***")]);
    let reply = timeline(&f, "").await;
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert_eq!(
        reply.json(),
        json!({ "data": { "entries": [
            {
                "seq": 9,
                "at": "2026-11-21T09:05:00.123456Z",
                "kind": "tool_call",
                "agent": "agent-1",
                "acting_user": "user-1",
                "tool": "update_doc",
                "outcome": "ok",
                "arguments": [{ "name": "title", "value": "Q3 plan" }],
            },
            {
                "seq": 4,
                "at": "2026-11-21T09:05:00.123456Z",
                "kind": "tool_call",
                "agent": "agent-1",
                "acting_user": "user-1",
                "tool": "update_doc",
                "outcome": "ok",
                "arguments": [{ "name": "title", "value": "***" }],
            },
        ] } })
    );
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    assert!(reply.header("x-request-id").is_some());
    // The answer is the entries and nothing else: no count, no hint of hidden entries.
    let body = reply.json();
    assert_eq!(body.as_object().unwrap().len(), 1);
    assert_eq!(body["data"].as_object().unwrap().len(), 1);
    let calls = f.fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0],
        (
            TOKEN.to_owned(),
            DOCTYPE.to_owned(),
            ID.to_owned(),
            50,
            // the caller is the credential, the limit is the contract's default
        )
    );
}

#[tokio::test]
async fn an_empty_list_is_a_200() {
    let f = fixture();
    let reply = timeline(&f, "limit=5").await;
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert_eq!(reply.json(), json!({ "data": { "entries": [] } }));
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    assert_eq!(f.fake.calls().len(), 1);
}

#[tokio::test]
async fn a_limit_from_one_to_a_hundred_reaches_the_port() {
    let f = fixture();
    for (query, expected) in [("limit=1", 1), ("limit=100", 100), ("limit=37", 37)] {
        let reply = timeline(&f, query).await;
        assert_eq!(reply.status, 200, "{query}: {}", reply.text);
        let calls = f.fake.calls();
        assert_eq!(calls.last().unwrap().3, expected, "{query}");
    }
}

#[tokio::test]
async fn a_limit_outside_one_to_a_hundred_is_refused_before_the_port_is_asked() {
    let f = fixture();
    for query in [
        "limit=0",
        "limit=101",
        "limit=-1",
        "limit=",
        "limit=ten",
        "limit=1.5",
        "limit=99999",
        "limit=5&limit=6",
        "limit=5&since=1",
        "since=1",
    ] {
        let reply = timeline(&f, query).await;
        assert_eq!(reply.status, 400, "{query}: {}", reply.text);
        assert_eq!(reply.code(), "api.invalid_request", "{query}");
        assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    }
    assert!(
        f.fake.calls().is_empty(),
        "no malformed call reached the port"
    );
}

#[tokio::test]
async fn a_path_the_contract_does_not_allow_is_the_empty_list_and_never_a_400() {
    let f = fixture();
    // Too long for the contract's bound: the same answer as a missing document, port not asked.
    let long = "x".repeat(129);
    for path in [
        format!("/api/v1/resource/{long}/{ID}/ai-timeline"),
        format!("/api/v1/resource/{DOCTYPE}/{long}/ai-timeline"),
    ] {
        let reply = get(&f.router, Some(TOKEN), &path).await;
        assert_eq!(reply.status, 200, "{}", reply.text);
        assert_eq!(reply.json(), json!({ "data": { "entries": [] } }));
        assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    }
    assert!(f.fake.calls().is_empty());

    // A name or id of the wrong shape is handed to the port, which answers the same empty list
    // for anything that names nothing: the route never calls it a bad request.
    for (doctype, id) in [("no_such_doctype", "L3-1"), ("Customer", "not-an-id")] {
        let reply = get(
            &f.router,
            Some(TOKEN),
            &format!("/api/v1/resource/{doctype}/{id}/ai-timeline"),
        )
        .await;
        assert_eq!(reply.status, 200, "{}", reply.text);
        assert_eq!(reply.json(), json!({ "data": { "entries": [] } }));
    }
    let calls = f.fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, "no_such_doctype");
    assert_eq!(calls[0].2, "L3-1");
    assert_eq!(calls[1].1, "Customer");
    assert_eq!(calls[1].2, "not-an-id");
}

#[tokio::test]
async fn no_bearer_is_401_and_the_port_is_not_asked() {
    let f = fixture();
    let reply = get(&f.router, None, &uri("")).await;
    assert_eq!(reply.status, 401, "{}", reply.text);
    assert_eq!(reply.code(), "auth.required");
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    assert!(f.fake.calls().is_empty());
}

#[tokio::test]
async fn an_undecidable_access_is_the_503_with_retry_after_and_never_entries() {
    let f = fixture();
    f.fake.refuse(
        ApiError::from_code(Code::ServerUnavailable)
            .detail(spot_http::api::storage_error::RETRY_AFTER_DETAIL, json!(7)),
    );
    let reply = timeline(&f, "").await;
    assert_eq!(reply.status, 503, "{}", reply.text);
    assert_eq!(reply.code(), "server.unavailable");
    assert_eq!(reply.header("retry-after"), Some("7"));
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    assert!(reply.json().get("data").is_none(), "{}", reply.text);

    // A port that sent no wait still gets one: the router never answers a 503 without it.
    f.fake.refuse(ApiError::from_code(Code::ServerUnavailable));
    let reply = timeline(&f, "").await;
    assert_eq!(reply.status, 503, "{}", reply.text);
    assert!(reply.header("retry-after").is_some());
}

#[tokio::test]
async fn the_ports_other_refusals_keep_their_codes_and_statuses() {
    let f = fixture();
    for (error, status, code) in [
        (
            ApiError::from_code(Code::AuthSessionExpired),
            401,
            "auth.session_expired",
        ),
        (
            ApiError::from_code(Code::PermissionDenied),
            403,
            "permission.denied",
        ),
    ] {
        f.fake.refuse(error);
        let reply = timeline(&f, "").await;
        assert_eq!(reply.status, status, "{code}: {}", reply.text);
        assert_eq!(reply.code(), code);
        assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    }
}

#[tokio::test]
async fn a_row_that_breaks_the_contract_or_a_port_that_overshoots_is_a_500() {
    let f = fixture();
    let mut bad = row(1, "v");
    bad.outcome = "maybe".to_owned();
    f.fake.answer(vec![bad]);
    let reply = timeline(&f, "").await;
    assert_eq!(reply.status, 500, "{}", reply.text);
    assert!(!reply.text.contains("maybe"), "{}", reply.text);

    let mut long = row(2, &"v".repeat(129));
    long.arguments.push(("n".to_owned(), "w".to_owned()));
    f.fake.answer(vec![long]);
    assert_eq!(timeline(&f, "").await.status, 500);

    f.fake.answer(vec![row(1, "a"), row(2, "b"), row(3, "c")]);
    let reply = timeline(&f, "limit=2").await;
    assert_eq!(reply.status, 500, "{}", reply.text);
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
}

// ------------------------------------------------------------------------------------------
// The log never holds a value.

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct Lines<'a>(&'a mut Vec<String>);

impl Visit for Lines<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push(format!("{}={value}", field.name()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push(format!("{}={value:?}", field.name()));
    }
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_new_span(&self, attrs: &Attributes<'_>, _id: &Id, _ctx: Context<'_, S>) {
        let mut lines = vec![format!("span {}", attrs.metadata().name())];
        attrs.record(&mut Lines(&mut lines));
        self.0.lock().unwrap().extend(lines);
    }

    fn on_record(&self, _id: &Id, values: &Record<'_>, _ctx: Context<'_, S>) {
        let mut lines = Vec::new();
        values.record(&mut Lines(&mut lines));
        self.0.lock().unwrap().extend(lines);
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut lines = vec![format!("event {}", event.metadata().target())];
        event.record(&mut Lines(&mut lines));
        self.0.lock().unwrap().extend(lines);
    }
}

#[tokio::test]
async fn no_argument_value_or_prompt_reaches_a_log_line() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(
        Registry::default()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(capture.clone()),
    );
    let f = fixture();
    // A success with a value, an empty answer, a refusal, a malformed limit and a broken row.
    f.fake.answer(vec![row(1, SECRET_VALUE)]);
    let ok = timeline(&f, "").await;
    assert!(ok.text.contains(SECRET_VALUE), "the caller gets the entry");
    timeline(&f, "").await;
    f.fake.refuse(ApiError::from_code(Code::ServerUnavailable));
    timeline(&f, "").await;
    timeline(&f, &format!("limit={PROMPT}")).await;
    let mut bad = row(2, SECRET_VALUE);
    bad.outcome = PROMPT.to_owned();
    f.fake.answer(vec![bad]);
    timeline(&f, "").await;
    let lines = capture.0.lock().unwrap().clone();
    for line in &lines {
        assert!(
            !line.contains(SECRET_VALUE),
            "an argument value in a log line: {line}"
        );
        assert!(!line.contains(PROMPT), "a prompt in a log line: {line}");
    }
}

// ------------------------------------------------------------------------------------------
// Mounting and the audience.

#[test]
fn the_group_serves_the_one_operation_authenticated() {
    assert_eq!(ai_timeline::SERVES, &["resource_ai_timeline"]);
    let registration = ai_timeline::registration();
    assert!(
        registration.public.is_none(),
        "the timeline is never public"
    );
    assert_eq!(registration.name, "ai_timeline");
    assert_eq!((registration.required_ports)().len(), 1);
}

#[test]
fn a_group_without_its_port_is_not_mounted() {
    let mounted = routes::mount(&[ai_timeline::registration()], &Ports::new(), &[]);
    assert!(mounted.report.mounted.is_empty());
    assert!(
        mounted
            .report
            .skipped
            .iter()
            .any(|(name, _)| *name == "ai_timeline"),
        "the group is skipped when its port is absent"
    );
    assert!(mounted.report.served.is_empty());
}

fn services_with(fake: Option<Arc<Fake>>, auth: Arc<dyn AuthService>) -> ApiServices {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let app = FakeApp::new(clock);
    let services = ApiServices::new(app.clone(), app.clone(), app, auth, store);
    match fake {
        Some(fake) => services.with_port::<dyn AiTimelinePort>(fake),
        None => services,
    }
}

fn site_router(api: Router) -> Router {
    let acme = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(api)
    .build()
}

#[tokio::test]
async fn without_the_port_the_operation_answers_as_an_unserved_one_does() {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let app = FakeApp::new(clock);
    let services = services_with(None, app);
    let (api, report) = router_with(services, &[ai_timeline::registration()]);
    assert!(report.mounted.is_empty(), "{report:?}");
    let router = site_router(api);
    let reply = get(&router, Some("test-token"), &uri("")).await;
    assert_eq!(reply.status, 404, "{}", reply.text);
}

/// Reports the audience a test token stands for; everything else is the fake app's.
struct Audiences {
    inner: Arc<FakeApp>,
    table: HashMap<&'static str, Audience>,
}

impl AuthService for Audiences {
    fn login<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        body: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>> {
        self.inner.login(ctx, body)
    }

    fn logout<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>> {
        self.inner.logout(ctx)
    }

    fn session<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>> {
        self.inner.session(ctx)
    }

    fn audience_of<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<Audience, ApiError>> {
        let answer = ctx
            .credential
            .as_ref()
            .and_then(|credential| self.table.get(credential.token.as_str()).copied());
        Box::pin(
            async move { answer.ok_or_else(|| ApiError::from_code(Code::AuthInvalidCredentials)) },
        )
    }
}

#[tokio::test]
async fn a_consumer_and_an_anonymous_caller_are_refused_before_the_port_is_called() {
    let fake = Fake::new();
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let auth = Arc::new(Audiences {
        inner: FakeApp::new(clock),
        table: HashMap::from([
            ("tok-staff", Audience::Staff),
            ("tok-partner", Audience::Partner),
            ("tok-consumer", Audience::Consumer),
        ]),
    });
    let services = services_with(Some(fake.clone()), auth);
    let router = site_router(router_for(&[Audience::Staff, Audience::Partner], services));

    let consumer = get(&router, Some("tok-consumer"), &uri("")).await;
    assert_eq!(consumer.status, 403, "{}", consumer.text);
    let anonymous = get(&router, None, &uri("")).await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.text);
    assert!(fake.calls().is_empty(), "neither reached the port");

    // Staff and partner are served.
    for token in ["tok-staff", "tok-partner"] {
        let reply = get(&router, Some(token), &uri("")).await;
        assert_eq!(reply.status, 200, "{token}: {}", reply.text);
        assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    }
    assert_eq!(fake.calls().len(), 2);

    // A consumer listener has no such route at all.
    let consumer_only = {
        let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
        let auth = FakeApp::new(clock);
        site_router(router_for(
            &[Audience::Consumer],
            services_with(Some(fake.clone()), auth),
        ))
    };
    let absent = get(&consumer_only, Some("tok-consumer"), &uri("")).await;
    assert_eq!(absent.status, 404, "{}", absent.text);
}
