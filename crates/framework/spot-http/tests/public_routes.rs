//! Public route groups (WP-PRD-01.T49): the one explicit way a registered group is served
//! without a bearer. A group is authenticated unless it declares `PUBLIC`; a public group runs
//! behind its rate class, per client address, before any handler work, and never sees the
//! caller's `Authorization` header.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[allow(dead_code)]
mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Request, header};
use axum::routing::get;
use serde_json::Value as Json;
use spot_core::{ManualClock, SiteId};
use spot_http::api::PortKey;
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::ports::consumer_sessions::{
    COOKIE_NAME, ConsumerSessions, MemoryConsumerSessions,
};
use spot_http::api::router::routes::{
    Public, REGISTRY, RateClass, Registration, RouteState, SkipReason,
};
use spot_http::api::router::{ApiServices, router_with};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use support::fake::FakeApp;
use time::macros::datetime;
use tower::ServiceExt as _;

/// The build script's scanner, applied here to a test registration's source text.
#[path = "../build.rs"]
mod scanner;

const HOST: &str = "erp.acme.com";

// --- test groups ----------------------------------------------------------------------------

async fn open_handler(headers: HeaderMap) -> String {
    format!(
        "authorization={}",
        headers.contains_key(header::AUTHORIZATION)
    )
}

async fn closed_handler(State(_): State<RouteState>) -> &'static str {
    "closed"
}

fn no_ports() -> &'static [PortKey] {
    &[]
}
fn open_routes() -> Router<RouteState> {
    Router::new().route("/api/v1/fake/open", get(open_handler))
}
fn closed_routes() -> Router<RouteState> {
    Router::new().route("/api/v1/fake/closed", get(closed_handler))
}

const OPEN: Registration = Registration {
    name: "open",
    serves: &["fake_open"],
    required_ports: no_ports,
    routes: open_routes,
    public: Some(Public {
        rate_class: RateClass::Auth,
    }),
};
const CLOSED: Registration = Registration {
    name: "closed",
    serves: &["fake_closed"],
    required_ports: no_ports,
    routes: closed_routes,
    public: None,
};

// --- harness --------------------------------------------------------------------------------

struct Fixture {
    router: Router,
    clock: Arc<ManualClock>,
    report: spot_http::api::router::routes::MountReport,
}

fn services(clock: &Arc<ManualClock>) -> ApiServices {
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock.clone());
    ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store)
        .with_clock(clock.clone())
}

fn fixture(
    registrations: &[Registration],
    extra: impl FnOnce(ApiServices) -> ApiServices,
) -> Fixture {
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-05 08:00:00 UTC)));
    let (api, report) = router_with(extra(services(&clock)), registrations);
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(api)
    .build();
    Fixture {
        router,
        clock,
        report,
    }
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    raw: String,
    body: Json,
}

async fn send(
    f: &Fixture,
    method: &str,
    path: &str,
    from: &str,
    headers: &[(&str, String)],
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    for (name, value) in headers {
        builder = builder.header(*name, value.clone());
    }
    let mut request = builder.body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::new(from.parse().unwrap(), 40_000)));
    let response = f.router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    Reply {
        status,
        headers,
        body: serde_json::from_str(&raw).unwrap_or(Json::Null),
        raw,
    }
}

async fn get_open(f: &Fixture, from: &str) -> Reply {
    send(f, "GET", "/api/v1/fake/open", from, &[]).await
}

// --- the marker and the mounting ------------------------------------------------------------

#[tokio::test]
async fn a_request_with_no_bearer_reaches_a_public_group_and_not_an_authenticated_one() {
    let f = fixture(&[OPEN, CLOSED], |s| s);
    assert_eq!(f.report.mounted, vec!["open", "closed"]);
    assert_eq!(f.report.public, vec!["open"]);

    let open = get_open(&f, "203.0.113.1").await;
    assert_eq!(open.status, 200, "{}", open.raw);
    let closed = send(&f, "GET", "/api/v1/fake/closed", "203.0.113.1", &[]).await;
    assert_eq!(closed.status, 401, "{}", closed.raw);
    assert_eq!(closed.body["error"]["code"], "auth.required");
    // With a bearer the authenticated group is reached.
    let ok = send(
        &f,
        "GET",
        "/api/v1/fake/closed",
        "203.0.113.1",
        &[("authorization", "Bearer t".into())],
    )
    .await;
    assert_eq!(ok.status, 200);
}

#[tokio::test]
async fn a_public_handler_is_not_handed_the_callers_identity() {
    let f = fixture(&[OPEN], |s| s);
    let r = send(
        &f,
        "GET",
        "/api/v1/fake/open",
        "203.0.113.1",
        &[("authorization", "Bearer some-token".into())],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    assert_eq!(r.raw, "authorization=false", "the header is removed");
}

#[tokio::test]
async fn a_public_operation_is_not_served_twice() {
    const TWIN: Registration = Registration {
        name: "twin",
        serves: &["fake_open"],
        required_ports: no_ports,
        routes: closed_routes,
        public: None,
    };
    let f = fixture(&[OPEN, TWIN], |s| s);
    assert_eq!(f.report.mounted, vec!["open"]);
    assert_eq!(
        f.report.skipped,
        vec![("twin", SkipReason::DuplicateOperation("fake_open"))]
    );
    // And a public group cannot take over a hand-wired operation either.
    const TAKEOVER: Registration = Registration {
        name: "takeover",
        serves: &["auth_logout"],
        required_ports: no_ports,
        routes: open_routes,
        public: Some(Public {
            rate_class: RateClass::Auth,
        }),
    };
    let f = fixture(&[TAKEOVER], |s| s);
    assert!(f.report.mounted.is_empty());
}

#[tokio::test]
async fn a_public_group_with_a_port_absent_or_serving_nothing_is_not_mounted() {
    fn needs_sessions() -> &'static [PortKey] {
        static KEYS: [PortKey; 1] = [PortKey::of::<dyn ConsumerSessions>()];
        &KEYS
    }
    const NEEDY: Registration = Registration {
        name: "needy",
        serves: &["fake_open"],
        required_ports: needs_sessions,
        routes: open_routes,
        public: Some(Public {
            rate_class: RateClass::Auth,
        }),
    };
    const EMPTY: Registration = Registration {
        name: "empty",
        serves: &[],
        required_ports: no_ports,
        routes: open_routes,
        public: Some(Public {
            rate_class: RateClass::Auth,
        }),
    };
    let f = fixture(&[NEEDY, EMPTY], |s| s);
    assert!(f.report.mounted.is_empty() && f.report.public.is_empty());
    assert_eq!(get_open(&f, "203.0.113.1").await.status, 404);
}

// --- every real group that is not public is authenticated -----------------------------------

/// The (method, path) of every operation of the contract, by operationId.
fn contract_routes() -> std::collections::BTreeMap<String, (String, String)> {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    ))
    .unwrap();
    let root: Json = serde_norway::from_str(&text).unwrap();
    let mut out = std::collections::BTreeMap::new();
    for (path, item) in root["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            if let Some(id) = op["operationId"].as_str() {
                out.insert(id.to_owned(), (method.to_ascii_uppercase(), path.clone()));
            }
        }
    }
    out
}

/// A path with its `{parameters}` filled in, so it reaches the route.
fn concrete(path: &str) -> String {
    let mut out = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c == '{' {
            for d in chars.by_ref() {
                if d == '}' {
                    break;
                }
            }
            out.push('x');
        } else {
            out.push(c);
        }
    }
    out
}

#[tokio::test]
async fn every_real_group_that_is_not_public_answers_401_without_a_bearer() {
    use spot_http::api::ports::notices::{MemoryNoticeSource, NoticeSource};
    let routes = contract_routes();
    let f = fixture(REGISTRY, |s| {
        s.with_port::<dyn NoticeSource>(Arc::new(MemoryNoticeSource::new()))
            .with_port::<dyn ConsumerSessions>(Arc::new(MemoryConsumerSessions::new(
                Arc::new(ManualClock::new(datetime!(2026-11-05 08:00:00 UTC))),
                "seed",
                time::Duration::minutes(15),
            )))
    });
    let mut checked = 0;
    for group in REGISTRY.iter().filter(|g| g.public.is_none()) {
        // A group whose ports this test does not provide is not mounted at all (it answers as
        // an unserved operation); only a mounted group can opt out, so only those are walked.
        if !f.report.mounted.contains(&group.name) {
            continue;
        }
        for id in group.serves {
            let (method, path) = routes
                .get(*id)
                .unwrap_or_else(|| panic!("{id} is not in the contract"));
            let r = send(&f, method, &concrete(path), "203.0.113.1", &[]).await;
            assert_eq!(r.status, 401, "{} / {id}: {}", group.name, r.raw);
            assert_eq!(
                r.body["error"]["code"], "auth.required",
                "{} / {id}",
                group.name
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "at least the notice feed is walked");
    // The public groups are exactly the ones the contract marks `security: []`: here, the
    // renewal, which does not answer `auth.required`.
    for group in REGISTRY.iter().filter(|g| g.public.is_some()) {
        assert!(
            f.report.public.contains(&group.name),
            "{} mounts",
            group.name
        );
        for id in group.serves {
            let (method, path) = routes.get(*id).unwrap();
            let r = send(&f, method, &concrete(path), "198.51.100.77", &[]).await;
            assert_ne!(r.body["error"]["code"], "auth.required", "{id}: {}", r.raw);
        }
    }
}

// --- the rate class -------------------------------------------------------------------------

#[tokio::test]
async fn the_auth_rate_class_answers_429_with_retry_after_per_client_address() {
    let f = fixture(&[OPEN], |s| s);
    let mut first_limited = None;
    for n in 1..=200 {
        let r = get_open(&f, "203.0.113.1").await;
        if r.status == 429 {
            assert_eq!(r.body["error"]["code"], "api.rate_limited", "{}", r.raw);
            let wait: u64 = r.headers[header::RETRY_AFTER]
                .to_str()
                .unwrap()
                .parse()
                .unwrap();
            assert!((1..=61).contains(&wait), "Retry-After {wait}");
            first_limited = Some(n);
            break;
        }
        assert_eq!(r.status, 200);
    }
    let limited_at = first_limited.expect("the class limits a client address");
    assert!(limited_at > 1, "a first request is never limited");
    // Still limited, and another client address has its own counter.
    assert_eq!(get_open(&f, "203.0.113.1").await.status, 429);
    assert_eq!(get_open(&f, "203.0.113.2").await.status, 200);
    // Two addresses of the same IPv6 /64 are one client; another /64 is another.
    for _ in 0..limited_at {
        let _ = get_open(&f, "2001:db8:1:1::5").await;
    }
    assert_eq!(get_open(&f, "2001:db8:1:1::9").await.status, 429);
    assert_eq!(get_open(&f, "2001:db8:1:2::1").await.status, 200);
    // The window passes by the injected clock, not the wall clock.
    f.clock.advance(Duration::from_secs(61));
    assert_eq!(get_open(&f, "203.0.113.1").await.status, 200);
}

#[tokio::test]
async fn an_authenticated_group_is_not_behind_the_public_rate_limit() {
    let f = fixture(&[OPEN, CLOSED], |s| s);
    for _ in 0..100 {
        let r = send(
            &f,
            "GET",
            "/api/v1/fake/closed",
            "203.0.113.1",
            &[("authorization", "Bearer t".into())],
        )
        .await;
        assert_eq!(r.status, 200);
    }
}

// --- the real renewal through the registry --------------------------------------------------

fn renewal_fixture() -> (Fixture, Arc<MemoryConsumerSessions>) {
    let sessions = Arc::new(MemoryConsumerSessions::new(
        Arc::new(ManualClock::new(datetime!(2026-11-05 08:00:00 UTC))),
        "seed",
        time::Duration::minutes(15),
    ));
    let held = sessions.clone();
    let f = fixture(REGISTRY, move |s| s.with_port::<dyn ConsumerSessions>(held));
    (f, sessions)
}

#[tokio::test]
async fn the_renewal_is_served_without_a_bearer_through_the_registry() {
    let (f, sessions) = renewal_fixture();
    assert!(f.report.public.contains(&"consumer_renew"));
    let (cookie, _) = sessions.sign_in("acme");
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/consumer/renew",
        "203.0.113.1",
        &[("cookie", format!("{COOKIE_NAME}={cookie}"))],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    // No cookie: the one generic 401, never `auth.required`.
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/consumer/renew",
        "203.0.113.1",
        &[],
    )
    .await;
    assert_eq!(r.status, 401);
    assert_eq!(r.body["error"]["code"], "auth.session_expired");
    assert!(r.headers.get(header::SET_COOKIE).is_none());
}

#[tokio::test]
async fn the_rate_limit_runs_before_the_cookie_is_touched() {
    let (f, sessions) = renewal_fixture();
    let (cookie, _) = sessions.sign_in("acme");
    let bad = [("cookie", format!("{COOKIE_NAME}=not-a-real-cookie"))];
    let mut limited = false;
    for _ in 0..200 {
        let r = send(
            &f,
            "POST",
            "/api/v1/auth/consumer/renew",
            "203.0.113.9",
            &bad,
        )
        .await;
        if r.status == 429 {
            limited = true;
            break;
        }
        assert_eq!(r.status, 401);
    }
    assert!(limited);
    // A limited request is refused before the cookie is read or spent: the good cookie sent
    // now is answered 429 and is still good once the window has passed.
    let good = [("cookie", format!("{COOKIE_NAME}={cookie}"))];
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/consumer/renew",
        "203.0.113.9",
        &good,
    )
    .await;
    assert_eq!(r.status, 429, "{}", r.raw);
    assert!(r.headers.get(header::SET_COOKIE).is_none());
    f.clock.advance(Duration::from_secs(61));
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/consumer/renew",
        "203.0.113.9",
        &good,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
}

#[tokio::test]
async fn a_bearer_sent_to_the_renewal_is_ignored() {
    let (f, sessions) = renewal_fixture();
    let (cookie, _) = sessions.sign_in("acme");
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/consumer/renew",
        "203.0.113.1",
        &[
            ("cookie", format!("{COOKIE_NAME}={cookie}")),
            ("authorization", "Bearer stolen".into()),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
}

// --- a public group mounts only the paths of its operations (WP-PRD-01.T51) -----------------

async fn extra_handler() -> &'static str {
    "extra"
}

/// A public group that mounts its operation's path and one more. `fn routes` below is copied into
/// a source text for the build-time check; the name differs so this file can hold several.
fn leaky_routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/fake/open", get(open_handler))
        .route("/api/v1/fake/extra", get(extra_handler))
}

const LEAKY: Registration = Registration {
    name: "leaky",
    serves: &["fake_open"],
    required_ports: no_ports,
    routes: leaky_routes,
    public: Some(Public {
        rate_class: RateClass::Auth,
    }),
};

const FAKE_CONTRACT: &str =
    "paths:\n  /api/v1/fake/open:\n    get:\n      operationId: fake_open\n      security: []\n";

/// The source text of `leaky_routes`, as a route file would spell it.
fn leaky_source() -> String {
    let all = include_str!("public_routes.rs");
    let start = all.find("fn leaky_routes()").unwrap();
    let end = start + all[start..].find("\n}\n").unwrap() + 3;
    all[start..end].replace("fn leaky_routes", "pub fn routes")
}

#[tokio::test]
async fn a_public_group_with_an_extra_path_is_rejected_at_test_time() {
    // Axum's `Router` does not expose its routes, so the check reads the registration's source
    // text with the build script's function (the build applies the same function to every public
    // file). The registration really would serve the extra path with no bearer:
    let f = fixture(&[LEAKY], |s| s);
    assert_eq!(f.report.public, vec!["leaky"]);
    let leaked = send(&f, "GET", "/api/v1/fake/extra", "203.0.113.1", &[]).await;
    assert_eq!(leaked.status, 200, "{}", leaked.raw);
    // ... which is why the check refuses it:
    let serves: Vec<String> = LEAKY.serves.iter().map(|s| (*s).to_owned()).collect();
    let err = scanner::check_public_routes(
        "leaky.rs",
        &leaky_source(),
        "fake.yaml",
        FAKE_CONTRACT,
        &serves,
    )
    .unwrap_err();
    assert!(
        err.contains("leaky.rs") && err.contains("/api/v1/fake/extra"),
        "{err}"
    );
    // The same group without the extra path passes.
    let clean = leaky_source().replace(".route(\"/api/v1/fake/extra\", get(extra_handler))", "");
    assert_eq!(
        scanner::check_public_routes("leaky.rs", &clean, "fake.yaml", FAKE_CONTRACT, &serves),
        Ok(())
    );
}

#[tokio::test]
async fn the_same_extra_path_in_an_authenticated_group_answers_401_without_a_bearer() {
    const GUARDED: Registration = Registration {
        name: "guarded",
        serves: &["fake_closed"],
        required_ports: no_ports,
        routes: leaky_routes,
        public: None,
    };
    let f = fixture(&[GUARDED], |s| s);
    assert!(f.report.public.is_empty());
    for path in ["/api/v1/fake/extra", "/api/v1/fake/open"] {
        let r = send(&f, "GET", path, "203.0.113.1", &[]).await;
        assert_eq!(r.status, 401, "{path}: {}", r.raw);
        assert_eq!(r.body["error"]["code"], "auth.required");
    }
}
