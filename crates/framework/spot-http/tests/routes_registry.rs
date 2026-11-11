//! The route registry: a served operation group mounts only when its ports are present,
//! never half wired, never twice, and an unmounted group answers as an unserved operation does.
//! Also the build script's scanner, tested without a build.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[allow(dead_code)]
mod support;

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use spot_core::ManualClock;
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::router::routes::{Registration, RouteState, SkipReason};
use spot_http::api::router::{ApiServices, router_with};
use spot_http::api::{PortKey, Ports};
use support::fake::FakeApp;
use tower::ServiceExt as _;

/// The build script's scanner, tested without a build.
#[path = "../build.rs"]
mod scanner;

// --- fake ports -----------------------------------------------------------------------------

trait Greeter: Send + Sync {
    fn hello(&self) -> &'static str;
}
struct English;
impl Greeter for English {
    fn hello(&self) -> &'static str {
        "hello"
    }
}

trait Audit: Send + Sync {}
struct NoAudit;
impl Audit for NoAudit {}

trait Other: Send + Sync {}
struct Unrelated;
impl Other for Unrelated {}

// --- fake route groups ----------------------------------------------------------------------

async fn greet(State(state): State<RouteState>) -> (StatusCode, String) {
    match state.port::<dyn Greeter>() {
        Some(g) => (StatusCode::OK, g.hello().to_owned()),
        None => (StatusCode::INTERNAL_SERVER_ERROR, "no port".to_owned()),
    }
}

/// Reports whether the handler can reach a port it did not declare.
async fn peek(State(state): State<RouteState>) -> String {
    format!(
        "greeter={} other={}",
        state.port::<dyn Greeter>().is_some(),
        state.port::<dyn Other>().is_some()
    )
}

fn greeter_routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/fake/greet", get(greet))
        .route("/api/v1/fake/peek", get(peek))
}
fn greeter_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn Greeter>()];
    &KEYS
}
fn guarded_ports() -> &'static [PortKey] {
    // A write group needs its audit port too: without it, nothing mounts.
    static KEYS: [PortKey; 2] = [PortKey::of::<dyn Greeter>(), PortKey::of::<dyn Audit>()];
    &KEYS
}
fn other_routes() -> Router<RouteState> {
    Router::new().route("/api/v1/fake/other", get(greet))
}

const fn greeter(serves: &'static [&'static str]) -> Registration {
    Registration {
        name: "greeter",
        serves,
        required_ports: greeter_ports,
        routes: greeter_routes,
        public: None,
    }
}

// --- harness --------------------------------------------------------------------------------

fn services(ports: impl FnOnce(ApiServices) -> ApiServices) -> ApiServices {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    ports(ApiServices::new(
        fake.clone(),
        fake.clone(),
        fake.clone(),
        fake,
        store,
    ))
}

async fn get_path(router: &Router, path: &str, bearer: bool) -> (StatusCode, String) {
    let mut request = Request::builder().method("GET").uri(path);
    if bearer {
        request = request.header("authorization", "Bearer test-token");
    }
    let response = router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// --- mounted --------------------------------------------------------------------------------

#[tokio::test]
async fn a_group_with_its_port_present_is_mounted_behind_authentication() {
    let (router, report) = router_with(
        services(|s| s.with_port::<dyn Greeter>(Arc::new(English))),
        &[greeter(&["fake_greet", "fake_peek"])],
    );
    assert_eq!(report.mounted, vec!["greeter"]);
    assert!(report.skipped.is_empty());
    assert!(report.served.contains("fake_greet"));

    assert_eq!(
        get_path(&router, "/api/v1/fake/greet", true).await,
        (StatusCode::OK, "hello".to_owned())
    );
    // The authentication layer wraps every registered group: no bearer, no handler.
    let (status, body) = get_path(&router, "/api/v1/fake/greet", false).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body.contains("auth.required"), "{body}");
}

#[tokio::test]
async fn a_handler_reaches_only_the_ports_its_group_declared() {
    let (router, _) = router_with(
        services(|s| {
            s.with_port::<dyn Greeter>(Arc::new(English))
                .with_port::<dyn Other>(Arc::new(Unrelated))
        }),
        &[greeter(&["fake_greet", "fake_peek"])],
    );
    assert_eq!(
        get_path(&router, "/api/v1/fake/peek", true).await.1,
        "greeter=true other=false"
    );
}

// --- port absent ----------------------------------------------------------------------------

#[tokio::test]
async fn a_group_with_its_port_absent_is_not_mounted_and_answers_as_unserved() {
    let (router, report) = router_with(services(|s| s), &[greeter(&["fake_greet"])]);
    assert!(report.mounted.is_empty());
    assert_eq!(report.served.len(), 0);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].0, "greeter");
    match &report.skipped[0].1 {
        SkipReason::MissingPort(name) => assert!(name.contains("Greeter"), "{name}"),
        other => panic!("{other:?}"),
    }

    // Exactly what an operation nobody serves answers: not a success, not a panic, not a 500.
    let unserved = get_path(&router, "/api/v1/never/served", true).await;
    let absent = get_path(&router, "/api/v1/fake/greet", true).await;
    assert_eq!(absent, unserved);
    assert_eq!(absent.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn removing_a_port_turns_a_served_operation_into_an_unserved_one() {
    let with = services(|s| s.with_port::<dyn Greeter>(Arc::new(English)));
    let (router, _) = router_with(with, &[greeter(&["fake_greet"])]);
    assert_eq!(
        get_path(&router, "/api/v1/fake/greet", true).await.0,
        StatusCode::OK
    );
    let (router, _) = router_with(services(|s| s), &[greeter(&["fake_greet"])]);
    assert_eq!(
        get_path(&router, "/api/v1/fake/greet", true).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_half_wired_group_is_not_mounted_at_all() {
    let write_group = Registration {
        name: "guarded",
        serves: &["fake_greet"],
        required_ports: guarded_ports,
        routes: greeter_routes,
        public: None,
    };
    // The greeter port is there, the audit port is not: nothing of the group is reachable.
    let (router, report) = router_with(
        services(|s| s.with_port::<dyn Greeter>(Arc::new(English))),
        &[write_group],
    );
    assert!(report.mounted.is_empty());
    assert!(matches!(
        report.skipped[0].1,
        SkipReason::MissingPort(name) if name.contains("Audit")
    ));
    assert_eq!(
        get_path(&router, "/api/v1/fake/greet", true).await.0,
        StatusCode::NOT_FOUND
    );
    // With both ports it mounts.
    let (router, report) = router_with(
        services(|s| {
            s.with_port::<dyn Greeter>(Arc::new(English))
                .with_port::<dyn Audit>(Arc::new(NoAudit))
        }),
        &[write_group],
    );
    assert_eq!(report.mounted, vec!["guarded"]);
    assert_eq!(
        get_path(&router, "/api/v1/fake/greet", true).await.0,
        StatusCode::OK
    );
}

// --- duplicates -----------------------------------------------------------------------------

#[tokio::test]
async fn an_operation_served_twice_mounts_only_the_first_group() {
    let second = Registration {
        name: "second",
        serves: &["fake_greet", "fake_other"],
        required_ports: greeter_ports,
        routes: other_routes,
        public: None,
    };
    let (router, report) = router_with(
        services(|s| s.with_port::<dyn Greeter>(Arc::new(English))),
        &[greeter(&["fake_greet"]), second],
    );
    assert_eq!(report.mounted, vec!["greeter"]);
    assert_eq!(
        report.skipped,
        vec![("second", SkipReason::DuplicateOperation("fake_greet"))]
    );
    assert_eq!(
        get_path(&router, "/api/v1/fake/greet", true).await.0,
        StatusCode::OK
    );
    // Nothing of the rejected group is reachable, not even its other operation.
    assert_eq!(
        get_path(&router, "/api/v1/fake/other", true).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_group_cannot_take_over_a_hand_wired_operation() {
    let (_, report) = router_with(
        services(|s| s.with_port::<dyn Greeter>(Arc::new(English))),
        &[greeter(&["meta_get"])],
    );
    assert!(report.mounted.is_empty());
    assert_eq!(
        report.skipped,
        vec![("greeter", SkipReason::DuplicateOperation("meta_get"))]
    );
}

#[tokio::test]
async fn a_group_that_serves_nothing_is_not_mounted() {
    let (_, report) = router_with(
        services(|s| s.with_port::<dyn Greeter>(Arc::new(English))),
        &[greeter(&[])],
    );
    assert_eq!(report.skipped, vec![("greeter", SkipReason::ServesNothing)]);
}

// --- ports map ------------------------------------------------------------------------------

#[test]
fn the_ports_map_is_typed_and_the_last_provider_wins() {
    let mut ports = Ports::new();
    assert!(ports.get::<dyn Greeter>().is_none());
    ports.provide::<dyn Greeter>(Arc::new(English));
    assert_eq!(ports.get::<dyn Greeter>().unwrap().hello(), "hello");
    assert!(ports.has(&PortKey::of::<dyn Greeter>()));
    assert!(!ports.has(&PortKey::of::<dyn Audit>()));
    assert_eq!(
        ports.first_missing(&[PortKey::of::<dyn Greeter>(), PortKey::of::<dyn Audit>()]),
        Some(PortKey::of::<dyn Audit>())
    );
    assert!(ports.subset(&[PortKey::of::<dyn Audit>()]).is_err());
    assert_eq!(ports.names().len(), 1);
}

// --- the build script's scanner -------------------------------------------------------------

#[test]
fn module_names_are_checked() {
    assert!(scanner::valid_module_name("functions"));
    assert!(scanner::valid_module_name("event_stream2"));
    assert!(!scanner::valid_module_name("2fast"));
    assert!(!scanner::valid_module_name("Reports"));
    assert!(!scanner::valid_module_name("type"));
    assert!(!scanner::valid_module_name(""));
}

#[test]
fn a_route_file_declares_all_three_items_or_none() {
    use scanner::{Kind, inspect};
    let full = "pub const SERVES: &[&str] = &[\"x\"];\npub fn required_ports() -> &'static [PortKey] { &[] }\npub fn routes() -> Router<RouteState> { Router::new() }\n";
    assert_eq!(inspect(Kind::Routes, "a.rs", full), Ok(true));
    assert_eq!(inspect(Kind::Routes, "a.rs", "fn helper() {}\n"), Ok(false));
    let partial = "pub const SERVES: &[&str] = &[];\n";
    let err = inspect(Kind::Routes, "a.rs", partial).unwrap_err();
    assert!(
        err.contains("required_ports") && err.contains("routes"),
        "{err}"
    );
    // An indented declaration (inside an impl or a test module) does not count.
    assert_eq!(
        inspect(Kind::Routes, "a.rs", "    pub fn routes() {}\n"),
        Ok(false)
    );
}

#[test]
fn a_port_file_registers_by_its_key() {
    use scanner::{Kind, inspect};
    assert_eq!(
        inspect(
            Kind::Ports,
            "p.rs",
            "pub const KEY: PortKey = PortKey::of::<dyn X>();\n"
        ),
        Ok(true)
    );
    assert_eq!(inspect(Kind::Ports, "p.rs", "pub trait X {}\n"), Ok(false));
}

#[test]
fn the_generated_source_lists_every_module_and_the_registered_ones() {
    use scanner::{Kind, Module, generate};
    let modules = vec![
        Module {
            name: "alpha".into(),
            path: "src/api/router/routes/alpha.rs".into(),
            registers: true,
            public: false,
            serves: vec!["a_op".into()],
        },
        Module {
            name: "helper".into(),
            path: "src/api/router/routes/helper.rs".into(),
            registers: false,
            public: false,
            serves: Vec::new(),
        },
    ];
    let out = generate("/m", Kind::Routes, &modules);
    assert!(out.contains("pub mod alpha;") && out.contains("pub mod helper;"));
    assert!(out.contains("name: \"alpha\", serves: alpha::SERVES"));
    assert!(!out.contains("name: \"helper\""));
    let ports = generate("/m", Kind::Ports, &modules);
    assert!(ports.contains("alpha::KEY,") && !ports.contains("helper::KEY"));
}

#[test]
fn a_missing_directory_is_an_empty_scan() {
    let dir = std::path::Path::new("/nonexistent-spot-http-dir");
    assert_eq!(scanner::scan(dir, scanner::Kind::Routes), Ok(Vec::new()));
}

// --- public groups: the marker and the contract check (WP-PRD-01.T49) -----------------------

const CONTRACT: &str = "\
openapi: 3.1.0
security:
  - bearer: []
paths:
  /api/v1/open:
    post:
      operationId: open_op
      description: |
        security: []
      security: []
  /api/v1/closed:
    get:
      operationId: closed_op
      security:
        - bearer: []
    post:
      operationId: inherited_op
components:
  x:
    get:
      operationId: not_a_path_op
      security: []
";

#[test]
fn the_contract_scan_reads_only_an_exact_empty_security_as_public() {
    let ops = scanner::contract_operations(CONTRACT);
    assert_eq!(
        ops,
        vec![
            ("open_op".to_owned(), true),
            ("closed_op".to_owned(), false),
            ("inherited_op".to_owned(), false),
        ]
    );
}

fn facts<'a>(name: &'a str, public: bool, serves: &'a [String]) -> scanner::GroupFacts<'a> {
    scanner::GroupFacts {
        name,
        public,
        serves,
    }
}

#[test]
fn a_public_group_may_serve_only_security_free_operations() {
    let ok = vec!["open_op".to_owned()];
    assert_eq!(
        scanner::check_security("c.yaml", CONTRACT, &[facts("g", true, &ok)]),
        Ok(())
    );
    // A secured operation in a public group stops the build and names file and operation.
    for id in ["closed_op", "inherited_op"] {
        let bad = vec!["open_op".to_owned(), id.to_owned()];
        let err =
            scanner::check_security("c.yaml", CONTRACT, &[facts("g", true, &bad)]).unwrap_err();
        assert!(
            err.contains("c.yaml") && err.contains(id) && err.contains("g"),
            "{err}"
        );
    }
    // An operation the check cannot see is refused too (fail closed).
    let unknown = vec!["ghost".to_owned()];
    let err =
        scanner::check_security("c.yaml", CONTRACT, &[facts("g", true, &unknown)]).unwrap_err();
    assert!(err.contains("ghost"), "{err}");
}

#[test]
fn an_authenticated_group_may_not_serve_a_security_free_operation() {
    let secured = vec!["closed_op".to_owned(), "inherited_op".to_owned()];
    assert_eq!(
        scanner::check_security("c.yaml", CONTRACT, &[facts("g", false, &secured)]),
        Ok(())
    );
    let open = vec!["open_op".to_owned()];
    let err = scanner::check_security("c.yaml", CONTRACT, &[facts("g", false, &open)]).unwrap_err();
    assert!(err.contains("open_op") && err.contains("PUBLIC"), "{err}");
}

#[test]
fn the_real_registry_agrees_with_the_real_contract() {
    // The build already runs this check; running it here pins it in the test run too.
    let yaml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    ))
    .unwrap();
    let ops = scanner::contract_operations(&yaml);
    assert!(ops.len() > 20, "the scan finds the contract's operations");
    for r in spot_http::api::router::routes::REGISTRY {
        for id in r.serves {
            let public = ops.iter().find(|(op, _)| op == id).map(|(_, p)| *p);
            assert_eq!(
                public,
                Some(r.public.is_some()),
                "{}: {id} must be public exactly when the contract says security: []",
                r.name
            );
        }
    }
}

#[test]
fn serves_must_be_a_literal_list() {
    let lit = "pub const SERVES: &[&str] = &[\n    \"a_op\",\n    \"b_op\",\n];\n";
    assert_eq!(
        scanner::serves_of("f.rs", lit),
        Ok(vec!["a_op".into(), "b_op".into()])
    );
    assert_eq!(
        scanner::serves_of("f.rs", "pub const SERVES: &[&str] = &[];\n"),
        Ok(vec![])
    );
    assert!(scanner::serves_of("f.rs", "pub const SERVES: &[&str] = OTHER;\n").is_err());
    assert!(scanner::serves_of("f.rs", "pub const SERVES: &[&str] = &[NAME];\n").is_err());
    // A mention in a comment is not the declaration: the column-0 one is read.
    let commented = "// pub const SERVES: &[&str] = &[\"fake_op\"];\npub const SERVES: &[&str] = &[\"real_op\"];\n";
    assert_eq!(
        scanner::serves_of("f.rs", commented),
        Ok(vec!["real_op".into()])
    );
}

#[test]
fn the_contract_scan_agrees_with_a_real_yaml_parse_of_the_contract() {
    // The scan is a plain text read; this pins it to what a YAML parser says, so a contract
    // formatting change that the scan misreads fails the test run (the build also fails closed).
    let yaml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    ))
    .unwrap();
    let root: serde_json::Value = serde_norway::from_str(&yaml).unwrap();
    let mut parsed = std::collections::BTreeMap::new();
    for (_, item) in root["paths"].as_object().unwrap() {
        for (method, op) in item.as_object().unwrap() {
            if !matches!(method.as_str(), "get" | "put" | "post" | "patch" | "delete") {
                continue;
            }
            let id = op["operationId"].as_str().unwrap().to_owned();
            let public = op["security"].as_array().is_some_and(Vec::is_empty);
            parsed.insert(id, public);
        }
    }
    let scanned: std::collections::BTreeMap<String, bool> =
        scanner::contract_operations(&yaml).into_iter().collect();
    assert_eq!(scanned, parsed);
}

#[test]
fn contract_scan_variants_never_read_an_unclear_operation_as_public() {
    // A trailing comment, a quoted id, flow style: each is either read correctly or not found
    // (and a public group serving a not-found operation fails the build), never public by error.
    let yaml = "paths:\n  /a:\n    post:\n      operationId: a_op\n      security: [] # none\n  /b:\n    post:\n      operationId: \"b_op\"\n      security: []\n  /c:\n    post: {operationId: c_op, security: []}\n  /d:\r\n    post:\r\n      operationId: d_op\r\n      security: []\r\n";
    let ops = scanner::contract_operations(yaml);
    assert!(!ops.contains(&("a_op".to_owned(), true)));
    assert!(!ops.iter().any(|(id, _)| id == "c_op"));
    assert!(ops.contains(&("d_op".to_owned(), true)));
}

#[test]
fn the_public_marker_needs_the_other_three_items() {
    use scanner::{Kind, inspect, is_public};
    let marker = "pub const PUBLIC: Public = Public { rate_class: RateClass::Auth };\n";
    assert!(is_public(marker));
    assert!(!is_public("    pub const PUBLIC: Public = x;\n"));
    let err = inspect(Kind::Routes, "a.rs", marker).unwrap_err();
    assert!(err.contains("PUBLIC"), "{err}");
    let full = format!(
        "{marker}pub const SERVES: &[&str] = &[\"x\"];\npub fn required_ports() -> &'static [PortKey] {{ &[] }}\npub fn routes() -> Router<RouteState> {{ Router::new() }}\n"
    );
    assert_eq!(inspect(Kind::Routes, "a.rs", &full), Ok(true));
}

#[test]
fn the_generated_registration_carries_the_public_marker() {
    use scanner::{Kind, Module, generate};
    let modules = vec![
        Module {
            name: "open".into(),
            path: "src/api/router/routes/open.rs".into(),
            registers: true,
            public: true,
            serves: vec!["o".into()],
        },
        Module {
            name: "closed".into(),
            path: "src/api/router/routes/closed.rs".into(),
            registers: true,
            public: false,
            serves: vec!["c".into()],
        },
    ];
    let out = generate("/m", Kind::Routes, &modules);
    assert!(out.contains("routes: open::routes, public: Some(open::PUBLIC) }"));
    assert!(out.contains("routes: closed::routes, public: None }"));
}

// --- the paths a public file mounts (WP-PRD-01.T51) -----------------------------------------

const PATH_CONTRACT: &str = "\
openapi: 3.1.0
paths:
  /api/v1/pub/{id}/renew:
    post:
      operationId: pub_renew
      security: []
    get:
      operationId: pub_status
      security: []
  /api/v1/pub/other:
    put:
      operationId: pub_other
      security: []
  /api/v1/secret:
    get:
      operationId: secret_op
";

/// A public file with `routes` as given, otherwise as the real one is written.
fn public_file(routes_body: &str) -> String {
    format!(
        "use axum::Router;\nuse axum::routing::{{get, post}};\n\n\
         pub const SERVES: &[&str] = &[\"pub_renew\"];\n\n\
         pub fn routes() -> Router<RouteState> {{\n    {routes_body}\n}}\n"
    )
}

fn check(source: &str, serves: &[&str]) -> Result<(), String> {
    let serves: Vec<String> = serves.iter().map(|s| (*s).to_owned()).collect();
    scanner::check_public_routes("pub.rs", source, "c.yaml", PATH_CONTRACT, &serves)
}

const RENEW_CHAIN: &str = "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h))";

#[test]
fn the_contract_endpoints_carry_method_and_path() {
    let eps = scanner::contract_endpoints(PATH_CONTRACT);
    let got: Vec<(&str, &str, &str)> = eps
        .iter()
        .map(|e| (e.id.as_str(), e.method.as_str(), e.path.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("pub_renew", "POST", "/api/v1/pub/{id}/renew"),
            ("pub_status", "GET", "/api/v1/pub/{id}/renew"),
            ("pub_other", "PUT", "/api/v1/pub/other"),
            ("secret_op", "GET", "/api/v1/secret"),
        ]
    );
}

#[test]
fn a_public_file_with_exactly_its_operations_path_passes() {
    assert_eq!(check(&public_file(RENEW_CHAIN), &["pub_renew"]), Ok(()));
    // Axum's two template spellings and layers are all the same route.
    for chain in [
        "Router::new().route(\"/api/v1/pub/:id/renew\", post(h))",
        "Router::new().route(\"/api/v1/pub/{x}/renew\", post(h))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).layer(l).route_layer(m).with_state(s)",
    ] {
        assert_eq!(
            check(&public_file(chain), &["pub_renew"]),
            Ok(()),
            "{chain}"
        );
    }
    // A method chain mounts several operations of one path.
    let both = public_file("Router::new().route(\"/api/v1/pub/{id}/renew\", post(h).get(g))");
    assert_eq!(check(&both, &["pub_renew", "pub_status"]), Ok(()));
}

#[test]
fn an_extra_path_a_method_mismatch_or_a_missing_route_fails_naming_the_path() {
    let extra = public_file(
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).route(\"/api/v1/admin/users\", get(a))",
    );
    let err = check(&extra, &["pub_renew"]).unwrap_err();
    assert!(
        err.contains("pub.rs") && err.contains("/api/v1/admin/users") && err.contains("GET"),
        "{err}"
    );
    // Same path, another method.
    let wrong = public_file("Router::new().route(\"/api/v1/pub/{id}/renew\", get(h))");
    let err = check(&wrong, &["pub_renew"]).unwrap_err();
    assert!(err.contains("GET") && err.contains("renew"), "{err}");
    // Declared operation not mounted.
    let missing = public_file("Router::new()");
    let err = check(&missing, &["pub_renew"]).unwrap_err();
    assert!(
        err.contains("pub_renew") && err.contains("not mounted"),
        "{err}"
    );
    // An extra method on the right path is also extra.
    let more = public_file("Router::new().route(\"/api/v1/pub/{id}/renew\", post(h).get(g))");
    let err = check(&more, &["pub_renew"]).unwrap_err();
    assert!(err.contains("GET"), "{err}");
    // A different path shape is not the contract's.
    let other = public_file("Router::new().route(\"/api/v1/pub/renew\", post(h))");
    assert!(check(&other, &["pub_renew"]).is_err());
    // Serving an operation that is not in the contract.
    let err = check(&public_file(RENEW_CHAIN), &["ghost"]).unwrap_err();
    assert!(err.contains("ghost"), "{err}");
    // The same route twice.
    let twice = public_file(
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).route(\"/api/v1/pub/{id}/renew\", post(h))",
    );
    assert!(check(&twice, &["pub_renew"]).is_err());
}

#[test]
fn constructs_the_scanner_cannot_read_fail_closed() {
    let cases = [
        // Other path-adding calls.
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).nest(\"/x\", r)",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).merge(r)",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).fallback(h)",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).fallback_service(s)",
        "Router::new().route_service(\"/api/v1/pub/{id}/renew\", s)",
        "Router::new().nest_service(\"/api\", s)",
        // Computed or non-literal paths.
        "Router::new().route(&format!(\"/api/{}\", v), post(h))",
        "Router::new().route(concat!(\"/api\", \"/v1\"), post(h))",
        "Router::new().route(PATH, post(h))",
        "Router::new().route(\"/api/v1/\".to_owned() + \"x\", post(h))",
        // Method expressions other than plain method calls.
        "Router::new().route(\"/api/v1/pub/{id}/renew\", on(MethodFilter::all(), h))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", any(h))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", MethodRouter::new())",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h).fallback(h))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h).on(f, h))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h).post(h))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h).layer(l))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", handler)",
        // Not a single chain from Router::new().
        "extra_router().route(\"/api/v1/pub/{id}/renew\", post(h))",
        "let r = Router::new(); r.route(\"/api/v1/pub/{id}/renew\", post(h))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h));",
        "Router::default().route(\"/api/v1/pub/{id}/renew\", post(h))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).clone()",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).layer(Router::new())",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).layer(x.route(1))",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).layer(m!())",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).with_state(m!{})",
        // A macro inside routes().
        "routes_macro!()",
        "Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)).route(\"/x\", get(format!(\"a\")))",
    ];
    for body in cases {
        assert!(
            check(&public_file(body), &["pub_renew"]).is_err(),
            "must fail closed: {body}"
        );
    }
    // A routes() with another signature, a missing one, or two.
    for src in [
        "pub fn routes() -> Router { Router::new() }",
        "pub fn routes() -> axum::Router<RouteState> { Router::new() }",
        "pub fn routes(x: u8) -> Router<RouteState> { Router::new() }",
        "pub fn nothing() {}",
        "pub fn routes() -> Router<RouteState> { Router::new() }\nmod m { pub fn routes() -> Router<RouteState> { Router::new() } }",
        "pub fn routes() -> Router<RouteState> { Router::new()",
    ] {
        assert!(check(src, &[]).is_err(), "must fail closed: {src}");
    }
}

#[test]
fn a_helper_a_macro_a_module_or_an_include_hides_nothing() {
    let route = "\"/api/v1/pub/{id}/renew\"";
    // A helper that takes and returns a Router.
    let helper = format!(
        "use axum::Router;\nuse axum::routing::post;\n\
         fn add(r: Router<RouteState>) -> Router<RouteState> {{ r.route(\"/api/v1/admin\", post(h)) }}\n\
         pub fn routes() -> Router<RouteState> {{ Router::new().route({route}, post(h)) }}\n"
    );
    let err = check(&helper, &["pub_renew"]).unwrap_err();
    assert!(
        err.contains("Router") && err.contains("outside `routes()`"),
        "{err}"
    );
    // A helper that returns a Router through an alias.
    let alias = format!(
        "use axum::Router;\ntype R = Router<RouteState>;\n\
         pub fn routes() -> Router<RouteState> {{ Router::new().route({route}, post(h)) }}\n"
    );
    assert!(check(&alias, &["pub_renew"]).is_err());
    // A macro_rules! that expands to .route.
    let mac = format!(
        "macro_rules! r {{ ($r:expr) => {{ $r.route(\"/api/v1/admin\", post(h)) }}; }}\n\
         pub fn routes() -> Router<RouteState> {{ Router::new().route({route}, post(h)) }}\n"
    );
    let err = check(&mac, &["pub_renew"]).unwrap_err();
    assert!(err.contains("macro"), "{err}");
    // A module, an include, an item-level foreign macro.
    for extra in [
        "mod hidden { }",
        "include!(\"other.rs\");",
        "const S: &str = include_str!(\"x\");",
        "other_crate::more_routes!();",
        "impl X { other_crate::more!(); }",
    ] {
        let src = format!(
            "{extra}\npub fn routes() -> Router<RouteState> {{ Router::new().route({route}, post(h)) }}\n"
        );
        assert!(
            check(&src, &["pub_renew"]).is_err(),
            "must fail closed: {extra}"
        );
    }
    // Renaming imports.
    for import in [
        "use axum::routing::any as post;",
        "use other::post;",
        "use axum::routing::MethodRouter;",
        "use other::format;",
    ] {
        let src = format!(
            "{import}\npub fn routes() -> Router<RouteState> {{ Router::new().route({route}, post(h)) }}\n"
        );
        assert!(
            check(&src, &["pub_renew"]).is_err(),
            "must fail closed: {import}"
        );
    }
    // A function named like a method.
    let src = format!(
        "fn post(h: u8) -> u8 {{ h }}\npub fn routes() -> Router<RouteState> {{ Router::new().route({route}, post(h)) }}\n"
    );
    assert!(check(&src, &["pub_renew"]).is_err());
}

#[test]
fn comments_and_strings_neither_count_nor_hide_a_route() {
    let route = "\"/api/v1/pub/{id}/renew\"";
    // A `.route(` and a `.nest(` in comments and strings are ignored.
    let src = format!(
        "// .route(\"/api/v1/admin\", get(a)) and macro_rules! mod\n\
         /* .nest(\"/x\", r) /* nested .merge(r) */ still a comment */\n\
         /// .route(\"/doc\", get(a))\n\
         const S: &str = \".route(\\\"/api/v1/admin\\\", get(a)) .merge(\";\n\
         const R: &str = r#\".route(\"/raw\", get(a)) \"quoted\" \"#;\n\
         const C: char = '\"';\n\
         pub fn routes() -> Router<RouteState> {{\n    // .fallback(h)\n    Router::new().route({route}, post(h)) /* .nest(1) */\n}}\n\
         fn f<'a>(x: &'a str) -> &'a str {{ let _ = b'\"'; x }}\n"
    );
    assert_eq!(check(&src, &["pub_renew"]), Ok(()));
    // A route behind a comment-looking string is still seen.
    let sneaky = format!(
        "pub fn routes() -> Router<RouteState> {{ Router::new().route({route}, post(h)).route(\"/api/v1/admin\", get(a)) /* \" */ }}\n"
    );
    assert!(check(&sneaky, &["pub_renew"]).is_err());
    // A string that closes a comment early does not hide the next call.
    let hidden = format!(
        "pub fn routes() -> Router<RouteState> {{ Router::new().route({route}, post(h)) /* \"*/ .route(\"/api/v1/admin\", get(a)) /* \" */ }}\n"
    );
    assert!(check(&hidden, &["pub_renew"]).is_err());
    // Unterminated comments and strings are errors, not silence.
    for bad in ["/* never closed", "const S: &str = \"never closed;"] {
        assert!(scanner::mounted_routes("f.rs", bad).is_err());
    }
}

#[test]
fn two_public_files_are_each_checked_against_their_own_serves() {
    let a = public_file(RENEW_CHAIN);
    let b = public_file("Router::new().route(\"/api/v1/pub/other\", put(h))");
    assert_eq!(check(&a, &["pub_renew"]), Ok(()));
    assert_eq!(check(&b, &["pub_other"]), Ok(()));
    // Each file's pair is wrong for the other's SERVES.
    assert!(check(&a, &["pub_other"]).is_err());
    assert!(check(&b, &["pub_renew"]).is_err());
}

#[test]
fn the_real_public_files_mount_exactly_their_contract_routes() {
    let yaml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    ))
    .unwrap();
    let mut checked = 0;
    for r in spot_http::api::router::routes::REGISTRY {
        if r.public.is_none() {
            continue;
        }
        let path = format!(
            "{}/src/api/router/routes/{}.rs",
            env!("CARGO_MANIFEST_DIR"),
            r.name
        );
        let source = std::fs::read_to_string(&path).unwrap();
        let serves: Vec<String> = r.serves.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            scanner::check_public_routes(r.name, &source, "openapi.yaml", &yaml, &serves),
            Ok(()),
            "{}",
            r.name
        );
        checked += 1;
    }
    assert!(checked >= 1, "the real registry has a public file");
    // The real renewal is the pair the contract names.
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/api/router/routes/consumer_renew.rs"
    ))
    .unwrap();
    assert_eq!(
        scanner::mounted_routes("consumer_renew.rs", &source),
        Ok(vec![(
            "POST".to_owned(),
            "/api/v1/auth/consumer/renew".to_owned()
        )])
    );
}

/// A clean `routes()` for the PATH_CONTRACT operation `pub_renew`, as the real files write it.
const CLEAN_ROUTES: &str = "pub fn routes() -> Router<RouteState> { Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)) }\n";
const CLEAN_USES: &str = "use axum::Router;\nuse axum::routing::post;\n";

fn refused(label: &str, src: &str) {
    let err = check(src, &["pub_renew"]).expect_err(&format!("must refuse: {label}"));
    assert!(!err.is_empty(), "{label}");
}

#[test]
fn bypass_decoy_routes_function_and_reexport_is_refused() {
    let decoy = "#[cfg(any())]\npub fn routes() -> Router<RouteState> { Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)) }\npub use super::admin::routes;\n";
    refused(
        "cfg(any()) decoy plus re-export",
        &format!("{CLEAN_USES}{decoy}"),
    );
    let nested = "const _: () = { pub fn routes() -> Router<RouteState> { Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)) } };\npub use super::admin::routes;\n";
    refused(
        "decoy nested in a const block",
        &format!("{CLEAN_USES}{nested}"),
    );
    let comment = "/*\npub fn routes() -> Router<RouteState> { Router::new().route(\"/api/v1/pub/{id}/renew\", post(h)) }\n*/\npub use super::admin::routes;\n";
    refused(
        "decoy inside a block comment",
        &format!("{CLEAN_USES}{comment}"),
    );
    for attr in [
        "cfg(any())",
        "cfg_attr(test, allow(dead_code))",
        "path = \"x.rs\"",
        "macro_use",
    ] {
        refused(attr, &format!("{CLEAN_USES}#[{attr}]\n{CLEAN_ROUTES}"));
    }
    refused(
        "alias",
        &format!("{CLEAN_USES}use other::routes as r;\n{CLEAN_ROUTES}"),
    );
    refused(
        "second fn",
        &format!("{CLEAN_USES}{CLEAN_ROUTES}fn routes() {{}}\n"),
    );
    refused(
        "crate attribute",
        &format!("#![cfg(any())]\n{CLEAN_USES}{CLEAN_ROUTES}"),
    );
    assert_eq!(
        check(&format!("{CLEAN_USES}{CLEAN_ROUTES}"), &["pub_renew"]),
        Ok(())
    );
    assert_eq!(
        check(
            &format!("{CLEAN_USES}#[allow(unused)]\n{CLEAN_ROUTES}"),
            &["pub_renew"]
        ),
        Ok(())
    );
}

#[test]
fn bypass_use_of_a_method_from_a_path_that_is_not_axum_routing_is_refused() {
    for import in [
        "use crate::evil::routing::{get, post};",
        "use super::routing::get;\nuse axum::routing::post;",
        "use axum::{routing::get, evil::post};",
        "use axum::routing::post;\nuse crate::evil::get;",
        "use axum::routing::post;\nuse self::get;",
        "use axum::Router;\nuse axum::routing::{post, evil::get};",
        "use axum::routing::post;\nuse crate::evil::Router;",
        "use ::axum::routing::post;",
        "use crate::evil::axum;",
    ] {
        let uses = if import.contains("axum::Router;") {
            String::new()
        } else {
            "use axum::Router;\n".to_owned()
        };
        refused(import, &format!("{uses}{import}\n{CLEAN_ROUTES}"));
    }
    let nested = "use axum::{Router, routing::{get, post}};\n";
    assert_eq!(
        check(&format!("{nested}{CLEAN_ROUTES}"), &["pub_renew"]),
        Ok(())
    );
}

#[test]
fn bypass_glob_imports_are_refused() {
    for glob in [
        "use crate::evil::*;",
        "use axum::routing::*;",
        "use axum::{routing::*, extract::State};",
        "use super::*;",
    ] {
        refused(glob, &format!("{CLEAN_USES}{glob}\n{CLEAN_ROUTES}"));
    }
}

#[test]
fn bypass_local_items_named_like_a_method_are_refused() {
    for item in [
        "const get: fn(u8) -> u8 = evil_get;",
        "static get: u8 = 1;",
        "struct get;",
        "enum post { A }",
        "type get = u8;",
        "fn get(h: u8) -> u8 { h }",
        "fn other() { let get = 1; }",
        "fn other() { let (a, delete) = (1, 2); }",
        "fn other() { let f = |get| get; }",
        "fn other(options: u8) {}",
        "const on: u8 = 1;",
        "static any: u8 = 1;",
    ] {
        refused(item, &format!("{CLEAN_USES}{item}\n{CLEAN_ROUTES}"));
    }
}

#[test]
fn bypass_extern_crate_shadowing_is_refused() {
    for ext in [
        "#[macro_use] extern crate evil;",
        "extern crate evil as axum;",
        "extern crate evil;",
        "extern \"C\" { fn f(); }",
    ] {
        refused(ext, &format!("{ext}\n{CLEAN_USES}{CLEAN_ROUTES}"));
    }
}

#[test]
fn bypass_macro_shadowing_and_qualified_logging_are_refused() {
    for (label, src) in [
        (
            "use of a non-tracing `tracing`",
            format!(
                "use crate::evil::tracing;\n{CLEAN_USES}{CLEAN_ROUTES}fn f() {{ tracing::error!(\"x\"); }}\n"
            ),
        ),
        (
            "json imported from elsewhere next to serde_json",
            format!(
                "use crate::evil::{{json, serde_json}};\n{CLEAN_USES}{CLEAN_ROUTES}fn f() {{ let _ = json!({{}}); }}\n"
            ),
        ),
        (
            "qualified logging macro from another path",
            format!(
                "{CLEAN_USES}{CLEAN_ROUTES}fn f() {{ crate::evil::tracing::error!(\"x\"); }}\n"
            ),
        ),
        (
            "use tree nested far too deep",
            format!(
                "{CLEAN_USES}use a::{}b{};\n{CLEAN_ROUTES}",
                "{".repeat(100_000),
                "}".repeat(100_000)
            ),
        ),
    ] {
        refused(label, &src);
    }
    let ok = format!(
        "{CLEAN_USES}use serde_json::json;\n{CLEAN_ROUTES}fn f() {{ tracing::error!(\"x\"); let _ = json!({{}}); }}\n"
    );
    assert_eq!(check(&ok, &["pub_renew"]), Ok(()));
}

#[test]
fn bypass_serves_decoy_in_a_comment_is_refused() {
    let list = vec!["pub_renew".to_owned()];
    let real = "pub const SERVES: &[&str] = &[\"pub_renew\"];\n";
    assert_eq!(scanner::check_serves_tokens("f.rs", real, &list), Ok(()));
    let decoy = "/*\npub const SERVES: &[&str] = &[\"pub_renew\"];\n*/\npub  const  SERVES: &[&str] = &[\"pub_renew\", \"admin_op\"];\n";
    assert!(scanner::check_serves_tokens("f.rs", decoy, &list).is_err());
    let hidden = "// pub const SERVES: &[&str] = &[\"pub_renew\"];\nconst SERVES: &[&str] = &[\"admin_op\"];\n";
    assert!(scanner::check_serves_tokens("f.rs", hidden, &list).is_err());
    assert!(scanner::check_serves_tokens("f.rs", "", &list).is_err());
}
