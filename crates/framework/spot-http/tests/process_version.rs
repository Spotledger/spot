//! The process version and migration operations over the wire (WP-BPM-09.T3, native API 0.12.0):
//! publish, list, get, retire, migration plan, start and get through `VersionPort`, and the
//! security properties the brief names: cross-tenant access, fail-open permission checks,
//! system-actor escalation, the existence oracle, bounds and logging.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::idempotency::{IdempotencyStore, MemoryIdempotencyStore};
use spot_http::api::ports::process_version::{
    Grant, MAX_DEFINITION_BYTES, MemoryVersionPort, MigrationPlan, MigrationProgress,
    MigrationRequest, MigrationState, Problem, PublishRequest, Published, VersionDetail,
    VersionPage, VersionPort, VersionPortError, VersionResult, VersionState, VersionSummary,
    content_hash,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{RouteState, process_version};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const HOST_B: &str = "erp.beta.com";
const ACME: &str = "acme";
const BETA: &str = "beta";
const MARK: &str = "SECRETMARK-do-not-echo";
const VERSIONS: &str = "/api/v1/process/expense_claim/versions";
const NO_ID: &str = "/api/v1/process/expense_claim/migrations/mig_9999999999999999";

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    bytes: Vec<u8>,
}

type Shape = (u16, Json, Option<String>, Option<String>);

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
    /// `(status, code)`: what a caller can tell two refusals apart by.
    fn kind(&self) -> (u16, String) {
        (self.status, self.code())
    }
    /// The JSON value at `pointer` (`Null` when absent).
    fn at(&self, pointer: &str) -> Json {
        self.json().pointer(pointer).cloned().unwrap_or(Json::Null)
    }
    /// Asserts a refusal: this status and this code.
    fn refused(&self, status: u16, code: &str) {
        assert_eq!(self.kind(), (status, code.to_owned()), "{}", self.text());
    }
    /// Status, body (request id removed) and comparable headers: two answers that must be
    /// indistinguishable are equal under this.
    fn shape(&self) -> Shape {
        let mut body = self.json();
        if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
            error.remove("request_id");
        }
        let h = |n: &str| self.header(n).map(str::to_owned);
        (self.status, body, h("content-type"), h("cache-control"))
    }
}

fn sites() -> StaticSites {
    let acme = SiteId::new(ACME).unwrap();
    let beta = SiteId::new(BETA).unwrap();
    StaticSites::new()
        .add(HOST, acme.clone())
        .add(HOST_B, beta.clone())
        .add_site_name(acme)
        .add_site_name(beta)
}

fn router_over(ports: &Ports) -> axum::Router {
    let state = RouteState::new(ports.clone());
    ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(process_version::routes().with_state(state))
        .build()
}

fn idempotency() -> Arc<MemoryIdempotencyStore> {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    Arc::new(MemoryIdempotencyStore::new(clock))
}

struct Fixture {
    router: axum::Router,
    port: Arc<MemoryVersionPort>,
}

/// `alice` publishes and reads, `sue` also publishes service-principal definitions, `bob` only
/// reads, `carol` migrates, `dave` is signed in and holds nothing. `expense_claim` exists on both
/// sites; on beta only `alice` holds `process.publish`.
fn fixture() -> Fixture {
    let port = Arc::new(MemoryVersionPort::new());
    for site in [ACME, BETA] {
        port.define_process(site, "expense_claim");
        port.grant(site, "alice", Grant::Publish);
    }
    port.define_process(ACME, "payroll_run");
    port.grant(ACME, "sue", Grant::Publish);
    port.grant(ACME, "sue", Grant::PublishServicePrincipal);
    port.grant(ACME, "bob", Grant::ReadVersions);
    port.grant(ACME, "carol", Grant::Migrate);
    let mut ports = Ports::new();
    ports.provide::<dyn VersionPort>(port.clone());
    ports.provide::<dyn IdempotencyStore>(idempotency());
    Fixture {
        router: router_over(&ports),
        port,
    }
}

async fn send(
    router: &axum::Router,
    host: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let mut has_content_type = false;
    for (name, value) in extra {
        has_content_type |= name.eq_ignore_ascii_case("content-type");
        builder = builder.header(*name, *value);
    }
    if body.is_some() && !has_content_type {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    let request = builder
        .body(body.map_or_else(Body::empty, Body::from))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let (status, headers) = (response.status().as_u16(), response.headers().clone());
    let collected = response.into_body().collect().await.unwrap();
    Reply {
        status,
        headers,
        bytes: collected.to_bytes().to_vec(),
    }
}

impl Fixture {
    async fn get(&self, path: &str, token: &str) -> Reply {
        send(&self.router, HOST, "GET", path, Some(token), &[], None).await
    }
    async fn plan(&self, from: u32, token: &str, body: &Json) -> Reply {
        self.post(&plan_url(from), token, None, body).await
    }
    async fn start(&self, from: u32, token: &str, key: Option<&str>, body: &Json) -> Reply {
        self.post(&start_url(from), token, key, body).await
    }
    async fn version(&self, n: u32, token: &str) -> Reply {
        self.get(&format!("{VERSIONS}/{n}"), token).await
    }
    async fn retire(&self, n: u32, token: &str) -> Reply {
        let url = format!("{VERSIONS}/{n}/retire");
        self.bare(HOST, "POST", &url, token).await
    }
    fn count(&self) -> usize {
        self.port.version_count(ACME, "expense_claim")
    }
    fn migrations(&self) -> usize {
        self.port.migration_count(ACME)
    }
    /// A request with no body and no headers (a retire, a read on another site).
    async fn bare(&self, host: &str, method: &str, path: &str, token: &str) -> Reply {
        send(&self.router, host, method, path, Some(token), &[], None).await
    }
    async fn raw(&self, path: &str, token: &str, hdr: &[(&str, &str)], body: &[u8]) -> Reply {
        let body = Some(body.to_vec());
        send(&self.router, HOST, "POST", path, Some(token), hdr, body).await
    }
    async fn post(&self, path: &str, token: &str, key: Option<&str>, body: &Json) -> Reply {
        let extra: Vec<(&str, &str)> = key.map(|k| ("idempotency-key", k)).into_iter().collect();
        let bytes = serde_json::to_vec(body).unwrap();
        self.raw(path, token, &extra, &bytes).await
    }
    async fn publish(&self, token: &str, key: &str, body: &Json) -> Reply {
        self.post(VERSIONS, token, Some(key), body).await
    }
    /// Publishes version `n` (a plain definition) as alice.
    async fn published(&self, n: u32) {
        let body =
            json!({"definition": definition(&format!("rev {n}\n")), "note": format!("v{n}")});
        let reply = self.publish("alice", &format!("k-{n}"), &body).await;
        assert_eq!(reply.status, 201, "{}", reply.text());
    }
    /// Two published versions, three runs on version 1 and one run that cannot move.
    async fn migratable() -> Fixture {
        let f = fixture();
        f.published(1).await;
        f.published(2).await;
        f.port.set_running_runs(ACME, "expense_claim", 1, 3);
        let (run, reason) = ("run_00000000000000000009", "bpm_versioning.unmapped_node");
        f.port.add_unmovable(ACME, "expense_claim", 1, run, reason);
        f
    }
}

fn definition(extra: &str) -> String {
    format!("process expense_claim\nnode start\nnode review\n{extra}")
}

fn plan_url(from: u32) -> String {
    format!("{VERSIONS}/{from}/migrations/plan")
}

fn start_url(from: u32) -> String {
    format!("{VERSIONS}/{from}/migrations")
}

fn migration_url(id: &str) -> String {
    format!("/api/v1/process/expense_claim/migrations/{id}")
}

// ------------------------------------------------------------------------------------------
// Publish

#[tokio::test]
async fn publish_returns_the_next_version_with_the_content_hash_as_etag() {
    let f = fixture();
    let text = definition("rev 1\n");
    let reply = f
        .publish(
            "alice",
            "k1",
            &json!({"definition": text, "note": "First."}),
        )
        .await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    assert_eq!(reply.at("/data/version"), 1);
    assert_eq!(reply.at("/data/state"), "published");
    assert_eq!(reply.at("/data/published_by"), "alice");
    assert_eq!(reply.at("/data/note"), "First.");
    assert_eq!(reply.at("/data/running_runs"), 0);
    let etag = format!("\"{}\"", content_hash(&text));
    assert_eq!(reply.header("etag"), Some(etag.as_str()));
    assert_eq!(reply.header("cache-control"), Some("private, no-store"));
    f.published(2).await;
    let third = json!({"definition": definition("rev 3\n"), "expected_previous": 2});
    let made = f.publish("alice", "k3", &third).await;
    assert_eq!(made.at("/data/version"), 3);
    assert_eq!(f.count(), 3);
}

#[tokio::test]
async fn an_expected_previous_that_is_not_the_latest_is_409_and_stores_nothing() {
    let f = fixture();
    f.published(1).await;
    for expected in [0, 2, 7] {
        let body = json!({"definition": definition(""), "expected_previous": expected});
        let reply = f.publish("alice", &format!("c{expected}"), &body).await;
        reply.refused(409, "process.version_conflict");
    }
    assert_eq!(f.count(), 1);
    let body = json!({"definition": definition(""), "expected_previous": 1});
    assert_eq!(f.publish("alice", "ok", &body).await.status, 201);
}

#[tokio::test]
async fn a_replay_returns_the_first_answer_and_a_changed_body_is_a_conflict() {
    let f = fixture();
    let body = json!({"definition": definition("rev 1\n"), "note": "n"});
    let first = f.publish("alice", "same", &body).await;
    let again = f.publish("alice", "same", &body).await;
    assert_eq!((first.status, again.status), (201, 200), "{}", again.text());
    assert_eq!(again.header("idempotency-replayed"), Some("true"));
    assert_eq!(first.header("idempotency-replayed"), None);
    assert_eq!(first.json(), again.json());
    assert_eq!(first.header("etag"), again.header("etag"));
    assert_eq!(f.count(), 1, "a replay publishes nothing");
    let other = json!({"definition": definition("rev 2\n")});
    f.publish("alice", "same", &other)
        .await
        .refused(409, "api.idempotency_conflict");
    // The key is scoped by credential: sue's "same" is her own key.
    assert_eq!(f.publish("sue", "same", &other).await.status, 201);
}

#[tokio::test]
async fn a_malformed_publish_body_is_refused_before_the_port() {
    let f = fixture();
    let ok = definition("");
    let cases = [
        json!({}),
        json!({"definition": ""}),
        json!({"definition": 7}),
        json!([1]),
        json!({"definition": ok, "note": "n".repeat(501)}),
        json!({"definition": ok, "note": 4}),
        json!({"definition": ok, "expected_previous": -1}),
        json!({"definition": ok, "expected_previous": 1.5}),
        json!({"definition": ok, "expected_previous": "1"}),
        json!({"definition": ok, "expected_previous": 4294967296u64}),
        json!({"definition": "x".repeat(MAX_DEFINITION_BYTES + 1)}),
    ];
    for (n, body) in cases.iter().enumerate() {
        let reply = f.publish("alice", &format!("m{n}"), body).await;
        reply.refused(400, "api.invalid_request");
    }
    let key = [("idempotency-key", "j")];
    f.raw(VERSIONS, "alice", &key, b"{nope")
        .await
        .refused(400, "api.invalid_json");
    f.raw(VERSIONS, "alice", &key, b"")
        .await
        .refused(400, "api.invalid_json");
    let text = [("idempotency-key", "t"), ("content-type", "text/plain")];
    assert_eq!(f.raw(VERSIONS, "alice", &text, b"x").await.status, 415);
    assert_eq!(f.count(), 0, "nothing reached the port");
}

#[tokio::test]
async fn a_definition_of_exactly_256_kib_is_accepted_and_goes_to_the_port_untouched() {
    let f = fixture();
    let mut text = definition("");
    text.push_str(&" \t\r\n\"\\é ".repeat(10));
    text.push_str(&"x".repeat(MAX_DEFINITION_BYTES - text.len()));
    assert_eq!(text.len(), MAX_DEFINITION_BYTES);
    let reply = f
        .publish("alice", "big", &json!({"definition": text}))
        .await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    let read = f.version(1, "alice").await;
    assert_eq!(read.at("/data/definition"), text, "stored as sent");
    assert_eq!(read.at("/data/hash"), content_hash(&text));
}

#[tokio::test]
async fn an_oversize_body_is_refused_while_streaming_and_a_declared_length_before_reading() {
    let f = fixture();
    let mut body = br#"{"definition":""#.to_vec();
    body.resize(process_version::PUBLISH_BODY_CAP + 1, b'x');
    let reply = f
        .raw(VERSIONS, "alice", &[("idempotency-key", "o1")], &body)
        .await;
    reply.refused(413, Code::HttpBodyTooLarge.as_str());
    let declared = [("idempotency-key", "o2"), ("content-length", "10485760")];
    assert_eq!(f.raw(VERSIONS, "alice", &declared, b"{}").await.status, 413);
    assert_eq!(f.count(), 0);
}

#[tokio::test]
async fn an_invalid_definition_is_422_with_node_ids_and_kinds_only() {
    let f = fixture();
    let body = json!({"definition": format!("process x\nbad ghost\nbad node_2\n{MARK}")});
    let reply = f.publish("alice", "inv", &body).await;
    reply.refused(422, "process.definition_invalid");
    let expected = json!([
        {"node": "ghost", "kind": "bpm_model.unknown_node"},
        {"node": "node_2", "kind": "bpm_model.unknown_node"},
    ]);
    assert_eq!(reply.at("/error/details/problems"), expected);
    assert!(!reply.text().contains(MARK), "echoed: {}", reply.text());
    let syntax = f
        .publish("alice", "syn", &json!({"definition": "not a process"}))
        .await;
    let problem = syntax.at("/error/details/problems/0");
    assert_eq!(problem, json!({"kind": "bpm_model.syntax"}));
    let audit = format!("{:?}", f.port.audit());
    assert!(audit.contains("process.definition_invalid"), "{audit}");
    assert!(
        !audit.contains(MARK),
        "audit records no definition: {audit}"
    );
}

// ------------------------------------------------------------------------------------------
// Permissions, the existence oracle and the actor

#[tokio::test]
async fn a_caller_without_the_permission_gets_the_answer_for_an_unknown_process() {
    let f = fixture();
    f.published(1).await;
    let body = json!({"definition": definition("")});
    let ghost = "/api/v1/process/ghost_process/versions";
    let (one, retire) = (format!("{VERSIONS}/1"), format!("{VERSIONS}/1/retire"));
    let (g_one, g_retire) = (format!("{ghost}/1"), format!("{ghost}/1/retire"));
    let denied = [
        f.get(VERSIONS, "dave").await,
        f.get(&one, "dave").await,
        f.publish("dave", "d1", &body).await,
        f.bare(HOST, "POST", &retire, "dave").await,
    ];
    let unknown = [
        f.get(ghost, "alice").await,
        f.get(&g_one, "alice").await,
        f.post(ghost, "alice", Some("d1"), &body).await,
        f.bare(HOST, "POST", &g_retire, "alice").await,
    ];
    for (denied, unknown) in denied.iter().zip(&unknown) {
        denied.refused(404, "process.not_found");
        assert_eq!(
            denied.shape(),
            unknown.shape(),
            "the same status, body and headers"
        );
    }
    // An unknown version of a visible process is the same answer again.
    let no_version = f.version(99, "alice").await;
    assert_eq!(no_version.shape(), denied[1].shape());
    // A reader (`process.read_versions`) can read, but not publish.
    assert_eq!(f.get(VERSIONS, "bob").await.status, 200);
    assert_eq!(f.publish("bob", "b1", &body).await.status, 404);
    assert_eq!(f.count(), 1);
}

#[tokio::test]
async fn a_service_principal_definition_is_403_for_a_publisher_without_the_extra_permission() {
    let f = fixture();
    let body = json!({"definition": definition("acts_as service_principal\n")});
    let reply = f.publish("alice", "sp1", &body).await;
    reply.refused(403, "process.publish_denied");
    assert!(!reply.text().contains("alice"), "{}", reply.text());
    let other = "/api/v1/process/payroll_run/versions";
    assert_eq!(f.post(other, "sue", Some("sp2"), &body).await.status, 201);
    // A caller who cannot see the process learns nothing of the rule: the plain 404.
    f.publish("dave", "sp3", &body)
        .await
        .refused(404, "process.not_found");
}

#[tokio::test]
async fn nothing_names_the_actor_but_the_credential() {
    let f = fixture();
    for (n, field) in ["actor", "published_by", "user", "tenant", "site", "role"]
        .iter()
        .enumerate()
    {
        let mut body = json!({"definition": definition("")});
        body[*field] = json!("root");
        let reply = f.publish("alice", &format!("a{n}"), &body).await;
        reply.refused(400, "api.invalid_request");
    }
    let hdr = [
        ("idempotency-key", "h1"),
        ("x-user", "root"),
        ("x-site", BETA),
    ];
    let body = serde_json::to_vec(&json!({"definition": definition("")})).unwrap();
    let reply = f.raw(VERSIONS, "alice", &hdr, &body).await;
    assert_eq!(reply.at("/data/published_by"), "alice", "{}", reply.text());
    let on_beta = f.port.version_count(BETA, "expense_claim");
    assert_eq!(on_beta, 0, "a header never picks the site");
    for query in ["user=root", "limit=1&limit=2", "x"] {
        assert_eq!(
            f.get(&format!("{VERSIONS}?{query}"), "alice").await.status,
            400,
            "{query}"
        );
    }
}

#[tokio::test]
async fn one_sites_token_reads_nothing_of_the_other() {
    let f = fixture();
    f.published(1).await;
    // Alice also publishes on beta (she holds process.publish there): the same process name, its
    // own versions.
    let beta_body = json!({"definition": definition(&format!("{MARK}\n")), "note": "beta only"});
    let bytes = serde_json::to_vec(&beta_body).unwrap();
    let key = [("idempotency-key", "bk")];
    let made = send(
        &f.router,
        HOST_B,
        "POST",
        VERSIONS,
        Some("alice"),
        &key,
        Some(bytes),
    )
    .await;
    assert_eq!(made.status, 201, "{}", made.text());
    let acme = f.get(VERSIONS, "alice").await;
    assert_eq!(acme.at("/data").as_array().unwrap().len(), 1);
    assert!(!acme.text().contains("beta only"), "{}", acme.text());
    let beta = f.bare(HOST_B, "GET", VERSIONS, "alice").await;
    assert!(beta.text().contains("beta only"), "{}", beta.text());
    assert!(!beta.text().contains("v1"), "{}", beta.text());
    // Bob reads on acme only: beta answers him as if the process did not exist.
    f.bare(HOST_B, "GET", VERSIONS, "bob")
        .await
        .refused(404, "process.not_found");
    let one = f.bare(HOST_B, "GET", &format!("{VERSIONS}/1"), "bob").await;
    assert!(one.status == 404 && !one.text().contains(MARK));
}

// ------------------------------------------------------------------------------------------
// List, get and retire

#[tokio::test]
async fn the_list_is_newest_first_paged_by_an_opaque_cursor_and_bounded() {
    let f = fixture();
    for n in 1..=3 {
        f.published(n).await;
    }
    f.port.set_running_runs(ACME, "expense_claim", 2, 4);
    let first = f.get(&format!("{VERSIONS}?limit=2"), "bob").await;
    assert_eq!(first.status, 200, "{}", first.text());
    assert_eq!(first.at("/data/0/version"), 3);
    assert_eq!(first.at("/data/0/note"), "v3");
    assert_eq!(first.at("/data/1/version"), 2);
    assert_eq!(first.at("/data/1/running_runs"), 4);
    let cursor = first.at("/next_cursor").as_str().unwrap().to_owned();
    let second = f
        .get(&format!("{VERSIONS}?limit=2&cursor={cursor}"), "bob")
        .await;
    assert_eq!(second.at("/data").as_array().unwrap().len(), 1);
    assert_eq!(second.at("/next_cursor"), Json::Null);
    assert_eq!(first.header("cache-control"), Some("private, no-store"));
    for bad in ["limit=0", "limit=101", "limit=x", "cursor="] {
        assert_eq!(
            f.get(&format!("{VERSIONS}?{bad}"), "bob").await.status,
            400,
            "{bad}"
        );
    }
    let forged = f.get(&format!("{VERSIONS}?cursor=zzz"), "bob").await;
    forged.refused(400, "api.invalid_request");
}

#[tokio::test]
async fn get_carries_the_content_hash_as_etag_and_honours_if_none_match() {
    let f = fixture();
    f.published(1).await;
    let url = format!("{VERSIONS}/1");
    let reply = f.get(&url, "bob").await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let quoted = format!("\"{}\"", reply.at("/data/hash").as_str().unwrap());
    assert_eq!(reply.header("etag"), Some(quoted.as_str()));
    assert_eq!(reply.header("cache-control"), Some("no-store"));
    assert_eq!(reply.at("/data/definition"), definition("rev 1\n"));
    let cond = [("if-none-match", quoted.as_str())];
    let hit = send(&f.router, HOST, "GET", &url, Some("bob"), &cond, None).await;
    assert!(hit.status == 304 && hit.bytes.is_empty(), "{}", hit.text());
    assert_eq!(hit.header("etag"), Some(quoted.as_str()));
    // A caller without the permission gets the 404, never a 304 that confirms the version.
    let denied = send(&f.router, HOST, "GET", &url, Some("dave"), &cond, None).await;
    denied.refused(404, "process.not_found");
}

#[tokio::test]
async fn a_path_that_is_not_the_documented_form_is_a_bad_request() {
    let f = fixture();
    f.published(1).await;
    for n in ["0", "01", "-1", "+1", "1.5", "abc", "1000000000"] {
        f.get(&format!("{VERSIONS}/{n}"), "alice")
            .await
            .refused(400, "api.invalid_request");
    }
    let bad_name = f.get("/api/v1/process/Has-Dash/versions", "alice").await;
    assert_eq!(bad_name.status, 400);
    assert_eq!(
        f.get(&format!("{VERSIONS}/1?x=1"), "alice").await.status,
        400
    );
}

#[tokio::test]
async fn retire_moves_a_published_version_to_retired_once() {
    let f = fixture();
    f.published(1).await;
    let url = format!("{VERSIONS}/1/retire");
    // The operation takes no body: one that might name an actor is refused unread.
    let body = f.post(&url, "alice", None, &json!({"actor": "root"})).await;
    body.refused(400, "api.invalid_request");
    let retired = f.bare(HOST, "POST", &url, "alice").await;
    assert_eq!(retired.status, 200, "{}", retired.text());
    assert_eq!(retired.at("/data/state"), "retired");
    f.bare(HOST, "POST", &url, "alice")
        .await
        .refused(409, "process.version_not_published");
    let kept = f.version(1, "alice").await;
    assert_eq!(kept.at("/data/state"), "retired");
}

// ------------------------------------------------------------------------------------------
// Migrations

#[tokio::test]
async fn a_plan_is_read_only_and_a_start_creates_one_migration() {
    let f = Fixture::migratable().await;
    let writes = f.port.write_count();
    let plan = f.post(&plan_url(1), "carol", None, &json!({"to": 2})).await;
    assert_eq!(plan.status, 200, "{}", plan.text());
    assert_eq!(plan.at("/data/from"), 1);
    assert_eq!(plan.at("/data/to"), 2);
    assert_eq!(plan.at("/data/movable"), 3);
    assert_eq!(plan.at("/data/unmoved_total"), 1);
    let run = "run_00000000000000000009";
    let unmoved = json!({"run": run, "reason": "bpm_versioning.unmapped_node"});
    assert_eq!(plan.at("/data/unmoved/0"), unmoved);
    let mapped = json!({"to": 2, "mapping": {"nodes": {"review": "review"}}});
    let mapped = f.plan(1, "carol", &mapped).await;
    assert_eq!(mapped.status, 200);
    assert_eq!(f.port.write_count(), writes, "a plan wrote nothing");
    assert_eq!(f.migrations(), 0);

    let started = f.start(1, "carol", Some("m1"), &json!({"to": 2})).await;
    assert_eq!(started.status, 202, "{}", started.text());
    let id = started.at("/migration_id").as_str().unwrap().to_owned();
    assert_eq!(f.migrations(), 1);
    assert_eq!(f.port.write_count(), writes + 1);
    let again = f.start(1, "carol", Some("m1"), &json!({"to": 2})).await;
    assert_eq!(again.status, 200, "a replay answers 200 with the first id");
    assert_eq!(again.at("/migration_id"), id.as_str());
    assert_eq!(again.header("idempotency-replayed"), Some("true"));
    let other = f.start(1, "carol", Some("m1"), &json!({"to": 3})).await;
    other.refused(409, "api.idempotency_conflict");
    assert_eq!(f.migrations(), 1, "a replay starts nothing");
    let progress = f.get(&migration_url(&id), "carol").await;
    assert_eq!(progress.at("/data/state"), "running");
    assert_eq!(progress.at("/data/ended_at"), Json::Null);
    f.port.finish_migration(ACME, &id, 3, 1, 0);
    let done = f.get(&migration_url(&id), "carol").await;
    assert_eq!(done.at("/data/state"), "completed");
    assert_eq!(done.at("/data/moved"), 3);
    assert_eq!(done.at("/data/skipped"), 1);
    assert!(done.at("/data/ended_at").is_string());
}

#[tokio::test]
async fn the_migration_refusals_are_the_contracts() {
    let f = Fixture::migratable().await;
    let retire = format!("{VERSIONS}/2/retire");
    assert_eq!(f.bare(HOST, "POST", &retire, "alice").await.status, 200);
    let ghost = json!({"to": 1, "mapping": {"nodes": {"a": "ghost"}}});
    let cases = [
        (json!({"to": 1}), 422, "process.migration_invalid"),
        (json!({"to": 9}), 409, "process.version_not_published"),
        (json!({"to": 2}), 409, "process.version_not_published"),
        (ghost.clone(), 422, "process.migration_invalid"),
    ];
    for (body, status, code) in cases {
        f.post(&plan_url(1), "carol", None, &body)
            .await
            .refused(status, code);
        f.start(1, "carol", Some("x"), &body)
            .await
            .refused(status, code);
    }
    assert_eq!(f.migrations(), 0, "a refused start created nothing");
    // From the retired version 2 to the published version 1: the mapping names a node 1 lacks.
    f.post(&plan_url(2), "carol", None, &ghost)
        .await
        .refused(422, "process.migration_invalid");
}

#[tokio::test]
async fn a_malformed_migration_body_is_refused_before_the_port() {
    let f = Fixture::migratable().await;
    let nodes: serde_json::Map<String, Json> =
        (0..501).map(|i| (format!("n{i}"), json!("m"))).collect();
    let defaults: serde_json::Map<String, Json> =
        (0..101).map(|i| (format!("v{i}"), json!(1))).collect();
    let cases = [
        json!({}),
        json!({"to": 0}),
        json!({"to": "2"}),
        json!({"to": 2.5}),
        json!({"to": 2, "actor": "root"}),
        json!({"to": 2, "mapping": 5}),
        json!({"to": 2, "mapping": {"extra": 1}}),
        json!({"to": 2, "mapping": {"nodes": {"a": 1}}}),
        json!({"to": 2, "mapping": {"nodes": {"": "b"}}}),
        json!({"to": 2, "mapping": {"nodes": nodes}}),
        json!({"to": 2, "mapping": {"required_variables": [""]}}),
        json!({"to": 2, "mapping": {"required_variables": vec!["a"; 101]}}),
        json!({"to": 2, "mapping": {"variable_defaults": defaults}}),
        json!({"to": 2, "mapping": {"variable_defaults": {"big": "x".repeat(70_000)}}}),
    ];
    for (n, body) in cases.iter().enumerate() {
        f.post(&plan_url(1), "carol", None, body)
            .await
            .refused(400, "api.invalid_request");
        let start = f
            .post(&start_url(1), "carol", Some(&format!("b{n}")), body)
            .await;
        assert_eq!(start.status, 400, "start {n}");
    }
    let audit = f.port.audit();
    assert!(
        audit.iter().all(|a| !a.operation.contains("migration")),
        "{audit:?}"
    );
}

#[tokio::test]
async fn the_migration_operations_answer_the_same_404_for_not_there_and_not_yours() {
    let f = Fixture::migratable().await;
    let started = f.start(1, "carol", Some("m"), &json!({"to": 2})).await;
    let id = started.at("/migration_id").as_str().unwrap().to_owned();
    let body = json!({"to": 2});
    // Alice may publish but not migrate: she sees what a stranger sees.
    let ghost = "/api/v1/process/ghost_process/versions/1/migrations";
    let denied = [
        f.plan(1, "alice", &body).await,
        f.start(1, "alice", Some("a"), &body).await,
    ];
    let unknown = [
        f.post(&format!("{ghost}/plan"), "carol", None, &body).await,
        f.post(ghost, "carol", Some("g"), &body).await,
    ];
    for (denied, unknown) in denied.iter().zip(&unknown) {
        denied.refused(404, "process.not_found");
        assert_eq!(denied.shape(), unknown.shape());
    }
    let from_nine = f.plan(9, "carol", &body).await;
    assert_eq!(from_nine.shape(), denied[0].shape());
    // The migration read: wrong permission, wrong process and unknown id are one answer.
    let elsewhere = format!("/api/v1/process/payroll_run/migrations/{id}");
    let answers = [
        f.get(&migration_url(&id), "alice").await,
        f.get(&elsewhere, "carol").await,
        f.get(NO_ID, "carol").await,
    ];
    answers[0].refused(404, "process.migration_not_found");
    assert_eq!(answers[0].shape(), answers[1].shape());
    assert_eq!(answers[0].shape(), answers[2].shape());
    assert_eq!(f.get(&migration_url("short"), "carol").await.status, 400);
}

// ------------------------------------------------------------------------------------------
// Fail closed

const ALL: [(&str, &str); 7] = [
    ("GET", "/versions"),
    ("POST", "/versions"),
    ("GET", "/versions/1"),
    ("POST", "/versions/1/retire"),
    ("POST", "/versions/1/migrations/plan"),
    ("POST", "/versions/1/migrations"),
    ("GET", "/migrations/mig_0000000000000001"),
];

/// A valid body for each operation of [`ALL`] (a retire takes none).
fn body_of(index: usize) -> Option<Vec<u8>> {
    match index {
        1 => Some(br#"{"definition":"process x"}"#.to_vec()),
        4 | 5 => Some(br#"{"to":2}"#.to_vec()),
        _ => None,
    }
}

async fn call(router: &axum::Router, index: usize, token: Option<&str>) -> Reply {
    let (method, suffix) = ALL[index];
    let path = format!("/api/v1/process/expense_claim{}", suffix);
    let key = [("idempotency-key", "k")];
    send(router, HOST, method, &path, token, &key, body_of(index)).await
}

async fn call_all(router: &axum::Router, token: Option<&str>) -> Vec<Reply> {
    let mut out = Vec::new();
    for index in 0..ALL.len() {
        out.push(call(router, index, token).await);
    }
    out
}

#[tokio::test]
async fn with_no_port_every_operation_answers_501_never_an_empty_list() {
    let router = router_over(&Ports::new());
    for reply in call_all(&router, Some("alice")).await {
        reply.refused(501, "service.not_configured");
        assert_eq!(reply.header("cache-control"), Some("private, no-store"));
        assert!(reply.json().get("data").is_none(), "{}", reply.text());
    }
    // Without a credential the answer is 401 first, port or not.
    for reply in call_all(&router, None).await {
        reply.refused(401, "auth.required");
    }
}

/// How the broken port misbehaves.
enum Mode {
    Refuse(VersionPortError),
    BadHash,
    LongPage,
    FreeText,
}

struct Broken(Mode);

fn summary() -> VersionSummary {
    VersionSummary {
        version: 1,
        state: VersionState::Published,
        published_at: Some("2026-11-01T00:00:00Z".to_owned()),
        published_by: Some("alice".to_owned()),
        note: String::new(),
        running_runs: 0,
    }
}

impl Broken {
    /// The refusal this mode scripts, or `ok()`.
    fn answer<T>(&self, ok: impl FnOnce() -> T) -> VersionResult<T> {
        match &self.0 {
            Mode::Refuse(error) => Err(error.clone()),
            Mode::FreeText => {
                let kind = format!("Unknown node: {MARK}");
                Err(VersionPortError::DefinitionInvalid(vec![Problem {
                    node: None,
                    kind,
                }]))
            }
            _ => Ok(ok()),
        }
    }
    fn hash(&self) -> String {
        let bad = matches!(self.0, Mode::BadHash);
        if bad {
            "XYZ".to_owned()
        } else {
            content_hash("process x")
        }
    }
}

type Ctx<'a> = &'a RequestCtx;
type Fut<'a, T> = BoxFuture<'a, VersionResult<T>>;

fn zero_plan() -> MigrationPlan {
    MigrationPlan {
        from: 0,
        to: 2,
        movable: 0,
        unmoved_total: 0,
        node_counts: vec![],
        unmoved: vec![],
    }
}

fn late_progress() -> MigrationProgress {
    MigrationProgress {
        state: MigrationState::Running,
        moved: 0,
        skipped: 0,
        failed: 0,
        started_at: "yesterday".to_owned(),
        ended_at: None,
    }
}

impl VersionPort for Broken {
    fn list_versions<'a>(
        &'a self,
        _: Ctx<'a>,
        _: &'a str,
        _: Option<&'a str>,
        _: u32,
    ) -> Fut<'a, VersionPage> {
        let many = if matches!(self.0, Mode::LongPage) {
            101
        } else {
            1
        };
        let items = vec![summary(); many];
        Box::pin(async move {
            self.answer(|| VersionPage {
                items,
                next_cursor: None,
            })
        })
    }
    fn publish<'a>(&'a self, _: Ctx<'a>, _: &'a str, _: PublishRequest) -> Fut<'a, Published> {
        let hash = self.hash();
        Box::pin(async move {
            self.answer(|| Published {
                version: summary(),
                hash,
            })
        })
    }
    fn get_version<'a>(&'a self, _: Ctx<'a>, _: &'a str, _: u32) -> Fut<'a, VersionDetail> {
        let (hash, definition) = (self.hash(), "process x".to_owned());
        let detail = move || VersionDetail {
            version: summary(),
            definition,
            hash,
        };
        Box::pin(async move { self.answer(detail) })
    }
    fn retire<'a>(&'a self, _: Ctx<'a>, _: &'a str, _: u32) -> Fut<'a, VersionSummary> {
        Box::pin(async move {
            self.answer(|| VersionSummary {
                version: 0,
                ..summary()
            })
        })
    }
    fn plan_migration<'a>(
        &'a self,
        _: Ctx<'a>,
        _: &'a str,
        _: u32,
        _: MigrationRequest,
    ) -> Fut<'a, MigrationPlan> {
        Box::pin(async move { self.answer(zero_plan) })
    }
    fn start_migration<'a>(
        &'a self,
        _: Ctx<'a>,
        _: &'a str,
        _: u32,
        _: MigrationRequest,
    ) -> Fut<'a, String> {
        Box::pin(async move { self.answer(|| "not a valid id!".to_owned()) })
    }
    fn get_migration<'a>(
        &'a self,
        _: Ctx<'a>,
        _: &'a str,
        _: &'a str,
    ) -> Fut<'a, MigrationProgress> {
        Box::pin(async move { self.answer(late_progress) })
    }
}

fn broken(mode: Mode) -> axum::Router {
    let mut ports = Ports::new();
    ports.provide::<dyn VersionPort>(Arc::new(Broken(mode)));
    ports.provide::<dyn IdempotencyStore>(idempotency());
    router_over(&ports)
}

#[tokio::test]
async fn a_port_error_outside_the_declared_refusals_is_500_with_no_detail_never_a_pass() {
    let router = broken(Mode::Refuse(VersionPortError::Internal));
    for reply in call_all(&router, Some("alice")).await {
        reply.refused(500, "http.internal");
        assert_eq!(reply.at("/error/details"), Json::Null);
        assert_eq!(reply.at("/data"), Json::Null);
    }
    let router = broken(Mode::Refuse(VersionPortError::Unavailable));
    for reply in call_all(&router, Some("alice")).await {
        reply.refused(503, "server.unavailable");
        assert!(reply.header("retry-after").is_some());
    }
}

#[tokio::test]
async fn a_port_answer_outside_the_contract_is_500_and_free_text_is_never_passed_on() {
    // Broken whatever the mode: a hash that is not a hash, a retired version 0, a plan from 0, an
    // id that is not an id and a progress with a bad stamp.
    let replies = call_all(&broken(Mode::BadHash), Some("alice")).await;
    for reply in replies.iter().skip(1) {
        reply.refused(500, "http.internal");
    }
    call(&broken(Mode::LongPage), 0, Some("alice"))
        .await
        .refused(500, "http.internal");
    let free = call(&broken(Mode::FreeText), 1, Some("alice")).await;
    free.refused(500, "http.internal");
    assert!(
        !free.text().contains(MARK),
        "a problem that is free text is never echoed"
    );
}
