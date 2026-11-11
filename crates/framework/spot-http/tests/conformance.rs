//! Contract conformance (brief WP-PLT-05.T3, suite "Conformance").
//!
//! Reads `contracts/native-api/openapi.yaml` at test time and, for **every** `/api/v1`
//! operation except realtime, sends one valid request and asserts:
//! - the status is one the contract lists for that operation (or the default error shape);
//! - the response body validates against that response's schema, with `jsonschema` resolving
//!   `$ref`s inside the one document (OpenAPI 3.1, JSON Schema 2020-12).
//!
//! The operation walk is exhaustive: a new operation in the contract with no case here fails
//! the test — it is never skipped.

#[allow(dead_code)]
mod support;

use serde_json::Value as Json;
use serde_json::json;
use support::Must;
use support::fake::TOKEN_ALICE;
use support::harness::{self, App};

/// The upload limit of the conformance file store.
const FILE_LIMIT: u64 = 64 * 1024;

/// The harness app plus a file store and a print service, so `file_upload`, `file_get` and
/// `print_get` are served as a host serves them (the harness `App::new` leaves them
/// unconfigured).
fn app_with_files() -> App {
    app_with_renewal().0
}

/// [`app_with_files`] plus the renewal cookie of a seeded sign-in on the harness site (the
/// `auth_consumer_renew` case sends it).
fn app_with_renewal() -> (App, String) {
    use spot_core::{ManualClock, SiteId};
    use spot_http::api::files::MemoryFilesPort;
    use spot_http::api::idempotency::MemoryIdempotencyStore;
    use spot_http::api::ports::consumer_sessions::{ConsumerSessions, MemoryConsumerSessions};
    use spot_http::api::ports::notices::NoticeSource;
    use spot_http::api::router::{ApiServices, router};
    use spot_http::{ServerBuilder, ServerConfig, StaticSites};
    use std::sync::Arc;
    use support::fake::FakeApp;
    use support::permissions::{FakePermissions, FakeReveal};

    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock.clone());
    let permissions = Arc::new(FakePermissions::new());
    let reveal = Arc::new(FakeReveal::new());
    let site = SiteId::new(harness::SITE).must();
    let sessions = Arc::new(MemoryConsumerSessions::new(
        clock.clone(),
        "conformance-seed",
        time::Duration::minutes(15),
    ));
    let (renew_cookie, _) = sessions.sign_in(harness::SITE);
    let services = ApiServices::new(
        fake.clone(),
        fake.clone(),
        fake.clone(),
        fake.clone(),
        store.clone(),
    )
    .with_permissions(permissions.clone())
    .with_reveal(reveal.clone())
    .with_files(Arc::new(MemoryFilesPort::new(FILE_LIMIT)))
    .with_print(Arc::new(spot_http::api::print::MemoryPrintPort::open()))
    .with_port::<dyn NoticeSource>(notices_for_conformance())
    .with_port::<dyn spot_http::api::ports::events::EventStreamPort>(Arc::new(
        ConformanceEvents::default(),
    ))
    .with_port::<dyn spot_http::api::ports::report::ReportCatalogPort>(Arc::new(ConformanceReports))
    .with_port::<dyn ConsumerSessions>(sessions);
    let router = ServerBuilder::new(
        ServerConfig {
            max_body_bytes: 1024 * 1024,
            ..ServerConfig::default()
        },
        StaticSites::new()
            .add(harness::HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build();
    let app = App {
        router,
        fake,
        permissions,
        reveal,
        store,
        clock,
    };
    (app, renew_cookie)
}

/// The notice port of the conformance app: alice and bob each hold a feed on the harness site
/// (the fixed fake-app tokens name them), and bob's notice must never reach alice.
fn notices_for_conformance() -> std::sync::Arc<spot_http::api::ports::notices::MemoryNoticeSource> {
    use spot_http::api::ports::notices::{MemoryNoticeSource, Notice};
    let source = MemoryNoticeSource::new();
    source.grant(harness::SITE, TOKEN_ALICE, "alice");
    source.grant(harness::SITE, support::fake::TOKEN_BOB, "bob");
    let notice = |n: u32| {
        let mut data = serde_json::Map::new();
        data.insert("title".into(), json!(format!("Notice {n}")));
        Notice {
            event_id: format!("ev{n:03}"),
            event: "user_notice".into(),
            data,
            created_at: format!("2026-10-31T08:{n:02}:00Z"),
        }
    };
    for n in 1..=3 {
        source.push(harness::SITE, "alice", notice(n));
    }
    source.push(harness::SITE, "bob", notice(0));
    std::sync::Arc::new(source)
}

/// The event stream port of the conformance app: an in-memory set of consumers owned by the
/// caller the credential names (alice or bob; any other token is refused). Another owner's
/// consumer is `eventstream.consumer_not_found`, the same answer as one that does not exist.
/// A pull takes the consumer's lease and holds it until the consumer is deleted; an open
/// stream takes the lease only while its feed lives and answers `eventstream.consumer_busy`
/// when it is held. A feed yields one frame and ends, so the walk never hangs.
#[derive(Default)]
struct ConformanceEvents {
    state: std::sync::Arc<std::sync::Mutex<EventsState>>,
}

#[derive(Default)]
struct EventsState {
    /// `(owner, name)` to the consumer.
    consumers:
        std::collections::BTreeMap<(String, String), spot_http::api::ports::events::ConsumerOut>,
    /// `(owner, name)` to the holder of the lease.
    leases: std::collections::BTreeMap<(String, String), String>,
}

const ALICE_ID: &str = "0192f3c4-0000-7000-8000-00000000a001";
const BOB_ID: &str = "0192f3c4-0000-7000-8000-00000000b001";
const EVENT_POSITION: &str = "v1.AAAAAAAAAAEAAAAAAAAAKg";

impl ConformanceEvents {
    fn lock(&self) -> std::sync::MutexGuard<'_, EventsState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The `(owner key, owner id)` of the caller; anything but alice or bob is refused.
    fn caller(
        ctx: &spot_http::api::service::RequestCtx,
    ) -> Result<(String, &'static str), spot_http::api::service::ApiError> {
        use spot_http::api::registry::Code;
        use spot_http::api::service::ApiError;
        match ctx.credential.as_ref().map(|c| c.token.as_str()) {
            Some(t) if t == TOKEN_ALICE => Ok(("alice".to_owned(), ALICE_ID)),
            Some(t) if t == support::fake::TOKEN_BOB => Ok(("bob".to_owned(), BOB_ID)),
            _ => Err(ApiError::new(Code::PermissionDenied, "Not allowed.")),
        }
    }

    fn not_found() -> spot_http::api::service::ApiError {
        spot_http::api::service::ApiError::new(
            spot_http::api::registry::Code::EventstreamConsumerNotFound,
            "No such consumer.",
        )
    }

    fn busy() -> spot_http::api::service::ApiError {
        spot_http::api::service::ApiError::new(
            spot_http::api::registry::Code::EventstreamConsumerBusy,
            "Another connection holds this consumer.",
        )
    }

    /// Checks the lease the caller sent against the one held (none held: any is fine).
    fn check_lease(
        state: &EventsState,
        key: &(String, String),
        lease: Option<&str>,
    ) -> Result<String, spot_http::api::service::ApiError> {
        match (state.leases.get(key), lease) {
            (Some(held), Some(sent)) if held == sent => Ok(held.clone()),
            (Some(_), _) => Err(Self::busy()),
            (None, _) => Ok("lease-conformance".to_owned()),
        }
    }
}

struct ConformanceFeed {
    state: std::sync::Arc<std::sync::Mutex<EventsState>>,
    key: (String, String),
    sent: bool,
}

impl Drop for ConformanceFeed {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.leases.remove(&self.key);
    }
}

impl spot_http::api::ports::events::EventFeed for ConformanceFeed {
    fn lease(&self) -> &str {
        "lease-stream"
    }

    fn next(
        &mut self,
    ) -> spot_http::api::service::BoxFuture<'_, spot_http::api::ports::events::FeedItem> {
        use spot_http::api::ports::events::FeedItem;
        Box::pin(async move {
            if self.sent {
                return FeedItem::Closed;
            }
            self.sent = true;
            FeedItem::Frame(
                format!(
                    "id: {EVENT_POSITION}\nevent: doc_posted\ndata: {{\"event_id\":\"e1\"}}\n\n"
                )
                .into_bytes(),
            )
        })
    }
}

impl spot_http::api::ports::events::EventStreamPort for ConformanceEvents {
    fn list<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::events::EventsResult<
            Vec<spot_http::api::ports::events::ConsumerOut>,
        >,
    > {
        Box::pin(async move {
            let (owner, _) = Self::caller(ctx)?;
            let state = self.lock();
            Ok(state
                .consumers
                .iter()
                .filter(|((o, _), _)| *o == owner)
                .map(|(_, c)| c.clone())
                .collect())
        })
    }

    fn put<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
        name: &'a str,
        spec: spot_http::api::ports::events::ConsumerSpecIn,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::events::EventsResult<(
            spot_http::api::ports::events::ConsumerOut,
            bool,
        )>,
    > {
        use spot_http::api::ports::events::{ConsumerOut, PartitionOut};
        Box::pin(async move {
            let (owner, owner_id) = Self::caller(ctx)?;
            let key = (owner, name.to_owned());
            let mut state = self.lock();
            if let Some(existing) = state.consumers.get(&key) {
                return Ok((existing.clone(), false));
            }
            let consumer = ConsumerOut {
                name: name.to_owned(),
                owner: owner_id.to_owned(),
                topics: spec.topics,
                filter: spec.filter,
                partitions: spec.partitions,
                payload_mode: spec.payload_mode,
                partitions_state: (0..spec.partitions)
                    .map(|partition| PartitionOut {
                        partition,
                        checkpoint: EVENT_POSITION.to_owned(),
                        parked: false,
                    })
                    .collect(),
            };
            state.consumers.insert(key, consumer.clone());
            Ok((consumer, true))
        })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
        name: &'a str,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::events::EventsResult<spot_http::api::ports::events::ConsumerOut>,
    > {
        Box::pin(async move {
            let (owner, _) = Self::caller(ctx)?;
            let state = self.lock();
            state
                .consumers
                .get(&(owner, name.to_owned()))
                .cloned()
                .ok_or_else(Self::not_found)
        })
    }

    fn delete<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
        name: &'a str,
    ) -> spot_http::api::service::BoxFuture<'a, spot_http::api::ports::events::EventsResult<()>>
    {
        Box::pin(async move {
            let (owner, _) = Self::caller(ctx)?;
            let key = (owner, name.to_owned());
            let mut state = self.lock();
            state.leases.remove(&key);
            state
                .consumers
                .remove(&key)
                .map(|_| ())
                .ok_or_else(Self::not_found)
        })
    }

    fn pull<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        _request: spot_http::api::ports::events::PullIn,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::events::EventsResult<spot_http::api::ports::events::PullOut>,
    > {
        Box::pin(async move {
            let (owner, _) = Self::caller(ctx)?;
            let key = (owner, name.to_owned());
            let mut state = self.lock();
            if !state.consumers.contains_key(&key) {
                return Err(Self::not_found());
            }
            let held = Self::check_lease(&state, &key, lease)?;
            state.leases.insert(key, held.clone());
            Ok(spot_http::api::ports::events::PullOut {
                lease: held,
                events: Vec::new(),
                next_position: EVENT_POSITION.to_owned(),
                skipped: 0,
                window_full: false,
            })
        })
    }

    fn ack<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        partition: u32,
        position: &'a str,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::events::EventsResult<spot_http::api::ports::events::CheckpointOut>,
    > {
        Box::pin(async move {
            let (owner, _) = Self::caller(ctx)?;
            let key = (owner, name.to_owned());
            let state = self.lock();
            if !state.consumers.contains_key(&key) {
                return Err(Self::not_found());
            }
            Self::check_lease(&state, &key, lease)?;
            Ok(spot_http::api::ports::events::CheckpointOut {
                partition,
                position: position.to_owned(),
            })
        })
    }

    fn restart<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        partition: u32,
        _from: spot_http::api::ports::events::StartIn,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::events::EventsResult<spot_http::api::ports::events::CheckpointOut>,
    > {
        Box::pin(async move {
            let (owner, _) = Self::caller(ctx)?;
            let key = (owner, name.to_owned());
            let state = self.lock();
            if !state.consumers.contains_key(&key) {
                return Err(Self::not_found());
            }
            Self::check_lease(&state, &key, lease)?;
            Ok(spot_http::api::ports::events::CheckpointOut {
                partition,
                position: EVENT_POSITION.to_owned(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        ctx: &'a spot_http::api::service::RequestCtx,
        request: spot_http::api::ports::events::StreamIn,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::events::EventsResult<
            Box<dyn spot_http::api::ports::events::EventFeed>,
        >,
    > {
        Box::pin(async move {
            let (owner, _) = Self::caller(ctx)?;
            let key = (owner, request.consumer);
            let mut state = self.lock();
            if !state.consumers.contains_key(&key) {
                return Err(Self::not_found());
            }
            if state.leases.contains_key(&key) {
                return Err(Self::busy());
            }
            state.leases.insert(key.clone(), "lease-stream".to_owned());
            Ok(Box::new(ConformanceFeed {
                state: self.state.clone(),
                key,
                sent: false,
            })
                as Box<dyn spot_http::api::ports::events::EventFeed>)
        })
    }

    fn limits(&self) -> spot_http::api::ports::events::StreamLimits {
        spot_http::api::ports::events::StreamLimits {
            heartbeat: std::time::Duration::from_secs(25),
            max_idle: std::time::Duration::from_secs(900),
        }
    }
}

/// The spec every conformance consumer is created with.
fn consumer_spec() -> Json {
    json!({"topics": ["doc_posted"], "partitions": 2, "payload_mode": "ids_only", "start": {"kind": "tail"}})
}

/// Creates alice's consumer `name`; asserts the contract's `201`.
async fn put_consumer(app: &App, name: &str) {
    let put = harness::call(
        app,
        "PUT",
        &format!("/api/v1/events/consumers/{name}"),
        &[harness::bearer(TOKEN_ALICE)],
        Some(consumer_spec()),
    )
    .await;
    put.expect_status(201, "seed an event consumer");
}

/// Creates alice's consumer `name` and pulls once; returns the lease the pull answered.
async fn pulled_consumer(app: &App, name: &str) -> String {
    put_consumer(app, name).await;
    let pull = harness::call(
        app,
        "POST",
        &format!("/api/v1/events/consumers/{name}/pull"),
        &[harness::bearer(TOKEN_ALICE)],
        Some(json!({"partition": 0})),
    )
    .await;
    pull.expect_status(200, "seed pull");
    pull.header("x-consumer-lease").must().to_owned()
}

/// The report catalogue of the conformance app: one report, `hours_by_task`, that every signed-in
/// caller may see. Any other name is the one answer for a report that does not exist or that the
/// caller's roles do not admit, `report.not_found`.
struct ConformanceReports;

const CONFORMANCE_REPORT: &str = "hours_by_task";

impl spot_http::api::ports::report::ReportCatalogPort for ConformanceReports {
    fn list<'a>(
        &'a self,
        _ctx: &'a spot_http::api::service::RequestCtx,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::report::CatalogResult<
            Vec<spot_http::api::ports::report::ReportSummaryOut>,
        >,
    > {
        Box::pin(async move {
            Ok(vec![spot_http::api::ports::report::ReportSummaryOut {
                name: CONFORMANCE_REPORT.to_owned(),
                label: "Hours by task".to_owned(),
                app: "sample".to_owned(),
                base_doctype: "time_entry".to_owned(),
            }])
        })
    }

    fn describe<'a>(
        &'a self,
        _ctx: &'a spot_http::api::service::RequestCtx,
        report: &'a str,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::report::CatalogResult<
            spot_http::api::ports::report::ReportDescriptionOut,
        >,
    > {
        use spot_http::api::ports::report::{
            ParameterOptionsOut, ReportDescriptionOut, ReportParameterOut,
        };
        Box::pin(async move {
            if report != CONFORMANCE_REPORT {
                return Err(spot_http::api::service::ApiError::from_code(
                    spot_http::api::registry::Code::ReportNotFound,
                ));
            }
            Ok(ReportDescriptionOut {
                name: CONFORMANCE_REPORT.to_owned(),
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
                ],
                columns: vec![spot_http::api::service::ReportColumn {
                    name: "task".to_owned(),
                    label: "Task".to_owned(),
                    field_type: spot_http::api::meta::FieldType::Link,
                }],
                max_rows: 10_000,
            })
        })
    }
}

/// One request with a raw body (the harness `call` only sends JSON).
async fn raw_call(
    app: &App,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> harness::Call {
    use tower::ServiceExt as _;
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(axum::http::header::HOST, harness::HOST);
    for (name, value) in headers {
        builder = builder.header(*name, value.clone());
    }
    let request = builder.body(axum::body::Body::from(body.to_vec())).must();
    let response = app
        .router
        .clone()
        .oneshot(request)
        .await
        .unwrap_or_else(|error| panic!("router service failed: {error}"));
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .must();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    harness::Call {
        status,
        headers,
        body: serde_json::from_str::<Json>(&raw).unwrap_or(Json::Null),
        raw,
    }
}

fn upload_headers(token: &str) -> Vec<(&'static str, String)> {
    vec![
        harness::bearer(token),
        ("content-type", "application/octet-stream".to_owned()),
        (
            "content-disposition",
            "attachment; filename=\"conformance.txt\"".to_owned(),
        ),
    ]
}

const CONTRACT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../contracts/native-api/openapi.yaml"
);

/// The base URI the document is registered under for `$ref` assembly.
const BASE: &str = "https://spotledger.test/native-api/v0.1";

/// A JSON pointer segment escaped per RFC 6901 (`/` → `~1`, `~` → `~0`).
fn pointer_segment(raw: &str) -> String {
    raw.replace('~', "~0").replace('/', "~1")
}

fn parse_root() -> Json {
    let text = std::fs::read_to_string(CONTRACT).must();
    serde_norway::from_str(&text).must()
}

/// Contract operations the host does not serve yet, by operationId: the contract's `/api/v1`
/// operations minus every operation a router serves (`served_operations()`: the hand-wired
/// routes plus the `SERVES` of every registered route group). It is computed, never edited: a
/// task that serves an operation declares it in its route file's `SERVES` and the operation
/// leaves the gap by itself, and then needs a case in `case()` below (the walk fails without
/// one). An entry is a named, reviewed gap, not a skip.
/// History: `lookup_search` arrived with contract 0.2.0 (ADR-068 §3.5) and was served by
/// WP-PLT-05.T4. Contract 0.3.0 (WP-CON-03.T4) added the six operations a metadata-driven form
/// needs beyond load and save, 0.4.0 (WP-CON-03.T5) the report catalogue, field reveal, bulk
/// export and the named-consumer event stream, 0.7.0 `print_get` (served by WP-PLT-13.T2) and
/// 0.10.0 (WP-CON-03.T13) `notification_list` (served by WP-CON-03.T14/T64) and
/// `auth_consumer_renew` (served by the public group of WP-PRD-01.T49). The eight event stream
/// operations of 0.4.0 are served by the group WP-CON-03.T65 registered, so none of them is a gap.
fn not_yet_served(root: &Json) -> std::collections::BTreeSet<String> {
    let served = spot_http::api::served_operations();
    api_operations(root)
        .into_iter()
        .map(|(_, _, id)| id)
        .filter(|id| !served.contains(id.as_str()))
        .collect()
}

/// The `/api/v1` operations of the contract (path, method, operationId), realtime excluded.
fn api_operations(root: &Json) -> Vec<(String, String, String)> {
    let paths = root["paths"].as_object().must();
    let mut out = Vec::new();
    for (path, item) in paths {
        if !path.starts_with("/api/v1") || path == "/api/v1/realtime" {
            continue;
        }
        for (method, op) in item.as_object().must() {
            if !matches!(method.as_str(), "get" | "put" | "post" | "patch" | "delete") {
                continue;
            }
            out.push((
                path.clone(),
                method.to_ascii_uppercase(),
                op["operationId"].as_str().must().to_owned(),
            ));
        }
    }
    out.sort();
    out
}

/// The JSON pointer of `responses[status]`'s JSON-schema, when that response carries one
/// (either directly under `content`, or through a `$ref` into `components/responses`).
fn schema_pointer_for(path: &str, method: &str, status: &str, response: &Json) -> Option<String> {
    if response
        .pointer("/content/application~1json/schema")
        .is_some()
    {
        return Some(format!(
            "/paths/{}/{}/responses/{}/content/application~1json/schema",
            pointer_segment(path),
            method.to_lowercase(),
            status,
        ));
    }
    let reference = response["$ref"].as_str()?;
    let target = reference.strip_prefix("#/")?;
    Some(format!("/{target}/content/application~1json/schema"))
}

/// The URI fragment form of a JSON pointer: braces and spaces must be percent-encoded for a
/// `$ref` URI, and the resolver decodes them back before walking the pointer.
fn uri_fragment(pointer: &str) -> String {
    pointer
        .replace('{', "%7B")
        .replace('}', "%7D")
        .replace(' ', "%20")
}

/// The (prebuilt) response-schema validators of the contract: per (path, method, status) the
/// pointer of that response's JSON schema, wrapped for `$ref` resolution through the registry.
fn response_validators(
    root: &Json,
) -> std::collections::BTreeMap<(String, String, String), jsonschema::Validator> {
    let registry = jsonschema::Registry::new()
        .add(BASE, root.clone())
        .must()
        .prepare()
        .must();
    let mut out = std::collections::BTreeMap::new();
    for (path, method, _) in api_operations(root) {
        let op = &root["paths"][path.as_str()][method.to_lowercase().as_str()];
        let statuses = op["responses"].as_object().must();
        for (status, response) in statuses {
            if status == "default" {
                continue;
            }
            // The schema to validate against: the response's own content schema, or the one
            // a `$ref` into `components/responses` points at.
            let Some(pointer) = schema_pointer_for(&path, &method, status, response) else {
                continue; // no JSON body schema (a bare 204 or 304)
            };
            if root.pointer(&pointer).is_none() {
                // The referenced response carries no JSON content (e.g. a 304): nothing to validate.
                continue;
            }
            let schema = json!({ "$ref": format!("{BASE}#{}", uri_fragment(&pointer)) });
            let validator = jsonschema::options()
                .with_registry(&registry)
                .build(&schema)
                .must();
            out.insert((path.clone(), method.clone(), status.clone()), validator);
        }
    }
    out
}

/// The resources shared by the operation cases. Independent documents for the cases that
/// destroy or move versions, so the sorted walk is order-safe.
struct Kit {
    token: String,
    customer: String,
    /// Own customer for `lifecycle_rename` (only its own version moves).
    rename_customer: String,
    /// Own customer for `resource_delete` (nothing links to it).
    deletable_customer: String,
    /// Draft invoice for `lifecycle_post` and `document_method_call`.
    draft_invoice: String,
    /// Posted invoice for `lifecycle_cancel`.
    cancel_invoice: String,
    /// Posted invoice for `lifecycle_correct`.
    correct_invoice: String,
    /// The version the two posted invoices sit at after seeding (2).
    posted_version: u64,
    /// The renewal cookie of a seeded sign-in, for `auth_consumer_renew` (set by the walk).
    renew_cookie: String,
}

/// Seeds everything the cases need; `auth_login` is validated here as the first case.
async fn kit(app: &App) -> Kit {
    let alice = TOKEN_ALICE;
    let login = harness::call(
        app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(serde_json::json!({ "user": "alice@acme.com", "password": "correct horse" })),
    )
    .await;
    login.expect_status(200, "auth_login case");
    let token = login.body["data"]["token"].as_str().must().to_owned();
    let customer = harness::seed_customer(app, alice, "ACME conformance").await;
    customer.expect_status(201, "seed customer");
    let rename_customer = harness::seed_customer(app, alice, "ACME rename me").await;
    rename_customer.expect_status(201, "seed rename customer");
    let deletable = harness::seed_customer(app, alice, "ACME delete me").await;
    deletable.expect_status(201, "seed deletable customer");
    let item =
        harness::create_doc(app, alice, "item", serde_json::json!({ "name": "Item X" })).await;
    item.expect_status(201, "seed item");
    let invoice_body = |name: &str| {
        serde_json::json!({
            "name": name,
            "customer": customer.doc_id(),
            "items": [ { "item": item.doc_id(), "qty": "2.00", "rate": "3.50" } ]
        })
    };
    let draft = harness::create_doc(app, alice, "sales_invoice", invoice_body("SINV-K1")).await;
    draft.expect_status(201, "seed draft invoice");
    let cancel = harness::create_doc(app, alice, "sales_invoice", invoice_body("SINV-K2")).await;
    cancel.expect_status(201, "seed cancel invoice");
    let correct = harness::create_doc(app, alice, "sales_invoice", invoice_body("SINV-K3")).await;
    correct.expect_status(201, "seed correct invoice");
    let posted_cancel = harness::post_doc(
        app,
        alice,
        "sales_invoice",
        &cancel.doc_id(),
        &[harness::if_match(cancel.doc_version())],
    )
    .await;
    posted_cancel.expect_status(200, "post the cancel invoice");
    let posted_correct = harness::post_doc(
        app,
        alice,
        "sales_invoice",
        &correct.doc_id(),
        &[harness::if_match(correct.doc_version())],
    )
    .await;
    posted_correct.expect_status(200, "post the correct invoice");
    Kit {
        token,
        customer: customer.doc_id(),
        rename_customer: rename_customer.doc_id(),
        deletable_customer: deletable.doc_id(),
        draft_invoice: draft.doc_id(),
        cancel_invoice: cancel.doc_id(),
        correct_invoice: correct.doc_id(),
        posted_version: posted_cancel.doc_version(),
        renew_cookie: String::new(),
    }
}

/// The one valid request for an operation. Exhaustive over the contract's operationIds: an
/// unmatched operation is a test failure, not a skip.
async fn case(app: &App, kit: &Kit, operation_id: &str, method: &str) -> harness::Call {
    let login = |user: &str, password: &str| {
        harness::call(
            app,
            "POST",
            "/api/v1/auth/login",
            &[],
            Some(serde_json::json!({ "user": user, "password": password })),
        )
    };
    match (method, operation_id) {
        // As BOB, so alice's (the kit's) session survives this case.
        ("POST", "auth_login") => login("bob@acme.com", "battery staple").await,
        ("POST", "auth_logout") => harness::call(app, "POST", "/api/v1/auth/logout", &[harness::bearer(support::fake::TOKEN_BOB)], None).await,
        ("GET", "auth_session") => harness::call(app, "GET", "/api/v1/auth/session", &[harness::bearer(&kit.token)], None).await,
        ("GET", "meta_get") => harness::call(app, "GET", "/api/v1/meta/customer", &[harness::bearer(&kit.token)], None).await,
        ("GET", "resource_list") => harness::call(app, "GET", "/api/v1/resource/customer", &[harness::bearer(&kit.token)], None).await,
        ("POST", "resource_create") => harness::call(app, "POST", "/api/v1/resource/customer", &[harness::bearer(&kit.token)], Some(serde_json::json!({ "name": "ACME created by conformance" }))).await,
        ("POST", "resource_query") => harness::call(app, "POST", "/api/v1/resource/customer/query", &[harness::bearer(&kit.token)], Some(serde_json::json!({ "filter": { "field": "name", "op": "prefix", "value": "ACME" }, "limit": 5 }))).await,
        ("GET", "resource_get") => harness::call(app, "GET", &format!("/api/v1/resource/customer/{}", kit.customer), &[harness::bearer(&kit.token)], None).await,
        ("PATCH", "resource_update") => harness::call(app, "PATCH", &format!("/api/v1/resource/customer/{}", kit.customer), &[harness::bearer(&kit.token), harness::if_match(1)], Some(serde_json::json!({ "category": "conformance" }))).await,
        ("DELETE", "resource_delete") => harness::call(app, "DELETE", &format!("/api/v1/resource/customer/{}", kit.deletable_customer), &[harness::bearer(&kit.token), harness::if_match(1)], None).await,
        ("POST", "lifecycle_post") => harness::call(app, "POST", &format!("/api/v1/resource/sales_invoice/{}/post", kit.draft_invoice), &[harness::bearer(&kit.token), harness::if_match(1)], None).await,
        ("POST", "lifecycle_cancel") => harness::call(app, "POST", &format!("/api/v1/resource/sales_invoice/{}/cancel", kit.cancel_invoice), &[harness::bearer(&kit.token), harness::if_match(kit.posted_version)], None).await,
        ("POST", "lifecycle_correct") => harness::call(app, "POST", &format!("/api/v1/resource/sales_invoice/{}/correct", kit.correct_invoice), &[harness::bearer(&kit.token), harness::if_match(kit.posted_version)], None).await,
        ("POST", "lifecycle_rename") => harness::call(app, "POST", &format!("/api/v1/resource/customer/{}/rename", kit.rename_customer), &[harness::bearer(&kit.token), harness::if_match(1)], Some(serde_json::json!({ "name": "CUST-CONF-1" }))).await,
        ("POST", "document_method_call") => harness::call(app, "POST", &format!("/api/v1/resource/sales_invoice/{}/methods/totals", kit.draft_invoice), &[harness::bearer(&kit.token), harness::if_match(1)], Some(serde_json::json!({ "args": {} }))).await,
        ("POST", "function_call") => harness::call(app, "POST", "/api/v1/method/spot/sales/fn", &[harness::bearer(&kit.token)], Some(serde_json::json!({ "args": {} }))).await,
        ("GET", "function_call_get") => harness::call(app, "GET", "/api/v1/method/spot/sales/fn", &[harness::bearer(&kit.token)], None).await,
        ("POST", "report_run") => harness::call(app, "POST", "/api/v1/report/totals_by_customer/run", &[harness::bearer(&kit.token)], Some(serde_json::json!({ "filters": {}, "limit": 5 }))).await,
        ("GET", "auth_permissions") => harness::call(app, "GET", "/api/v1/auth/permissions?doctype=customer&doctype=sales_invoice", &[harness::bearer(&kit.token)], None).await,
        ("POST", "field_reveal") => {
            let id = spot_http::api::codec::parse_record_id(&kit.customer).must();
            app.reveal.grant(&kit.token);
            app.reveal.add_document(id);
            app.reveal.allow_field("customer", "phone", json!("0300-1234567"));
            harness::call(app, "POST", &format!("/api/v1/resource/customer/{}/reveal", kit.customer), &[harness::bearer(&kit.token)], Some(json!({ "field": "phone", "reason": "Customer asked on ticket 4521." }))).await
        }
        ("POST", "file_upload") => raw_call(app, "POST", "/api/v1/files", &upload_headers(&kit.token), b"conformance bytes, not JSON").await,
        ("GET", "file_get") => {
            let up = raw_call(app, "POST", "/api/v1/files", &upload_headers(&kit.token), b"conformance bytes, not JSON").await;
            up.expect_status(201, "upload for file_get");
            raw_call(app, "GET", &format!("/api/v1/files/{}", up.doc_id()), &[harness::bearer(&kit.token)], b"").await
        }
        ("GET", "print_get") => raw_call(app, "GET", &format!("/api/v1/print/customer/{}", kit.customer), &[harness::bearer(&kit.token)], b"").await,
        ("POST", "lookup_search") => harness::call(app, "POST", "/api/v1/lookup/customer/name", &[harness::bearer(&kit.token)], Some(serde_json::json!({ "text": "ACME" }))).await,
        ("GET", "notification_list") => harness::call(app, "GET", "/api/v1/notifications?limit=2", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("GET", "report_list") => harness::call(app, "GET", "/api/v1/report", &[harness::bearer(&kit.token)], None).await,
        ("GET", "report_describe") => harness::call(app, "GET", &format!("/api/v1/report/{CONFORMANCE_REPORT}"), &[harness::bearer(&kit.token)], None).await,
        ("POST", "auth_consumer_renew") => harness::call(app, "POST", "/api/v1/auth/consumer/renew", &[("cookie", format!("__Host-spot_consumer_renew={}", kit.renew_cookie))], None).await,
        ("GET", "event_consumer_list") => {
            put_consumer(app, "conf_list").await;
            harness::call(app, "GET", "/api/v1/events/consumers", &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("PUT", "event_consumer_put") => harness::call(app, "PUT", "/api/v1/events/consumers/conf_put", &[harness::bearer(TOKEN_ALICE)], Some(consumer_spec())).await,
        ("GET", "event_consumer_get") => {
            put_consumer(app, "conf_get").await;
            harness::call(app, "GET", "/api/v1/events/consumers/conf_get", &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("DELETE", "event_consumer_delete") => {
            put_consumer(app, "conf_delete").await;
            harness::call(app, "DELETE", "/api/v1/events/consumers/conf_delete", &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("POST", "event_consumer_pull") => {
            put_consumer(app, "conf_pull").await;
            harness::call(app, "POST", "/api/v1/events/consumers/conf_pull/pull", &[harness::bearer(TOKEN_ALICE)], Some(json!({"partition": 0, "limit": 10}))).await
        }
        ("POST", "event_consumer_ack") => {
            let lease = pulled_consumer(app, "conf_ack").await;
            harness::call(app, "POST", "/api/v1/events/consumers/conf_ack/ack", &[harness::bearer(TOKEN_ALICE), ("x-consumer-lease", lease)], Some(json!({"partition": 0, "position": EVENT_POSITION}))).await
        }
        ("POST", "event_consumer_restart") => {
            let lease = pulled_consumer(app, "conf_restart").await;
            harness::call(app, "POST", "/api/v1/events/consumers/conf_restart/restart", &[harness::bearer(TOKEN_ALICE), ("x-consumer-lease", lease)], Some(json!({"partition": 0, "from": {"kind": "from_beginning"}}))).await
        }
        ("GET", "event_stream") => {
            put_consumer(app, "conf_stream").await;
            let call = harness::call(app, "GET", "/api/v1/events/stream?consumer=conf_stream&partition=0", &[harness::bearer(TOKEN_ALICE)], None).await;
            let content_type = call.header("content-type").unwrap_or_default().to_owned();
            assert!(content_type.starts_with("text/event-stream"), "event_stream answers an SSE content type, got {content_type:?}");
            assert!(call.raw.starts_with("id: v1."), "event_stream: the first frame starts with its id line, got {:?}", call.raw);
            call
        }
        other => panic!(
            "the contract has an operation this suite has no case for: {other:?} \
             (every /api/v1 operation except realtime needs a case)"
        ),
    }
}

/// The registered event stream group: a consumer is its creator's alone (bob gets the one
/// answer for a consumer that does not exist), an unknown consumer is `404`, and a second
/// stream for a consumer whose lease is held is the contract's `409`.
#[tokio::test]
async fn the_registered_event_stream_is_scoped_and_refuses_a_second_holder() {
    let (app, _) = app_with_renewal();
    assert!(
        spot_http::api::served_operations().contains("event_stream"),
        "the events group is registered"
    );
    put_consumer(&app, "conf_scope").await;
    let bob = harness::bearer(support::fake::TOKEN_BOB);
    for (method, path) in [
        ("GET", "/api/v1/events/consumers/conf_scope"),
        ("DELETE", "/api/v1/events/consumers/conf_scope"),
        ("GET", "/api/v1/events/consumers/conf_nobody"),
    ] {
        let call = harness::call(&app, method, path, std::slice::from_ref(&bob), None).await;
        call.expect_status(404, path);
        assert_eq!(
            call.body["error"]["code"], "eventstream.consumer_not_found",
            "{path}"
        );
    }
    let list = harness::call(
        &app,
        "GET",
        "/api/v1/events/consumers",
        std::slice::from_ref(&bob),
        None,
    )
    .await;
    list.expect_status(200, "bob lists his consumers");
    assert_eq!(
        list.body["data"].as_array().must().len(),
        0,
        "alice's consumer is not in bob's list"
    );
    let unknown = harness::call(
        &app,
        "GET",
        "/api/v1/events/consumers/conf_nobody",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    unknown.expect_status(404, "unknown consumer");
    // A pull takes the lease; a stream for the held consumer is the conflict.
    pulled_consumer(&app, "conf_held").await;
    let busy = harness::call(
        &app,
        "GET",
        "/api/v1/events/stream?consumer=conf_held&partition=0",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    busy.expect_status(409, "stream for a held consumer");
    assert_eq!(busy.body["error"]["code"], "eventstream.consumer_busy");
    // Without a credential nothing is served.
    let anonymous = harness::call(&app, "GET", "/api/v1/events/consumers", &[], None).await;
    anonymous.expect_status(401, "no credential");
}

#[tokio::test]
async fn every_api_v1_operation_answers_within_the_contract() {
    let root = parse_root();
    let validators = response_validators(&root);
    let (app, renew_cookie) = app_with_renewal();
    let mut kit = kit(&app).await;
    kit.renew_cookie = renew_cookie;
    let operations = api_operations(&root);
    assert!(
        operations.len() >= 18,
        "the expected /api/v1 operations are found: {}",
        operations.len()
    );
    let gap = not_yet_served(&root);
    for (path, method, operation_id) in operations {
        if gap.contains(&operation_id) {
            continue;
        }
        let call = case(&app, &kit, &operation_id, &method).await;
        let what = format!("{operation_id} ({method} {path})");
        assert!(
            call.header("x-request-id").is_some(),
            "{what}: every response carries X-Request-Id"
        );
        let responses = root["paths"][path.as_str()][method.to_lowercase().as_str()]["responses"]
            .as_object()
            .must();
        let status_key = call.status.to_string();
        let Some(response) = responses.get(&status_key) else {
            // The default error is the only thing a valid request may fall back to.
            panic!(
                "{what}: status {} is not listed for the operation; the contract lists {:?} \
                 (a valid request must answer a listed status)",
                status_key,
                responses.keys().collect::<Vec<_>>()
            );
        };
        let Some(pointer) = schema_pointer_for(&path, &method, &status_key, response) else {
            assert!(
                call.body.is_null(),
                "{what}: a {status_key} response has no body schema; got {}",
                call.raw
            );
            continue;
        };
        let validator = validators
            .get(&(path.clone(), method.clone(), status_key.clone()))
            .unwrap_or_else(|| panic!("{what}: no validator for {status_key} at {pointer}"));
        if let Err(errors) = validator.validate(&call.body) {
            panic!(
                "{what}: the response body does not match the contract schema ({pointer}):\n{errors}\n{}",
                call.raw
            );
        }
    }
}

/// Validates one response body against the contract schema of (path, method, status).
fn assert_matches_contract(
    validators: &std::collections::BTreeMap<(String, String, String), jsonschema::Validator>,
    path: &str,
    method: &str,
    call: &harness::Call,
) {
    let key = (path.to_owned(), method.to_owned(), call.status.to_string());
    let validator = validators
        .get(&key)
        .unwrap_or_else(|| panic!("no validator for {key:?}"));
    if let Err(errors) = validator.validate(&call.body) {
        panic!(
            "{key:?} does not match the contract:\n{errors}\n{}",
            call.raw
        );
    }
}

/// Contract 0.5.0, both directions: with every new property present the responses still
/// validate against the schemas, and with none present nothing new appears.
#[tokio::test]
async fn the_0_5_0_properties_validate_and_are_absent_when_unused() {
    use spot_http::api::lookup::{LookupColumn, LookupPage, LookupRow};
    use spot_http::api::meta::ControlHint;
    use support::fake::{LookupOverride, MetaTweak};

    let root = parse_root();
    let validators = response_validators(&root);
    let app = App::new();
    let token = TOKEN_ALICE;
    let customer = harness::seed_customer(&app, token, "ACME five").await;
    customer.expect_status(201, "seed");
    let id = customer.doc_id();

    // Absent: nothing new on the wire.
    let plain = harness::get_doc(&app, token, "customer", &id).await;
    assert!(plain.body["data"].get("_currency").is_none());
    let meta = harness::get_meta(&app, token, "customer", &[]).await;
    assert!(!meta.raw.contains("\"multiple\"") && !meta.raw.contains("\"control\""));

    // Present.
    app.fake.set_currency("name", "USD", "$");
    app.fake.tweak_meta(MetaTweak {
        doctype: "customer".into(),
        field: "category".into(),
        multiple: true,
        control: Some(ControlHint::Chips),
    });
    let doc = harness::get_doc(&app, token, "customer", &id).await;
    assert_eq!(doc.body["data"]["_currency"]["name"]["symbol"], "$");
    assert_matches_contract(&validators, "/api/v1/resource/{doctype}/{id}", "GET", &doc);
    let meta = harness::get_meta(&app, token, "customer", &[]).await;
    let fields = meta.body["data"]["fields"].as_array().must().clone();
    let category = fields.iter().find(|f| f["name"] == "category").must();
    assert_eq!(category["multiple"], true);
    assert_eq!(category["hints"]["control"], "chips");
    assert_matches_contract(&validators, "/api/v1/meta/{doctype}", "GET", &meta);

    let page = LookupPage {
        rows: vec![LookupRow {
            id: spot_http::api::codec::parse_record_id(&id).must(),
            label: "ACME".into(),
            description: None,
            extras: Default::default(),
            cells: [("city".to_owned(), "Karachi".to_owned())]
                .into_iter()
                .collect(),
        }],
        has_more: false,
        cursor: None,
        columns: vec![LookupColumn {
            name: "city".into(),
            label: "City".into(),
        }],
    };
    app.fake
        .register_lookup_override(support::fake::SEAM_FIXED, LookupOverride::Static(page));
    let picked = harness::call(
        &app,
        "POST",
        "/api/v1/lookup/sales_invoice/customer",
        &[harness::bearer(token)],
        Some(json!({ "text": "" })),
    )
    .await;
    picked.expect_status(200, "lookup with columns");
    assert_eq!(picked.body["data"]["columns"][0]["name"], "city");
    assert_eq!(picked.body["data"]["rows"][0]["cells"]["city"], "Karachi");
    assert_matches_contract(
        &validators,
        "/api/v1/lookup/{doctype}/{field}",
        "POST",
        &picked,
    );

    let perms = harness::call(
        &app,
        "GET",
        "/api/v1/auth/permissions",
        &[harness::bearer(token)],
        None,
    )
    .await;
    perms.expect_status(200, "permissions");
    assert_eq!(
        perms.body["data"]["doctypes"][0]["fields"]["revealable"][0],
        "phone"
    );
    assert_eq!(
        perms.body["data"]["doctypes"][1]["attach_limits"]["scan"]["max_bytes"],
        1_048_576
    );
    assert_matches_contract(&validators, "/api/v1/auth/permissions", "GET", &perms);
}

/// A validator for one component schema of the contract.
fn component_validator(root: &Json, name: &str) -> jsonschema::Validator {
    let registry = jsonschema::Registry::new()
        .add(BASE, root.clone())
        .must()
        .prepare()
        .must();
    let schema = json!({ "$ref": format!("{BASE}#/components/schemas/{name}") });
    jsonschema::options()
        .with_registry(&registry)
        .build(&schema)
        .must()
}

/// Contract 0.6.0 (WP-CON-03.T7), additive: the rule expressions, `expr_version` and the
/// provisional `provides` fills are all optional, bounded and closed where they must be. No
/// service emits them yet (the metadata compiler is WP-MET-02), so this checks the schemas: every
/// 0.5.0 payload still validates, the new properties validate, and a bound or a type error is
/// refused.
#[test]
fn the_0_6_0_properties_are_optional_bounded_and_additive() {
    let root = parse_root();
    // The contract is at 0.11.1; the 0.6.0 properties must still hold unchanged.
    assert_eq!(root["info"]["version"], "0.11.1");
    let meta = component_validator(&root, "DocTypeMeta");
    let row = component_validator(&root, "LookupRow");
    let request = component_validator(&root, "LookupRequest");

    // A 0.5.0 DocTypeMeta (no expression, no `expr_version`) is unchanged.
    let old = json!({
        "doctype": "customer", "label": "Customer", "version": 3,
        "fields": [{"name": "title", "label": "Title", "type": "text"}]
    });
    assert!(
        meta.is_valid(&old),
        "{:?}",
        meta.iter_errors(&old).collect::<Vec<_>>()
    );

    // With every new property.
    let new = json!({
        "doctype": "sales_invoice", "label": "Sales Invoice", "version": 7, "expr_version": 1,
        "fields": [{
            "name": "reason", "label": "Reason", "type": "text",
            "depends_on": "doc.discount > 0",
            "mandatory_depends_on": "doc.discount > 10",
            "read_only_depends_on": "doc.status == 'Paid'",
            "default_expr": "'None'"
        }]
    });
    assert!(
        meta.is_valid(&new),
        "{:?}",
        meta.iter_errors(&new).collect::<Vec<_>>()
    );

    // An expression is a bounded string; `expr_version` is an integer of at least 1.
    for (pointer, bad) in [
        ("/fields/0/depends_on", json!("x".repeat(1025))),
        ("/fields/0/depends_on", json!(5)),
        ("/fields/0/default_expr", json!(["a"])),
        ("/expr_version", json!(0)),
        ("/expr_version", json!("1")),
    ] {
        let mut doc = new.clone();
        if let Some(slot) = doc.pointer_mut(pointer) {
            *slot = bad;
        }
        assert!(!meta.is_valid(&doc), "{pointer} must be refused");
    }

    // The lookup row: `provides` is optional and carries encoded values.
    let plain = json!({"id": "0192f3c4-0000-7000-8000-00000000c009", "label": "Acme"});
    assert!(row.is_valid(&plain));
    let provides = json!({
        "id": "0192f3c4-0000-7000-8000-00000000c009", "label": "Acme",
        "provides": {"customer_name": "Acme", "credit_limit": "25000.00", "active": true}
    });
    assert!(row.is_valid(&provides));
    let mut bad = provides.clone();
    bad["provides"] = json!("Acme");
    assert!(!row.is_valid(&bad));

    // The request stays closed: the server applies the filter, the client sends none.
    assert!(request.is_valid(&json!({"text": "ac", "doc": {"a": 1}, "parent": {}})));
    assert!(!request.is_valid(&json!({"text": "ac", "filters": {"active": true}})));
    assert!(!request.is_valid(&json!({"text": "ac", "filter": "doc.a == 1"})));
}

/// Contract 0.7.0 (WP-CON-03.T9), additive: `print_get` is a read-only `GET` that answers a PDF
/// and nothing else. WP-PLT-13.T2 serves it (the walk above has its case); this test pins the
/// shape the operation must keep.
#[test]
fn the_0_7_0_print_operation_is_a_read_only_get_that_answers_a_pdf() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.11.1");
    assert_eq!(
        root["x-spot-error-codes"].as_object().must().len(),
        62,
        "the registry counts the two print codes and, from 0.10.0, partition.not_owner"
    );
    assert!(root["x-spot-error-codes"]["print.format_unknown"].is_array());
    assert_eq!(root["x-spot-error-codes"]["print.render_failed"][0], 500);

    let path = "/api/v1/print/{doctype}/{id}";
    let item = root["paths"][path].as_object().must();
    assert_eq!(
        item.keys().map(|key| key.as_str()).collect::<Vec<_>>(),
        vec!["get", "parameters"],
        "printing is read-only: the path answers GET only"
    );
    let op = &item["get"];
    assert_eq!(op["operationId"], "print_get");
    assert!(
        op.get("requestBody").is_none(),
        "a read-only operation takes no request body"
    );

    // `format` is the only query parameter; the path parameters are the DocType and the id.
    let query = op["parameters"].as_array().must();
    assert_eq!(query.len(), 1, "one query parameter");
    assert_eq!(query[0]["name"], "format");
    assert_eq!(query[0]["in"], "query");
    assert_eq!(
        query[0]["required"], false,
        "omitted means the default format"
    );

    // The success response is a PDF, and only a PDF.
    let responses = op["responses"].as_object().must();
    assert_eq!(responses.len(), 2, "the 200 and the shared error response");
    assert_eq!(responses["default"]["$ref"], "#/components/responses/Error");
    let ok = &responses["200"];
    let content = ok["content"].as_object().must();
    assert_eq!(
        content.keys().map(|key| key.as_str()).collect::<Vec<_>>(),
        vec!["application/pdf"],
        "a print returns a PDF and nothing else"
    );
    let headers = ok["headers"].as_object().must();
    assert_eq!(
        headers["Content-Type"]["schema"]["const"],
        "application/pdf"
    );
    assert_eq!(
        headers["Cache-Control"]["schema"]["const"],
        "private, no-store"
    );
    let disposition = headers["Content-Disposition"]["description"]
        .as_str()
        .must();
    assert!(
        disposition.contains("inline; filename=\"<doctype>-<id>.pdf\""),
        "the filename is built from the validated path names: {disposition}"
    );
    for header in headers.keys() {
        let lower = header.to_ascii_lowercase();
        assert!(
            lower != "etag" && lower != "accept-ranges" && !lower.contains("range"),
            "this version has no ETag and no Range: {header}"
        );
    }

    // It is served: no longer a named gap, so the walk above exercises it.
    assert!(
        !not_yet_served(&parse_root()).contains("print_get"),
        "print_get is served since WP-PLT-13.T2"
    );
}

/// Contract 0.9.0 (ADR-083, ADR-068 amendment A4), additive: `create_only` on a field, the
/// `consumer_ref` field type, and the `consumer` event actor. A 0.8.0 payload still validates,
/// and nothing in this version removes or reshapes a 0.8.0 property.
#[test]
fn the_0_9_0_properties_are_optional_and_additive() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.11.1");
    let field = component_validator(&root, "FieldMeta");
    let kind = component_validator(&root, "FieldType");

    // A 0.8.0 field (no `create_only`) is unchanged, and the new property is a boolean.
    let plain = json!({"name": "user_name", "label": "User Name", "type": "text"});
    assert!(field.is_valid(&plain));
    let mut once = plain.clone();
    once["create_only"] = json!(true);
    assert!(field.is_valid(&once));
    once["create_only"] = json!("yes");
    assert!(!field.is_valid(&once), "create_only is a boolean");

    // The new type is in the enumeration; every 0.8.0 type still is.
    assert!(kind.is_valid(&json!("consumer_ref")));
    for old in [
        "bool",
        "int",
        "decimal",
        "text",
        "link",
        "attach",
        "table_multiselect",
    ] {
        assert!(kind.is_valid(&json!(old)), "{old}");
    }
    assert!(!kind.is_valid(&json!("principal")));

    // The three actor kinds of an event envelope are those of `Actor` in contracts/events, and
    // the 0.8.0 two are still there.
    let kinds = root["components"]["schemas"]["EventEnvelope"]["properties"]["meta"]["properties"]
        ["actor"]["properties"]["kind"]["enum"]
        .as_array()
        .must();
    let kinds: Vec<&str> = kinds.iter().filter_map(Json::as_str).collect();
    assert_eq!(kinds, vec!["user", "consumer", "system"]);

    // 0.9.0 added no operation and no error code: 0.10.0 added `partition.not_owner` (the 62nd).
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 62);
}

/// Contract 0.10.0 (WP-CON-03.T13), additive: `partition.not_owner`, the notice feed and the
/// consumer session renewal. The two operations are not served yet (NOT_YET_SERVED); this test
/// pins the shape a server must keep.
#[test]
fn the_0_10_0_additions_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.11.1");
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 62);
    assert_eq!(root["x-spot-error-codes"]["partition.not_owner"][0], 403);
    assert_eq!(root["x-spot-error-codes"]["partition.not_owner"][1], false);
    // Every 0.9.0 code is still there (the 61 are the 62 less the new one).
    for old in [
        "print.format_unknown",
        "print.render_failed",
        "auth.session_expired",
    ] {
        assert!(root["x-spot-error-codes"][old].is_array(), "{old}");
    }

    let feed = &root["paths"]["/api/v1/notifications"]["get"];
    assert_eq!(feed["operationId"], "notification_list");
    let mut names: Vec<&str> = feed["parameters"]
        .as_array()
        .must()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["after", "limit"],
        "after and limit are the only parameters"
    );
    let page = component_validator(&root, "NotificationPage");
    let item = json!({"event_id": "e1", "event": "user_notice", "data": {"title": "x"}, "created_at": "2026-10-31T08:15:00Z"});
    assert!(page.is_valid(&json!({"items": [item.clone()], "next": "e1"})));
    assert!(
        page.is_valid(&json!({"items": []})),
        "an empty page has no next"
    );
    assert!(!page.is_valid(&json!({"items": [{"event_id": "e1"}]})));

    let renew = &root["paths"]["/api/v1/auth/consumer/renew"]["post"];
    assert_eq!(renew["operationId"], "auth_consumer_renew");
    assert!(
        renew.get("requestBody").is_none(),
        "the cookie is the only credential"
    );
    assert_eq!(renew["security"], json!([]), "no bearer token is needed");
    let session = component_validator(&root, "ConsumerSession");
    assert!(session.is_valid(&json!({"token": "t", "expires_at": "2026-10-31T09:15:00Z"})));
    assert!(!session.is_valid(&json!({"token": "t"})));

    // The feed and the renewal are served (their cases are in the walk).
    let gap = not_yet_served(&root);
    let registered = spot_http::api::router::routes::registry_serves();
    assert!(
        !gap.contains("notification_list") && registered.contains("notification_list"),
        "notification_list is served by the registered notices group"
    );
    assert!(
        !gap.contains("auth_consumer_renew") && registered.contains("auth_consumer_renew"),
        "auth_consumer_renew is served by the registered public renewal group"
    );
}

/// The registered feed is guarded like every registered group: no credential and a credential
/// the site does not know are `401`; it answers only for the caller (bob's notice never reaches
/// alice), and it is never cacheable.
#[tokio::test]
async fn the_registered_notice_feed_is_authenticated_scoped_and_private() {
    let app = app_with_files();
    let ids = |call: &harness::Call| -> Vec<String> {
        call.body["items"]
            .as_array()
            .must()
            .iter()
            .map(|i| i["event_id"].as_str().must().to_owned())
            .collect()
    };
    let none = harness::call(&app, "GET", "/api/v1/notifications", &[], None).await;
    none.expect_status(401, "no credential");
    let bad = harness::call(
        &app,
        "GET",
        "/api/v1/notifications",
        &[harness::bearer("not-a-token")],
        None,
    )
    .await;
    bad.expect_status(401, "an unknown token");
    for denied in [&none, &bad] {
        assert!(denied.body.get("items").is_none(), "{}", denied.raw);
    }
    let alice = harness::call(
        &app,
        "GET",
        "/api/v1/notifications",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    alice.expect_status(200, "alice's feed");
    assert_eq!(ids(&alice), ["ev001", "ev002", "ev003"]);
    assert_eq!(alice.header("cache-control"), Some("private, no-store"));
    let bob = harness::call(
        &app,
        "GET",
        "/api/v1/notifications",
        &[harness::bearer(support::fake::TOKEN_BOB)],
        None,
    )
    .await;
    bob.expect_status(200, "bob's feed");
    assert_eq!(ids(&bob), ["ev000"]);
    // An unknown `after` is the contract's request error (`400 api.invalid_request`; the
    // contract lists only the default error shape), and bob's id is the same answer for alice.
    for after in ["nope", "ev000"] {
        let bad_after = harness::call(
            &app,
            "GET",
            &format!("/api/v1/notifications?after={after}"),
            &[harness::bearer(TOKEN_ALICE)],
            None,
        )
        .await;
        bad_after.expect_status(400, "an unknown or foreign after");
        assert_eq!(bad_after.body["error"]["code"], "api.invalid_request");
    }
}

/// The registered report catalogue is guarded like every registered group: no credential is
/// `401` (the port, not the route, decides whether a credential is known); a report the port refuses is `404 report.not_found` (the same
/// answer for one that does not exist), and every answer, success or not, is never cacheable.
#[tokio::test]
async fn the_registered_report_catalogue_is_authenticated_and_private() {
    let app = app_with_files();
    let root = parse_root();
    let gap = not_yet_served(&root);
    let registered = spot_http::api::router::routes::registry_serves();
    for id in ["report_list", "report_describe"] {
        assert!(
            !gap.contains(id) && registered.contains(id),
            "{id} is served by the registered report group"
        );
    }
    let listed = format!("/api/v1/report/{CONFORMANCE_REPORT}");
    for path in ["/api/v1/report", listed.as_str()] {
        let none = harness::call(&app, "GET", path, &[], None).await;
        none.expect_status(401, "no credential");
        assert!(none.body.get("data").is_none(), "{}", none.raw);
        assert!(!none.raw.contains("Hours by task"), "{}", none.raw);
    }
    let list = harness::call(
        &app,
        "GET",
        "/api/v1/report",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    list.expect_status(200, "the catalogue");
    assert_eq!(list.body["data"][0]["name"], CONFORMANCE_REPORT);
    assert_eq!(list.header("cache-control"), Some("private, no-store"));
    let one = harness::call(&app, "GET", &listed, &[harness::bearer(TOKEN_ALICE)], None).await;
    one.expect_status(200, "the report described");
    assert_eq!(one.header("cache-control"), Some("private, no-store"));
    for refused in ["no_such_report", "totals_by_customer"] {
        let gone = harness::call(
            &app,
            "GET",
            &format!("/api/v1/report/{refused}"),
            &[harness::bearer(TOKEN_ALICE)],
            None,
        )
        .await;
        gone.expect_status(404, "a report the port refuses");
        assert_eq!(gone.body["error"]["code"], "report.not_found");
        assert_eq!(gone.header("cache-control"), Some("private, no-store"));
    }
}

/// The gap list is a computation over `SERVES`, so it must be sound: every served id is a real
/// contract operation, no operation is served twice, and what was served stays served.
#[test]
fn the_served_set_is_consistent_with_the_contract() {
    use spot_http::api::router::{HAND_WIRED_SERVES, routes};
    use std::collections::BTreeSet;

    let root = parse_root();
    let contract: BTreeSet<String> = api_operations(&root)
        .into_iter()
        .map(|(_, _, id)| id)
        .collect();

    let registered: Vec<&str> = routes::REGISTRY
        .iter()
        .flat_map(|r| r.serves.iter().copied())
        .collect();
    let mut seen = BTreeSet::new();
    for id in HAND_WIRED_SERVES.iter().copied().chain(registered) {
        assert!(
            contract.contains(id),
            "{id} is served but is not an /api/v1 operation of contracts/native-api/openapi.yaml"
        );
        assert!(seen.insert(id), "{id} is served by two routes or groups");
    }

    // The floor: these were served when the registry arrived and must stay served. It only
    // ever grows (the registry's groups add to the served set, never to this list).
    let gap = not_yet_served(&root);
    for id in [
        "auth_login",
        "auth_logout",
        "auth_session",
        "auth_permissions",
        "meta_get",
        "lookup_search",
        "resource_list",
        "resource_create",
        "resource_query",
        "resource_get",
        "resource_update",
        "resource_delete",
        "field_reveal",
        "lifecycle_post",
        "lifecycle_cancel",
        "lifecycle_correct",
        "lifecycle_rename",
        "document_method_call",
        "function_call",
        "function_call_get",
        "report_run",
        "report_list",
        "report_describe",
        "file_upload",
        "file_get",
        "print_get",
    ] {
        assert!(!gap.contains(id), "{id} was served and must stay served");
    }
    // Everything the contract lists is either served or a named gap, never neither.
    assert_eq!(
        gap.len() + seen.len(),
        contract.len(),
        "served and gap operations partition the contract"
    );
}
