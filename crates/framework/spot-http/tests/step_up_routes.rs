//! `POST /api/v1/auth/step-up` (`auth_step_up`, WP-IAM-12.T7): the shape of a request and of an
//! answer, with a fake [`StepUpPort`]. Who the caller is, whether the proof is right, the
//! throttle, the marker and the audit belong to the port and are tested with the host's adapter
//! (`spot-cli`, `tests/step_up_host.rs`).
//!
//! The fake decides nothing the adapter decides; it answers what a test scripts and keeps what the
//! route passed it, so the tests prove the route's half of the bargain: what is checked before the
//! port, what is passed to it (never a user id) and how its answer becomes the response.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_core::{Audience, ManualClock, SiteId};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::ports::step_up::{
    StepUpError, StepUpOut, StepUpPort, StepUpProof, StepUpResult,
};
use spot_http::api::router::routes::{self, RouteState, step_up};
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
const PASSWORD: &str = "correct-horse-distinctive-pw";
const CODE: &str = "246810";
const REDIRECT: &str = "https://idp.example.com/reauth?state=distinctive-redirect";

/// What the fake answers next.
enum Next {
    Ok(StepUpOut),
    Fail(StepUpError),
}

#[derive(Default)]
struct Script {
    next: Option<Next>,
    /// The proofs the port received, with the token of the call.
    calls: Vec<(String, StepUpProof)>,
}

struct Fake {
    script: Mutex<Script>,
}

impl Fake {
    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn calls(&self) -> Vec<(String, StepUpProof)> {
        self.lock().calls.clone()
    }

    fn answer(&self, out: StepUpOut) {
        self.lock().next = Some(Next::Ok(out));
    }

    fn refuse(&self, error: StepUpError) {
        self.lock().next = Some(Next::Fail(error));
    }
}

fn default_out() -> StepUpOut {
    StepUpOut {
        step_up_until: Some("2026-11-21T09:05:00Z".to_owned()),
        redirect_url: None,
    }
}

impl StepUpPort for Fake {
    fn step_up<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        proof: StepUpProof,
    ) -> BoxFuture<'a, StepUpResult<StepUpOut>> {
        Box::pin(async move {
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .unwrap_or_default();
            let mut script = self.lock();
            script.calls.push((token, proof));
            match script.next.take() {
                Some(Next::Fail(error)) => Err(error),
                Some(Next::Ok(out)) => Ok(out),
                None => Ok(default_out()),
            }
        })
    }
}

struct Fixture {
    router: Router,
    fake: Arc<Fake>,
}

fn fixture() -> Fixture {
    let fake = Arc::new(Fake {
        script: Mutex::new(Script::default()),
    });
    let mut ports = Ports::new();
    ports.provide::<dyn StepUpPort>(fake.clone());
    let state = RouteState::new(ports.subset(step_up::required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(step_up::routes().with_state(state))
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

async fn send(router: &Router, token: Option<&str>, body: Option<String>) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/step-up")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder
        .body(match body {
            Some(text) => Body::from(text),
            None => Body::empty(),
        })
        .unwrap();
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

async fn step(f: &Fixture, body: Json) -> Reply {
    send(&f.router, Some(TOKEN), Some(body.to_string())).await
}

#[tokio::test]
async fn a_password_step_up_answers_the_contract_shape_and_is_never_cached() {
    let f = fixture();
    let reply = step(&f, json!({ "password": PASSWORD })).await;
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert_eq!(
        reply.json(),
        json!({ "data": { "step_up_until": "2026-11-21T09:05:00Z", "redirect_url": null } })
    );
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert!(reply.header("x-request-id").is_some());
    let calls = f.fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, TOKEN, "the caller is the credential");
    assert_eq!(
        calls[0].1,
        StepUpProof {
            password: Some(PASSWORD.to_owned()),
            totp: None,
            sso: false
        }
    );
}

#[tokio::test]
async fn a_code_reaches_the_port_with_the_password_unchanged() {
    let f = fixture();
    let reply = step(&f, json!({ "password": PASSWORD, "totp": CODE })).await;
    assert_eq!(reply.status, 200, "{}", reply.text);
    let calls = f.fake.calls();
    assert_eq!(calls[0].1.password.as_deref(), Some(PASSWORD));
    assert_eq!(calls[0].1.totp.as_deref(), Some(CODE));
    assert!(!calls[0].1.sso);
}

#[tokio::test]
async fn an_sso_step_up_answers_the_redirect() {
    let f = fixture();
    f.fake.answer(StepUpOut {
        step_up_until: None,
        redirect_url: Some(REDIRECT.to_owned()),
    });
    let reply = step(&f, json!({ "sso": true })).await;
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert_eq!(
        reply.json(),
        json!({ "data": { "step_up_until": null, "redirect_url": REDIRECT } })
    );
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    let calls = f.fake.calls();
    assert_eq!(
        calls[0].1,
        StepUpProof {
            password: None,
            totp: None,
            sso: true
        }
    );
}

#[tokio::test]
async fn a_malformed_body_is_refused_before_the_port_is_asked() {
    let f = fixture();
    let long_password = "x".repeat(1025);
    let long_code = "1".repeat(17);
    for body in [
        json!({}),
        json!([]),
        json!("pw"),
        json!(null),
        json!({ "sso": false }),
        json!({ "totp": CODE }),
        json!({ "password": "" }),
        json!({ "password": PASSWORD, "totp": "" }),
        json!({ "password": long_password }),
        json!({ "password": PASSWORD, "totp": long_code }),
        json!({ "password": 1 }),
        json!({ "password": null }),
        json!({ "password": PASSWORD, "totp": 123456 }),
        json!({ "password": PASSWORD, "sso": "yes" }),
        json!({ "password": PASSWORD, "sso": true }),
        json!({ "totp": CODE, "sso": true }),
        json!({ "password": PASSWORD, "user": "ada" }),
        json!({ "password": PASSWORD, "user_id": concat!("0190a1b2-c3d4-7e5f-8a6b-", "1c2d3e4f5a6b") }),
    ] {
        let reply = step(&f, body.clone()).await;
        assert_eq!(reply.status, 400, "{body}");
        assert_eq!(reply.code(), "api.invalid_request", "{body}");
        assert_eq!(reply.header("cache-control"), Some("no-store"));
        // The answer never echoes the body.
        assert!(!reply.text.contains(PASSWORD), "{}", reply.text);
        assert!(!reply.text.contains(CODE), "{}", reply.text);
    }
    assert!(
        f.fake.calls().is_empty(),
        "no malformed call reached the port"
    );
}

#[tokio::test]
async fn an_empty_or_unparsable_body_is_refused_before_the_port_is_asked() {
    let f = fixture();
    let empty = send(&f.router, Some(TOKEN), None).await;
    assert_eq!(empty.status, 400, "{}", empty.text);
    let broken = send(&f.router, Some(TOKEN), Some("{\"password\": ".to_owned())).await;
    assert_eq!(broken.status, 400, "{}", broken.text);
    let wrong_type = {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/step-up")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::from(json!({ "password": PASSWORD }).to_string()))
            .unwrap();
        f.router.clone().oneshot(request).await.unwrap().status()
    };
    assert_eq!(wrong_type.as_u16(), 415);
    assert!(f.fake.calls().is_empty());
}

#[tokio::test]
async fn no_bearer_is_401_and_the_port_is_not_asked() {
    let f = fixture();
    let reply = send(
        &f.router,
        None,
        Some(json!({ "password": PASSWORD }).to_string()),
    )
    .await;
    assert_eq!(reply.status, 401, "{}", reply.text);
    assert_eq!(reply.code(), "auth.required");
    assert!(f.fake.calls().is_empty());
}

#[tokio::test]
async fn the_ports_refusals_keep_their_codes_and_statuses() {
    let f = fixture();
    for (error, status, code) in [
        (
            ApiError::from_code(Code::AuthInvalidCredentials),
            401,
            "auth.invalid_credentials",
        ),
        (
            ApiError::from_code(Code::AuthTotpRequired),
            401,
            "auth.totp_required",
        ),
        (
            ApiError::from_code(Code::AuthTotpInvalid),
            401,
            "auth.totp_invalid",
        ),
        (
            ApiError::from_code(Code::PermissionDenied),
            403,
            "permission.denied",
        ),
    ] {
        f.fake.refuse(StepUpError::Api(error));
        let reply = step(&f, json!({ "password": PASSWORD })).await;
        assert_eq!(reply.status, status, "{code}: {}", reply.text);
        assert_eq!(reply.code(), code);
        assert_eq!(reply.header("cache-control"), Some("no-store"));
        assert!(!reply.text.contains(PASSWORD), "{}", reply.text);
    }
}

#[tokio::test]
async fn a_throttled_call_is_the_rate_limit_answer_with_retry_after() {
    let f = fixture();
    f.fake.refuse(StepUpError::Api(
        ApiError::from_code(Code::ApiRateLimited)
            .detail(spot_http::api::storage_error::RETRY_AFTER_DETAIL, json!(37)),
    ));
    let reply = step(&f, json!({ "password": PASSWORD })).await;
    assert_eq!(reply.status, 429, "{}", reply.text);
    assert_eq!(reply.code(), "api.rate_limited");
    assert_eq!(reply.header("retry-after"), Some("37"));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
}

#[tokio::test]
async fn an_unconfigured_branch_is_501_service_not_configured() {
    let f = fixture();
    f.fake.refuse(StepUpError::NotConfigured);
    let reply = step(&f, json!({ "sso": true })).await;
    assert_eq!(reply.status, 501, "{}", reply.text);
    assert_eq!(reply.code(), "service.not_configured");
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert_eq!(reply.json()["error"]["retryable"], false);
}

#[tokio::test]
async fn a_repeat_is_a_repeat_and_an_idempotency_key_is_ignored() {
    let f = fixture();
    for _ in 0..2 {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/step-up")
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(
                "idempotency-key",
                concat!("0190a1b2-c3d4-7e5f-8a6b-", "1c2d3e4f5a6b"),
            )
            .body(Body::from(json!({ "password": PASSWORD }).to_string()))
            .unwrap();
        let response = f.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert!(response.headers().get("idempotency-replayed").is_none());
    }
    let calls = f.fake.calls();
    assert_eq!(calls.len(), 2, "both calls reached the port");
}

#[test]
fn the_proof_debug_is_redacted() {
    let proof = StepUpProof {
        password: Some(PASSWORD.to_owned()),
        totp: Some(CODE.to_owned()),
        sso: false,
    };
    let shown = format!("{proof:?}");
    assert!(!shown.contains(PASSWORD), "{shown}");
    assert!(!shown.contains(CODE), "{shown}");
}

// ------------------------------------------------------------------------------------------
// The log never holds a secret.

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
async fn no_password_code_or_redirect_reaches_a_log_line() {
    let capture = Capture::default();
    let _guard = tracing::subscriber::set_default(
        Registry::default()
            .with(tracing_subscriber::filter::LevelFilter::TRACE)
            .with(capture.clone()),
    );
    let f = fixture();
    // A success, a refusal, a malformed call and an SSO redirect.
    step(&f, json!({ "password": PASSWORD, "totp": CODE })).await;
    f.fake.refuse(StepUpError::Api(ApiError::from_code(
        Code::AuthInvalidCredentials,
    )));
    step(&f, json!({ "password": PASSWORD })).await;
    step(&f, json!({ "password": PASSWORD, "extra": CODE })).await;
    f.fake.answer(StepUpOut {
        step_up_until: None,
        redirect_url: Some(REDIRECT.to_owned()),
    });
    step(&f, json!({ "sso": true })).await;
    let lines = capture.0.lock().unwrap().clone();
    for line in &lines {
        assert!(!line.contains(PASSWORD), "password in a log line: {line}");
        assert!(!line.contains(CODE), "code in a log line: {line}");
        assert!(
            !line.contains("distinctive-redirect"),
            "redirect in a log line: {line}"
        );
    }
}

// ------------------------------------------------------------------------------------------
// Mounting and the audience.

#[test]
fn the_group_serves_the_one_operation_authenticated() {
    assert_eq!(step_up::SERVES, &["auth_step_up"]);
    let registration = step_up::registration();
    assert!(registration.public.is_none(), "step-up is never public");
    assert_eq!(registration.name, "step_up");
    assert_eq!((registration.required_ports)().len(), 1);
}

#[test]
fn a_group_without_its_port_is_not_mounted() {
    let mounted = routes::mount(&[step_up::registration()], &Ports::new(), &[]);
    assert!(mounted.report.mounted.is_empty());
    assert!(
        mounted
            .report
            .skipped
            .iter()
            .any(|(name, _)| *name == "step_up"),
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
        Some(fake) => services.with_port::<dyn StepUpPort>(fake),
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
    let (api, report) = router_with(services, &[step_up::registration()]);
    assert!(report.mounted.is_empty(), "{report:?}");
    let router = site_router(api);
    let reply = send(
        &router,
        Some("test-token"),
        Some(json!({ "password": PASSWORD }).to_string()),
    )
    .await;
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
    let fake = Arc::new(Fake {
        script: Mutex::new(Script::default()),
    });
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
    let body = || Some(json!({ "password": PASSWORD }).to_string());

    let consumer = send(&router, Some("tok-consumer"), body()).await;
    assert_eq!(consumer.status, 403, "{}", consumer.text);
    let anonymous = send(&router, None, body()).await;
    assert_eq!(anonymous.status, 401, "{}", anonymous.text);
    assert!(fake.calls().is_empty(), "neither reached the port");

    // Staff and partner are served.
    for token in ["tok-staff", "tok-partner"] {
        let reply = send(&router, Some(token), body()).await;
        assert_eq!(reply.status, 200, "{token}: {}", reply.text);
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
    let absent = send(&consumer_only, Some("tok-consumer"), body()).await;
    assert_eq!(absent.status, 404, "{}", absent.text);
}
