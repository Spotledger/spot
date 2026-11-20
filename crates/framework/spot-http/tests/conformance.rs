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
    let services =
        ApiServices::new(
            fake.clone(),
            fake.clone(),
            fake.clone(),
            fake.clone(),
            store.clone(),
        )
        .with_permissions(permissions.clone())
        .with_reveal(reveal.clone())
        .with_port::<dyn spot_http::api::ports::task::TaskPort>(conformance_tasks())
        .with_files(Arc::new(MemoryFilesPort::new(FILE_LIMIT)))
        .with_print(Arc::new(spot_http::api::print::MemoryPrintPort::open()))
        .with_port::<dyn NoticeSource>(notices_for_conformance())
        .with_port::<dyn spot_http::api::ports::events::EventStreamPort>(Arc::new(
            ConformanceEvents::default(),
        ))
        .with_port::<dyn spot_http::api::ports::report::ReportCatalogPort>(Arc::new(
            ConformanceReports,
        ))
        .with_port::<dyn spot_http::api::ports::session::SessionPort>(Arc::new(
            ConformanceSessions::new(),
        ))
        .with_port::<dyn ConsumerSessions>(sessions)
        .with_port::<dyn spot_http::api::idempotency::IdempotencyStore>(store.clone())
        .with_port::<dyn spot_http::api::ports::process::ProcessPort>(conformance_processes())
        .with_port::<dyn spot_http::api::ports::webhook::WebhookPort>(conformance_webhooks())
        .with_port::<dyn spot_http::api::ports::import::ImportPort>(import_for_conformance(
            clock.clone(),
        ))
        .with_port::<dyn spot_http::api::ports::occupancy::OccupancyPort>(
            occupancy_for_conformance(clock.clone()),
        )
        .with_port::<dyn spot_http::api::ports::graph::GraphPort>(graph_for_conformance())
        .with_port::<dyn spot_http::api::ports::graph::LineagePort>(lineage_for_conformance())
        .with_port::<dyn spot_http::api::ports::process_version::VersionPort>(
            versions_for_conformance(),
        )
        .with_port::<dyn spot_http::api::ports::webhook_secret::WebhookSecretPort>(
            std::sync::Arc::new(ConformanceSecrets),
        )
        .with_port::<dyn spot_http::api::ports::identity::IdentityAdminPort>(
            identity_fake::fake(),
        )
        .with_port::<dyn spot_http::api::ports::identity::IdentityPermissionPort>(
            identity_fake::fake(),
        )
        .with_port::<dyn spot_http::api::ports::identity::IdentityStepUpPort>(
            identity_fake::fake(),
        )
        .with_port::<dyn spot_http::api::ports::identity::IdentityAuditPort>(
            identity_fake::fake(),
        )
        .with_port::<dyn spot_http::api::ports::connectors::ConnectorsPort>(
            conformance_connectors(),
        )
        .with_port::<dyn spot_http::api::ports::apps::AppsPort>(conformance_apps())
        .with_port::<dyn spot_http::api::ports::invitation_accept::InvitationAcceptPort>({
            let port = Arc::new(spot_http::api::ports::invitation_accept::MemoryInvitationAccept::new(
                clock.clone(),
                12,
            ));
            port.invite(
                harness::SITE,
                INVITATION_TOKEN,
                time::OffsetDateTime::now_utc() + time::Duration::days(3),
            );
            port
        })
        .with_port::<dyn spot_http::api::ports::journal_access::JournalAccessPort>(
            conformance_journal(),
        )
        .with_port::<dyn spot_http::api::ports::link_inbound::LinkInboundPort>(Arc::new(
            ConformanceLinkInbound,
        ))
        .with_port::<dyn spot_http::api::ports::sso::SsoPort>(std::sync::Arc::new(
            sso_fake::SsoFake,
        ))
        .with_port::<dyn spot_http::api::ports::sso::SsoAdminPort>(std::sync::Arc::new(
            sso_fake::SsoAdminFake,
        ));
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

/// The conformance process port: alice may start `expense_claim` (the memory port names the
/// caller by the credential's token itself, so the fixed fake-app token is the user).
fn conformance_processes() -> std::sync::Arc<spot_http::api::ports::process::MemoryProcessPort> {
    use spot_http::api::ports::process::{MemoryProcessPort, ProcessDef};
    let port = MemoryProcessPort::new();
    port.define(
        harness::SITE,
        ProcessDef::open("expense_claim", "Expense claim", &["amount"]),
    );
    std::sync::Arc::new(port)
}

/// The file the import cases use (uploaded by alice).
const IMPORT_FILE: &str = "0192f3c4-0000-7000-8000-00000000f001";

/// The import port of the conformance app: alice holds the import right on `customer` and owns
/// one uploaded file of three rows, one of which has an error.
fn import_for_conformance(
    clock: std::sync::Arc<spot_core::ManualClock>,
) -> std::sync::Arc<spot_http::api::ports::import::InMemoryImportPort> {
    use spot_http::api::ports::import::{InMemoryImportPort, MemoryFile, MemoryRowError};
    let port = InMemoryImportPort::new(clock);
    port.grant_token(harness::SITE, TOKEN_ALICE, ALICE_ID);
    port.grant_import(harness::SITE, ALICE_ID, "customer");
    port.add_file(
        harness::SITE,
        ALICE_ID,
        IMPORT_FILE,
        MemoryFile {
            rows: 3,
            columns: 2,
            bytes: 64,
            errors: vec![MemoryRowError {
                row: 2,
                column: Some("name".to_owned()),
                cell: "ACME".to_owned(),
                writable: true,
                code: "import.bad_value".to_owned(),
            }],
        },
    );
    std::sync::Arc::new(port)
}

/// The conformance webhook port: the `orders` trigger accepts [`WEBHOOK_SIGNATURE`].
fn conformance_webhooks() -> std::sync::Arc<spot_http::api::ports::webhook::MemoryWebhookPort> {
    let port = spot_http::api::ports::webhook::MemoryWebhookPort::new();
    port.register(harness::SITE, "orders", WEBHOOK_SIGNATURE);
    std::sync::Arc::new(port)
}

/// The conformance connector port: alice sees `crm`, may invoke `echo` and read its run log.
fn conformance_connectors()
-> std::sync::Arc<spot_http::api::ports::connectors::MemoryConnectorsPort> {
    use spot_http::api::ports::connectors::{
        ConnectorSeed, MemoryConnectorsPort, RunOutcome, RunView,
    };
    let port = MemoryConnectorsPort::new();
    port.add_connector(harness::SITE, ConnectorSeed::new("crm"));
    port.grant(harness::SITE, TOKEN_ALICE, "alice");
    port.grant_runs(harness::SITE, TOKEN_ALICE);
    port.add_run(
        harness::SITE,
        "crm",
        RunView {
            at: "2026-11-12T09:00:00Z".to_owned(),
            operation: "echo".to_owned(),
            outcome: RunOutcome::Ok,
            duration_ms: 5,
            code: None,
        },
    );
    std::sync::Arc::new(port)
}

/// The conformance apps port: alice is an administrator of the harness site and sees one app.
fn conformance_apps() -> std::sync::Arc<spot_http::api::ports::apps::MemoryAppsPort> {
    use spot_http::api::ports::apps::MemoryAppsPort;
    let port = MemoryAppsPort::new();
    port.add_app(harness::SITE, MemoryAppsPort::simple("inventory"));
    port.grant_admin(harness::SITE, TOKEN_ALICE);
    std::sync::Arc::new(port)
}

const WEBHOOK_SIGNATURE: &str = "t=1,v1=goodsignature";

/// Starts an `expense_claim` run as alice and returns its id.
async fn conformance_run(app: &App, key: &str) -> String {
    let reply = harness::call(
        app,
        "POST",
        "/api/v1/process/expense_claim/runs",
        &[
            harness::bearer(TOKEN_ALICE),
            ("idempotency-key", key.to_owned()),
        ],
        Some(json!({"input": {"amount": "10.00"}})),
    )
    .await;
    reply.expect_status(201, "start a run for a conformance case");
    reply.body["data"]["id"].as_str().must().to_owned()
}

/// Starts alice's import dry run under `key`; returns the job's `data`.
async fn import_dry_run(app: &App, key: &str) -> Json {
    let call = harness::call(
        app,
        "POST",
        "/api/v1/import",
        &[harness::bearer(TOKEN_ALICE), harness::idem_key(key)],
        Some(json!({"file_id": IMPORT_FILE, "doctype": "customer", "mode": "insert"})),
    )
    .await;
    call.expect_status(202, "seed an import dry run");
    call.body["data"].clone()
}

/// The occupancy port of the conformance app: alice may claim on the `room_hold` entity.
fn occupancy_for_conformance(
    clock: std::sync::Arc<spot_core::ManualClock>,
) -> std::sync::Arc<spot_http::api::ports::occupancy::InMemoryOccupancyPort> {
    use spot_http::api::ports::occupancy::InMemoryOccupancyPort;
    let port = InMemoryOccupancyPort::new(clock);
    port.grant_token(harness::SITE, TOKEN_ALICE, ALICE_ID);
    port.add_entity(harness::SITE, "room_hold");
    std::sync::Arc::new(port)
}

/// Claims `unit` for a night in 2030 as alice under `key`; returns the hold's `data`.
async fn occupancy_hold(app: &App, unit: &str, key: &str) -> Json {
    let call = harness::call(
        app,
        "POST",
        "/api/v1/occupancy/room_hold/holds",
        &[harness::bearer(TOKEN_ALICE), harness::idem_key(key)],
        Some(json!({"unit": unit, "start": "2030-01-10T14:00:00Z", "end": "2030-01-11T10:00:00Z"})),
    )
    .await;
    call.expect_status(201, "seed an occupancy hold");
    call.body["data"].clone()
}

/// A canonical UUIDv7 for graph record `n`.
fn graph_id(n: u32) -> String {
    format!("0192f3c4-0000-7000-8000-{n:012x}")
}

/// The graph port of the conformance app: `sales_invoice --settled_by--> payment_entry`, readable
/// by alice.
fn graph_for_conformance() -> std::sync::Arc<spot_http::api::ports::graph::MemoryGraph> {
    use spot_http::api::ports::graph::{MemoryGraph, NodeRef};
    let graph = MemoryGraph::new();
    let site = spot_core::SiteId::new(harness::SITE).must();
    graph.add_edge(
        &site,
        NodeRef::new("sales_invoice", graph_id(1)),
        NodeRef::new("payment_entry", graph_id(2)),
        "settled_by",
    );
    graph.grant_all(&site, TOKEN_ALICE);
    std::sync::Arc::new(graph)
}

/// The lineage port of the conformance app: a two-level bill of materials alice may read.
fn lineage_for_conformance() -> std::sync::Arc<spot_http::api::ports::graph::MemoryLineage> {
    use spot_http::api::ports::graph::{MemoryLineage, NodeRef};
    let lineage = MemoryLineage::new();
    let site = spot_core::SiteId::new(harness::SITE).must();
    lineage.add_tree(
        &site,
        NodeRef::new("item", graph_id(10)),
        Some("1"),
        &[(0, NodeRef::new("item", graph_id(11)), Some("2.500"))],
    );
    lineage.grant_all(&site, TOKEN_ALICE);
    std::sync::Arc::new(lineage)
}

/// The process version port of the conformance app: `expense_claim`, which alice publishes,
/// reads and migrates.
fn versions_for_conformance()
-> std::sync::Arc<spot_http::api::ports::process_version::MemoryVersionPort> {
    use spot_http::api::ports::process_version::{Grant, MemoryVersionPort};
    let port = MemoryVersionPort::new();
    port.define_process(harness::SITE, "expense_claim");
    for grant in [Grant::Publish, Grant::ReadVersions, Grant::Migrate] {
        port.grant(harness::SITE, TOKEN_ALICE, grant);
    }
    // The cases publish versions as they need them (numbers climb), each with runs to move.
    for version in 1..=64 {
        port.set_running_runs(harness::SITE, "expense_claim", version, 1);
    }
    std::sync::Arc::new(port)
}

/// The webhook signing-secret port of the conformance app: every webhook has a secret.
struct ConformanceSecrets;

impl spot_http::api::ports::webhook_secret::WebhookSecretPort for ConformanceSecrets {
    fn put<'a>(
        &'a self,
        _ctx: &'a spot_http::api::service::RequestCtx,
        _webhook: &'a str,
        input: spot_http::api::ports::webhook_secret::SigningSecretIn,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        Result<
            spot_http::api::ports::webhook_secret::SigningSecretOut,
            spot_http::api::ports::webhook_secret::WebhookSecretError,
        >,
    > {
        use spot_http::api::ports::webhook_secret::{
            SigningSecretIn, SigningSecretOut, SigningSecretText,
        };
        Box::pin(async move {
            let generated = match input {
                SigningSecretIn::Generate => Some(SigningSecretText::new(
                    "generated-conformance-secret-0123456789abcdef",
                )),
                SigningSecretIn::Supplied(_) => None,
            };
            Ok(SigningSecretOut {
                key_version: 1,
                fingerprint: "0123456789ab".to_owned(),
                generated,
            })
        })
    }

    fn get<'a>(
        &'a self,
        _ctx: &'a spot_http::api::service::RequestCtx,
        _webhook: &'a str,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        Result<
            spot_http::api::ports::webhook_secret::SigningSecretInfo,
            spot_http::api::ports::webhook_secret::WebhookSecretError,
        >,
    > {
        Box::pin(async move {
            Ok(spot_http::api::ports::webhook_secret::SigningSecretInfo {
                set: true,
                key_version: 1,
                fingerprint: "0123456789ab".to_owned(),
            })
        })
    }
}

/// The process definition text the conformance publishes.
fn version_definition(extra: &str) -> String {
    format!("process expense_claim\nnode start\nnode review\n{extra}")
}

/// Publishes one version of `expense_claim` as alice under `key`; returns its number.
async fn publish_version(app: &App, key: &str, extra: &str) -> u64 {
    let call = harness::call(
        app,
        "POST",
        "/api/v1/process/expense_claim/versions",
        &[harness::bearer(TOKEN_ALICE), harness::idem_key(key)],
        Some(json!({"definition": version_definition(extra), "note": key})),
    )
    .await;
    call.expect_status(201, "seed a process version");
    call.body["data"]["version"].as_u64().must()
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

/// The signed-in user's sessions in the conformance app: the current one and one other, both
/// alice's. Any other id is the one answer for an unknown, another user's or an ended session,
/// `auth.session_not_found`; ending one removes it, so a second ending of it is that answer too.
struct ConformanceSessions {
    live: std::sync::Mutex<Vec<spot_http::api::ports::session::SessionOut>>,
}

const CONFORMANCE_SESSION_CURRENT: &str = "ses_current";
const CONFORMANCE_SESSION_OTHER: &str = "ses_other";

impl ConformanceSessions {
    fn new() -> Self {
        let record = |id: &str, current: bool| spot_http::api::ports::session::SessionOut {
            id: id.to_owned(),
            created_at: "2026-10-09T18:04:11Z".to_owned(),
            expires_at: "2026-10-10T18:04:11Z".to_owned(),
            last_used_at: Some("2026-10-10T09:41:02Z".to_owned()),
            method: "password".to_owned(),
            source: "ab12cd34".to_owned(),
            device: Some("Conformance laptop".to_owned()),
            current,
        };
        Self {
            live: std::sync::Mutex::new(vec![
                record(CONFORMANCE_SESSION_CURRENT, true),
                record(CONFORMANCE_SESSION_OTHER, false),
            ]),
        }
    }
}

impl spot_http::api::ports::session::SessionPort for ConformanceSessions {
    fn list<'a>(
        &'a self,
        _ctx: &'a spot_http::api::service::RequestCtx,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        spot_http::api::ports::session::SessionResult<
            Vec<spot_http::api::ports::session::SessionOut>,
        >,
    > {
        Box::pin(async move { Ok(self.live.lock().unwrap_or_else(|p| p.into_inner()).clone()) })
    }

    fn end<'a>(
        &'a self,
        _ctx: &'a spot_http::api::service::RequestCtx,
        id: &'a str,
    ) -> spot_http::api::service::BoxFuture<'a, spot_http::api::ports::session::SessionResult<()>>
    {
        Box::pin(async move {
            let mut live = self.live.lock().unwrap_or_else(|p| p.into_inner());
            match live.iter().position(|s| s.id == id) {
                Some(index) => {
                    live.remove(index);
                    Ok(())
                }
                None => Err(spot_http::api::service::ApiError::from_code(
                    spot_http::api::registry::Code::AuthSessionNotFound,
                )),
            }
        })
    }
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

/// The conformance task port: one open task per task operation (the walk changes each one), all
/// assigned to alice, who also supervises the one `task_reassign` moves.
fn conformance_tasks() -> std::sync::Arc<spot_http::api::ports::task::MemoryTaskPort> {
    use spot_http::api::ports::task::{MemoryTaskPort, TaskSeed};
    let port = MemoryTaskPort::new(
        time::OffsetDateTime::parse(
            "2026-11-12T00:00:00Z",
            &time::format_description::well_known::Rfc3339,
        )
        .must(),
    );
    port.grant(harness::SITE, TOKEN_ALICE, "alice");
    // The users a delegation and a reassignment name must be known users of the site.
    port.grant(harness::SITE, "tok-bob", "bob");
    port.grant(harness::SITE, "tok-carol", "carol");
    for id in [
        "t_get",
        "t_claim",
        "t_release",
        "t_complete",
        "t_delegate",
        "t_reassign",
    ] {
        let mut seed = TaskSeed::new(id, "expense", "Approve the expense", &["alice"]);
        seed.supervisors = vec!["alice".to_owned()];
        port.add_task(harness::SITE, seed);
    }
    std::sync::Arc::new(port)
}

/// A task POST as alice at `version`.
async fn conformance_task_post(
    app: &App,
    id: &str,
    action: &str,
    version: u32,
    key: Option<&str>,
    body: Option<Json>,
) -> harness::Call {
    let mut headers = vec![
        harness::bearer(TOKEN_ALICE),
        ("if-match", format!("\"{version}\"")),
    ];
    if let Some(key) = key {
        headers.push(("idempotency-key", key.to_owned()));
    }
    harness::call(
        app,
        "POST",
        &format!("/api/v1/task/{id}/{action}"),
        &headers,
        body,
    )
    .await
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

/// One identity administration request as alice: every case sends an `Idempotency-Key` and an
/// `If-Match` (the group ignores the ones an operation does not take).
async fn identity_call(
    app: &App,
    method: &str,
    path: &str,
    body: Option<Json>,
    key: &str,
) -> harness::Call {
    let path = format!("/api/v1/identity{path}");
    harness::call(
        app,
        method,
        &path,
        &[
            harness::bearer(TOKEN_ALICE),
            ("idempotency-key", key.to_owned()),
            harness::if_match(7),
        ],
        body,
    )
    .await
}

/// The registered single sign-on groups: the sign-in operations need no bearer (they are public,
/// `security: []`), every administration operation is behind the bearer layer.
#[tokio::test]
async fn the_sso_groups_keep_sign_in_public_and_administration_authenticated() {
    let app = app_with_files();
    let providers = harness::call(&app, "GET", "/api/v1/auth/sso/providers", &[], None).await;
    providers.expect_status(200, "sign-in needs no bearer");
    for (method, path) in [
        ("GET", "/api/v1/identity/sso/providers"),
        ("POST", "/api/v1/identity/sso/providers"),
        ("GET", "/api/v1/identity/sso/providers/p1"),
        ("PUT", "/api/v1/identity/sso/providers/p1"),
        ("DELETE", "/api/v1/identity/sso/providers/p1"),
        ("POST", "/api/v1/identity/sso/providers/p1/test"),
        ("GET", "/api/v1/identity/sso/policy"),
        ("PUT", "/api/v1/identity/sso/policy"),
    ] {
        let reply = harness::call(&app, method, path, &[], None).await;
        reply.expect_status(401, &format!("{method} {path} without a bearer"));
    }
}

/// The binding cookie the fake single sign-on port issued on `begin`.
fn sso_binding_cookie() -> (&'static str, String) {
    ("cookie", "__Host-spot_sso_binding=bind-1".to_owned())
}

fn sso_form() -> (&'static str, String) {
    (
        "content-type",
        "application/x-www-form-urlencoded".to_owned(),
    )
}

fn sso_provider_input() -> Json {
    json!({
        "kind": "oidc", "name": "Okta", "audience": ["staff"],
        "oidc": {"issuer": "https://idp.example.com", "client_id": "cid", "scopes": ["openid"]}
    })
}

const IDENTITY_USER: &str = "0190a1b2-c3d4-7e5f-8a6b-1c2d3e4f5a6b";
const IDENTITY_OTHER: &str = "0190a1b2-c3d4-7e5f-8a6b-1c2d3e4f5a6c";

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
        ("GET", "auth_session_list") => harness::call(app, "GET", "/api/v1/auth/sessions", &[harness::bearer(&kit.token)], None).await,
        ("DELETE", "auth_session_end") => harness::call(app, "DELETE", &format!("/api/v1/auth/sessions/{CONFORMANCE_SESSION_OTHER}"), &[harness::bearer(&kit.token)], None).await,
        ("GET", "report_list") => harness::call(app, "GET", "/api/v1/report", &[harness::bearer(&kit.token)], None).await,
        ("GET", "report_describe") => harness::call(app, "GET", &format!("/api/v1/report/{CONFORMANCE_REPORT}"), &[harness::bearer(&kit.token)], None).await,
        ("GET", "task_list") => harness::call(app, "GET", "/api/v1/task", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("GET", "task_get") => harness::call(app, "GET", "/api/v1/task/t_get", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("POST", "task_claim") => conformance_task_post(app, "t_claim", "claim", 1, None, None).await,
        ("POST", "task_release") => {
            conformance_task_post(app, "t_release", "claim", 1, None, None).await.expect_status(200, "claim before the release case");
            conformance_task_post(app, "t_release", "release", 2, None, None).await
        }
        ("POST", "task_complete") => conformance_task_post(app, "t_complete", "complete", 1, Some("conf-complete"), Some(json!({"outcome": "approve", "comment": "Looks right."}))).await,
        ("POST", "task_delegate") => conformance_task_post(app, "t_delegate", "delegate", 1, None, Some(json!({"to": "carol", "reason": "On leave."}))).await,
        ("POST", "task_reassign") => conformance_task_post(app, "t_reassign", "reassign", 1, None, Some(json!({"to": "bob", "reason": "Covering the queue."}))).await,
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
        ("GET", "process_list") => harness::call(app, "GET", "/api/v1/process", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("POST", "run_start") => harness::call(app, "POST", "/api/v1/process/expense_claim/runs", &[harness::bearer(TOKEN_ALICE), ("idempotency-key", "conf-run-start".to_owned())], Some(json!({"input": {"amount": "10.00"}}))).await,
        ("GET", "run_list") => {
            conformance_run(app, "conf-run-list").await;
            harness::call(app, "GET", "/api/v1/process/runs", &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("GET", "run_get") => {
            let id = conformance_run(app, "conf-run-get").await;
            harness::call(app, "GET", &format!("/api/v1/process/runs/{id}"), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("GET", "run_events") => {
            let id = conformance_run(app, "conf-run-events").await;
            harness::call(app, "GET", &format!("/api/v1/process/runs/{id}/events"), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("POST", "run_cancel") => {
            let id = conformance_run(app, "conf-run-cancel").await;
            harness::call(app, "POST", &format!("/api/v1/process/runs/{id}/cancel"), &[harness::bearer(TOKEN_ALICE)], Some(json!({"reason": "Conformance."}))).await
        }
        ("POST", "process_webhook") => harness::call(app, "POST", "/api/v1/process/hook/orders", &[("x-spot-signature", WEBHOOK_SIGNATURE.to_owned())], Some(json!({"order": "SO-1", "amount": "10.00"}))).await,
        ("GET", "import_limits") => harness::call(app, "GET", "/api/v1/import/limits", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("POST", "import_create") => harness::call(app, "POST", "/api/v1/import", &[harness::bearer(TOKEN_ALICE), harness::idem_key("conf-import-create")], Some(json!({"file_id": IMPORT_FILE, "doctype": "customer", "mode": "insert", "mapping": {"columns": [{"column": "Name", "field": "name"}]}}))).await,
        ("GET", "import_get") => {
            let job = import_dry_run(app, "conf-import-get").await;
            harness::call(app, "GET", &format!("/api/v1/import/{}", job["id"].as_str().must()), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("GET", "import_errors") => {
            let job = import_dry_run(app, "conf-import-errors").await;
            harness::call(app, "GET", &format!("/api/v1/import/{}/errors?limit=10", job["id"].as_str().must()), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("POST", "import_run") => {
            let job = import_dry_run(app, "conf-import-run").await;
            harness::call(app, "POST", &format!("/api/v1/import/{}/run", job["id"].as_str().must()), &[harness::bearer(TOKEN_ALICE), harness::idem_key("conf-import-run-go")], Some(json!({"digest": job["digest"]}))).await
        }
        ("POST", "import_cancel") => {
            let job = import_dry_run(app, "conf-import-cancel").await;
            harness::call(app, "POST", &format!("/api/v1/import/{}/cancel", job["id"].as_str().must()), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("POST", "occupancy_claim") => harness::call(app, "POST", "/api/v1/occupancy/room_hold/holds", &[harness::bearer(TOKEN_ALICE), harness::idem_key("conf-occ-claim")], Some(json!({"unit": "u-claim", "start": "2030-01-10T14:00:00Z", "end": "2030-01-11T10:00:00Z", "ttl_seconds": 600}))).await,
        ("POST", "occupancy_confirm") => {
            let hold = occupancy_hold(app, "u-confirm", "conf-occ-confirm-seed").await;
            harness::call(app, "POST", &format!("/api/v1/occupancy/room_hold/holds/{}/confirm", hold["id"].as_str().must()), &[harness::bearer(TOKEN_ALICE), harness::idem_key("conf-occ-confirm")], None).await
        }
        ("DELETE", "occupancy_release") => {
            let hold = occupancy_hold(app, "u-release", "conf-occ-release-seed").await;
            harness::call(app, "DELETE", &format!("/api/v1/occupancy/room_hold/holds/{}", hold["id"].as_str().must()), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("GET", "occupancy_availability") => {
            occupancy_hold(app, "u-avail", "conf-occ-avail-seed").await;
            harness::call(app, "GET", "/api/v1/occupancy/room_hold/availability?unit=u-avail&from=2030-01-09T00:00:00Z&to=2030-01-13T00:00:00Z", &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("POST", "graph_query") => harness::call(app, "POST", "/api/v1/graph/query", &[harness::bearer(TOKEN_ALICE)], Some(json!({"start": {"doctype": "sales_invoice", "id": graph_id(1)}, "direction": "both", "depth": 1, "limit": 10}))).await,
        ("POST", "lineage_trace") => harness::call(app, "POST", "/api/v1/lineage", &[harness::bearer(TOKEN_ALICE)], Some(json!({"kind": "bom", "doctype": "item", "id": graph_id(10)}))).await,
        ("PUT", "webhook_signing_secret_put") => harness::call(app, "PUT", "/api/v1/webhooks/orders/signing-secret", &[harness::bearer(TOKEN_ALICE), harness::idem_key("conf-wh-secret")], Some(json!({"generate": true}))).await,
        ("GET", "webhook_signing_secret_get") => harness::call(app, "GET", "/api/v1/webhooks/orders/signing-secret", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("GET", "process_version_list") => {
            publish_version(app, "conf-ver-list", "list\n").await;
            harness::call(app, "GET", "/api/v1/process/expense_claim/versions", &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("POST", "process_version_publish") => harness::call(app, "POST", "/api/v1/process/expense_claim/versions", &[harness::bearer(TOKEN_ALICE), harness::idem_key("conf-ver-publish")], Some(json!({"definition": version_definition("publish\n"), "note": "conformance"}))).await,
        ("GET", "process_version_get") => {
            let n = publish_version(app, "conf-ver-get", "get\n").await;
            harness::call(app, "GET", &format!("/api/v1/process/expense_claim/versions/{n}"), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("POST", "process_version_retire") => {
            let n = publish_version(app, "conf-ver-retire", "retire\n").await;
            harness::call(app, "POST", &format!("/api/v1/process/expense_claim/versions/{n}/retire"), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("POST", "process_migration_plan") => {
            let from = publish_version(app, "conf-mig-plan-a", "plan a\n").await;
            let to = publish_version(app, "conf-mig-plan-b", "plan b\n").await;
            harness::call(app, "POST", &format!("/api/v1/process/expense_claim/versions/{from}/migrations/plan"), &[harness::bearer(TOKEN_ALICE)], Some(json!({"to": to}))).await
        }
        ("POST", "process_migration_start") => {
            let from = publish_version(app, "conf-mig-start-a", "start a\n").await;
            let to = publish_version(app, "conf-mig-start-b", "start b\n").await;
            harness::call(app, "POST", &format!("/api/v1/process/expense_claim/versions/{from}/migrations"), &[harness::bearer(TOKEN_ALICE), harness::idem_key("conf-mig-start")], Some(json!({"to": to}))).await
        }
        ("GET", "process_migration_get") => {
            let from = publish_version(app, "conf-mig-get-a", "get a\n").await;
            let to = publish_version(app, "conf-mig-get-b", "get b\n").await;
            let started = harness::call(app, "POST", &format!("/api/v1/process/expense_claim/versions/{from}/migrations"), &[harness::bearer(TOKEN_ALICE), harness::idem_key("conf-mig-get")], Some(json!({"to": to}))).await;
            started.expect_status(202, "seed a migration");
            let id = started.body["migration_id"].as_str().must().to_owned();
            harness::call(app, "GET", &format!("/api/v1/process/expense_claim/migrations/{id}"), &[harness::bearer(TOKEN_ALICE)], None).await
        }
        ("GET", "connector_list") => harness::call(app, "GET", "/api/v1/connectors", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("GET", "connector_get") => harness::call(app, "GET", "/api/v1/connectors/crm", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("POST", "connector_invoke") => harness::call(app, "POST", "/api/v1/connectors/crm/operations/echo", &[harness::bearer(TOKEN_ALICE), ("idempotency-key", "conf-connector".to_owned())], Some(json!({"input": {}}))).await,
        ("GET", "connector_runs") => harness::call(app, "GET", "/api/v1/connectors/crm/runs", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("POST", "auth_invitation_accept") => raw_call(app, "POST", "/api/v1/auth/invitations/accept", &[("content-type", "application/json".to_owned())], json!({"token": INVITATION_TOKEN, "password": "correct horse battery staple"}).to_string().as_bytes()).await,
        ("GET", "journal_info_get") => harness::call(app, "GET", "/api/v1/journal", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("GET", "journal_manifest_get") => harness::call(app, "GET", "/api/v1/journal/manifest/1", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("GET", "journal_segment_get") => harness::call(app, "GET", "/api/v1/journal/segments/1/1-100", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("POST", "link_inbound") => raw_call(app, "POST", "/api/v1/link/inbound/acme", &[("content-type", "application/json".to_owned())], b"{}").await,
        ("GET", "app_list") => harness::call(app, "GET", "/api/v1/apps", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("GET", "identity_user_list") => identity_call(app, "GET", "/users?status=active&q=ay&limit=10", None, "c-identity_user_list").await,
        ("POST", "identity_user_create") => identity_call(app, "POST", "/users", Some(json!({"email": "n@acme.com", "full_name": "N", "audience": "staff", "roles": ["Viewer"], "invite": true})), "c-identity_user_create").await,
        ("GET", "identity_user_get") => identity_call(app, "GET", &format!("/users/{IDENTITY_USER}"), None, "c-identity_user_get").await,
        ("PATCH", "identity_user_patch") => identity_call(app, "PATCH", &format!("/users/{IDENTITY_USER}"), Some(json!({"full_name": "New Name"})), "c-identity_user_patch").await,
        ("POST", "identity_user_disable") => identity_call(app, "POST", &format!("/users/{IDENTITY_USER}/disable"), None, "c-identity_user_disable").await,
        ("POST", "identity_user_enable") => identity_call(app, "POST", &format!("/users/{IDENTITY_USER}/enable"), None, "c-identity_user_enable").await,
        ("POST", "identity_user_anonymise") => identity_call(app, "POST", &format!("/users/{IDENTITY_USER}/anonymise"), Some(json!({"confirm_email": "a@acme.com"})), "c-identity_user_anonymise").await,
        ("POST", "identity_user_recover") => identity_call(app, "POST", &format!("/users/{IDENTITY_USER}/recover"), Some(json!({"kind": "password_reset"})), "c-identity_user_recover").await,
        ("GET", "identity_invitation_list") => identity_call(app, "GET", "/invitations", None, "c-identity_invitation_list").await,
        ("DELETE", "identity_invitation_revoke") => identity_call(app, "DELETE", &format!("/invitations/{IDENTITY_USER}"), None, "c-identity_invitation_revoke").await,
        ("POST", "identity_invitation_resend") => identity_call(app, "POST", &format!("/invitations/{IDENTITY_USER}/resend"), None, "c-identity_invitation_resend").await,
        ("GET", "identity_role_list") => identity_call(app, "GET", "/roles", None, "c-identity_role_list").await,
        ("GET", "identity_role_get") => identity_call(app, "GET", &format!("/roles/{IDENTITY_USER}"), None, "c-identity_role_get").await,
        ("POST", "identity_role_customise") => identity_call(app, "POST", &format!("/roles/{IDENTITY_USER}/customise"), Some(json!({"name": "Copy"})), "c-identity_role_customise").await,
        ("GET", "identity_role_upgrade_diff") => identity_call(app, "GET", &format!("/roles/{IDENTITY_USER}/upgrade-diff"), None, "c-identity_role_upgrade_diff").await,
        ("GET", "identity_assignment_list") => identity_call(app, "GET", &format!("/assignments?user={IDENTITY_USER}&role={IDENTITY_OTHER}"), None, "c-identity_assignment_list").await,
        ("POST", "identity_assignment_create") => identity_call(app, "POST", "/assignments", Some(json!({"user_id": IDENTITY_USER, "role_id": IDENTITY_OTHER, "scope": "company:A", "expires_at": "2027-01-01T00:00:00Z", "reason": "cover"})), "c-identity_assignment_create").await,
        ("DELETE", "identity_assignment_delete") => identity_call(app, "DELETE", &format!("/assignments/{IDENTITY_USER}"), None, "c-identity_assignment_delete").await,
        ("GET", "identity_assignment_rules_get") => identity_call(app, "GET", "/assignment-rules", None, "c-identity_assignment_rules_get").await,
        ("PUT", "identity_assignment_rules_put") => identity_call(app, "PUT", "/assignment-rules", Some(json!({"rules": [{"id": "r1", "when": "dept == 'x'", "role_id": IDENTITY_USER, "scope": null, "enabled": true}]})), "c-identity_assignment_rules_put").await,
        ("POST", "identity_assignment_rules_preview") => identity_call(app, "POST", "/assignment-rules/preview", Some(json!({})), "c-identity_assignment_rules_preview").await,
        ("POST", "identity_assignment_rules_run") => identity_call(app, "POST", "/assignment-rules/run", Some(json!({"plan_digest": "abc123"})), "c-identity_assignment_rules_run").await,
        ("GET", "identity_api_key_list") => identity_call(app, "GET", "/api-keys", None, "c-identity_api_key_list").await,
        ("POST", "identity_api_key_create") => identity_call(app, "POST", "/api-keys", Some(json!({"name": "ci", "owner_id": IDENTITY_USER, "scopes": ["read"]})), "c-identity_api_key_create").await,
        ("DELETE", "identity_api_key_delete") => identity_call(app, "DELETE", &format!("/api-keys/{IDENTITY_USER}"), None, "c-identity_api_key_delete").await,
        ("GET", "identity_scim_credential_list") => identity_call(app, "GET", "/scim-credentials", None, "c-identity_scim_credential_list").await,
        ("POST", "identity_scim_credential_create") => identity_call(app, "POST", "/scim-credentials", None, "c-identity_scim_credential_create").await,
        ("DELETE", "identity_scim_credential_delete") => identity_call(app, "DELETE", &format!("/scim-credentials/{IDENTITY_USER}"), None, "c-identity_scim_credential_delete").await,
        ("GET", "identity_session_list") => identity_call(app, "GET", &format!("/sessions?user={IDENTITY_USER}"), None, "c-identity_session_list").await,
        ("DELETE", "identity_session_end") => identity_call(app, "DELETE", &format!("/sessions/{IDENTITY_USER}"), None, "c-identity_session_end").await,
        ("POST", "identity_user_sessions_end_all") => identity_call(app, "POST", &format!("/users/{IDENTITY_USER}/sessions/end-all"), None, "c-identity_user_sessions_end_all").await,
        ("POST", "identity_impersonation_start") => identity_call(app, "POST", "/impersonation", Some(json!({"user_id": IDENTITY_USER, "reason": "ticket 1", "ticket": "T-1"})), "c-identity_impersonation_start").await,
        ("DELETE", "identity_impersonation_end") => identity_call(app, "DELETE", "/impersonation", None, "c-identity_impersonation_end").await,
        ("GET", "sso_provider_list") => harness::call(app, "GET", "/api/v1/auth/sso/providers", &[], None).await,
        ("POST", "sso_begin") => harness::call(app, "POST", "/api/v1/auth/sso/okta/begin", &[], Some(json!({"return_to": "/desk"}))).await,
        ("GET", "sso_callback") => raw_call(app, "GET", "/api/v1/auth/sso/okta/callback?code=c&state=s", &[sso_binding_cookie()], b"").await,
        ("POST", "sso_acs") => raw_call(app, "POST", "/api/v1/auth/sso/saml/acs", &[sso_form(), sso_binding_cookie()], b"SAMLResponse=abc&RelayState=r").await,
        ("POST", "sso_complete") => raw_call(app, "POST", "/api/v1/auth/sso/complete", &[("cookie", "__Host-spot_sso_handover=hand-1".to_owned())], b"").await,
        ("POST", "sso_backchannel_logout") => raw_call(app, "POST", "/api/v1/auth/sso/okta/backchannel-logout", &[sso_form()], b"logout_token=t").await,
        ("GET", "sso_slo_get") => raw_call(app, "GET", "/api/v1/auth/sso/saml/slo?SAMLRequest=abc", &[], b"").await,
        ("POST", "sso_slo_post") => raw_call(app, "POST", "/api/v1/auth/sso/saml/slo", &[sso_form()], b"SAMLResponse=abc").await,
        ("GET", "sso_saml_metadata") => raw_call(app, "GET", "/api/v1/auth/sso/saml/saml-metadata", &[], b"").await,
        ("GET", "sso_admin_provider_list") => harness::call(app, "GET", "/api/v1/identity/sso/providers?limit=5", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("POST", "sso_admin_provider_create") => harness::call(app, "POST", "/api/v1/identity/sso/providers", &[harness::bearer(TOKEN_ALICE), ("idempotency-key", "c-sso-create".to_owned())], Some(sso_provider_input())).await,
        ("GET", "sso_admin_provider_get") => harness::call(app, "GET", "/api/v1/identity/sso/providers/p1", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("PUT", "sso_admin_provider_put") => harness::call(app, "PUT", "/api/v1/identity/sso/providers/p1", &[harness::bearer(TOKEN_ALICE), harness::if_match(3)], Some(sso_provider_input())).await,
        ("DELETE", "sso_admin_provider_delete") => harness::call(app, "DELETE", "/api/v1/identity/sso/providers/p1", &[harness::bearer(TOKEN_ALICE), harness::if_match(3)], None).await,
        ("POST", "sso_admin_provider_test") => harness::call(app, "POST", "/api/v1/identity/sso/providers/p1/test", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("GET", "sso_admin_policy_get") => harness::call(app, "GET", "/api/v1/identity/sso/policy", &[harness::bearer(TOKEN_ALICE)], None).await,
        ("PUT", "sso_admin_policy_put") => harness::call(app, "PUT", "/api/v1/identity/sso/policy", &[harness::bearer(TOKEN_ALICE), harness::if_match(1)], Some(json!({"mode": "sso_optional", "break_glass_users": [IDENTITY_USER]}))).await,
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
    // The contract is at 0.26.1; the 0.6.0 properties must still hold unchanged.
    assert_eq!(root["info"]["version"], "0.26.1");
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
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(
        root["x-spot-error-codes"].as_object().must().len(),
        149,
        "the registry counts the two print codes, from 0.10.0 partition.not_owner and, from 0.12.0, the thirteen process and task codes and, from 0.13.0, the six version codes and, from 0.14.0, the five graph and lineage codes"
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
    assert_eq!(root["info"]["version"], "0.26.1");
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

    // 0.9.0 added no operation and no error code: 0.10.0 added `partition.not_owner` (the 62nd)
    // and 0.12.0 the thirteen process and task codes and 0.13.0 the six version codes (81 in all at 0.13.0, 86 at 0.14.0).
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
}

/// Contract 0.10.0 (WP-CON-03.T13), additive: `partition.not_owner`, the notice feed and the
/// consumer session renewal. The two operations are not served yet (NOT_YET_SERVED); this test
/// pins the shape a server must keep.
#[test]
fn the_0_10_0_additions_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
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

/// Contract 0.12.0 (WP-CON-03.T20), additive: the process, run and task inbox operations and
/// their thirteen error codes. No server yet (the route modules that follow add them to
/// `SERVES`, and the walk then needs a case for each); this pins the shape a server must keep.
#[test]
fn the_0_12_0_process_and_task_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(
        api_operations(&root).len(),
        189,
        "71 operations less the health probes and the realtime upgrade (0.12.0 had 57 of 60, 0.13.0 64 of 67, 0.14.0 66 of 69)"
    );
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);

    // (operationId, method, path); only `process_webhook` is public.
    let expected = [
        ("process_list", "get", "/api/v1/process"),
        ("run_start", "post", "/api/v1/process/{process}/runs"),
        ("run_list", "get", "/api/v1/process/runs"),
        ("run_get", "get", "/api/v1/process/runs/{id}"),
        ("run_events", "get", "/api/v1/process/runs/{id}/events"),
        ("run_cancel", "post", "/api/v1/process/runs/{id}/cancel"),
        ("process_webhook", "post", "/api/v1/process/hook/{trigger}"),
        ("task_list", "get", "/api/v1/task"),
        ("task_get", "get", "/api/v1/task/{id}"),
        ("task_claim", "post", "/api/v1/task/{id}/claim"),
        ("task_release", "post", "/api/v1/task/{id}/release"),
        ("task_complete", "post", "/api/v1/task/{id}/complete"),
        ("task_delegate", "post", "/api/v1/task/{id}/delegate"),
        ("task_reassign", "post", "/api/v1/task/{id}/reassign"),
    ];
    for (id, method, path) in expected {
        let op = &root["paths"][path][method];
        assert_eq!(op["operationId"], id, "{method} {path}");
        let public = op["security"].as_array().is_some_and(Vec::is_empty);
        assert_eq!(
            public,
            id == "process_webhook",
            "{id}: only the webhook is public"
        );
    }
    for (code, status) in [
        ("process.not_found", 404),
        ("process.input_invalid", 422),
        ("process.run_not_found", 404),
        ("process.run_not_active", 409),
        ("process.webhook_rejected", 401),
        ("task.not_found", 404),
        ("task.already_claimed", 409),
        ("task.not_claimed_by_caller", 409),
        ("task.outcome_invalid", 422),
        ("task.segregation", 403),
        ("task.delegation_invalid", 422),
        ("task.target_invalid", 422),
        ("task.not_active", 409),
    ] {
        assert_eq!(root["x-spot-error-codes"][code][0], status, "{code}");
        assert_eq!(
            root["x-spot-error-codes"][code][1], false,
            "{code} is not retryable"
        );
    }

    // The bodies are closed and bounded, and a run answer carries no variables.
    let start = component_validator(&root, "RunStart");
    assert!(start.is_valid(&json!({"input": {"amount": "1.00"}, "business_key": "EXP-1"})));
    assert!(!start.is_valid(&json!({"input": {}, "starter": "someone"})));
    assert!(!start.is_valid(&json!({"input": {}, "business_key": ""})));
    assert!(!start.is_valid(&json!({"business_key": "x"})));
    let complete = component_validator(&root, "TaskComplete");
    assert!(complete.is_valid(&json!({"outcome": "approve", "comment": "ok", "values": {"a": 1}})));
    assert!(!complete.is_valid(&json!({"outcome": "approve", "actor": "system:sla"})));
    assert!(!complete.is_valid(&json!({"outcome": "approve", "comment": "x".repeat(2001)})));
    let cancel = component_validator(&root, "RunCancel");
    assert!(cancel.is_valid(&json!({"reason": "wrong customer"})));
    assert!(!cancel.is_valid(&json!({"reason": ""})));
    assert!(!cancel.is_valid(&json!({"reason": "x".repeat(501)})));
    let run = component_validator(&root, "Run");
    let a_run = json!({
        "id": "k3J9x0Qw7Zr2Lm8Pa1Bc4D",
        "process": "expense_claim",
        "version": 3,
        "state": "waiting",
        "business_key": null,
        "started_at": "2026-11-12T08:00:00Z",
        "steps": [{"node": "review", "label": "Review", "state": "waiting"}],
        "documents": [{"doctype": "expense_claim", "id": "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f"}]
    });
    assert!(run.is_valid(&a_run));
    let mut leaky = a_run.clone();
    leaky["variables"] = json!({"secret": 1});
    assert!(!run.is_valid(&leaky), "a run carries no variables");
    let mut stateless = a_run.clone();
    stateless["state"] = json!("paused");
    assert!(!run.is_valid(&stateless));
}

/// Contract 0.13.0 (WP-CON-03.T21), additive: the process version and migration operations and
/// their six error codes. No server yet; this pins the shape a server must keep.
#[test]
fn the_0_13_0_process_version_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);

    let expected = [
        (
            "process_version_list",
            "get",
            "/api/v1/process/{process}/versions",
        ),
        (
            "process_version_publish",
            "post",
            "/api/v1/process/{process}/versions",
        ),
        (
            "process_version_get",
            "get",
            "/api/v1/process/{process}/versions/{n}",
        ),
        (
            "process_version_retire",
            "post",
            "/api/v1/process/{process}/versions/{n}/retire",
        ),
        (
            "process_migration_plan",
            "post",
            "/api/v1/process/{process}/versions/{from}/migrations/plan",
        ),
        (
            "process_migration_start",
            "post",
            "/api/v1/process/{process}/versions/{from}/migrations",
        ),
        (
            "process_migration_get",
            "get",
            "/api/v1/process/{process}/migrations/{id}",
        ),
    ];
    for (id, method, path) in expected {
        let op = &root["paths"][path][method];
        assert_eq!(op["operationId"], id, "{method} {path}");
        assert!(op.get("security").is_none(), "{id} needs the bearer");
    }
    for (code, status) in [
        ("process.version_conflict", 409),
        ("process.version_not_published", 409),
        ("process.publish_denied", 403),
        ("process.definition_invalid", 422),
        ("process.migration_invalid", 422),
        ("process.migration_not_found", 404),
    ] {
        assert_eq!(root["x-spot-error-codes"][code][0], status, "{code}");
        assert_eq!(
            root["x-spot-error-codes"][code][1], false,
            "{code} is not retryable"
        );
    }

    let publish = component_validator(&root, "ProcessVersionPublish");
    assert!(publish.is_valid(&json!({"definition": "x", "note": "n", "expected_previous": 2})));
    assert!(!publish.is_valid(&json!({"note": "n"})));
    assert!(!publish.is_valid(&json!({"definition": "x", "actor": "someone"})));
    assert!(!publish.is_valid(&json!({"definition": "x".repeat(262_145)})));
    assert!(!publish.is_valid(&json!({"definition": "x", "note": "n".repeat(501)})));
    let request = component_validator(&root, "MigrationRequest");
    assert!(request.is_valid(&json!({"to": 3})));
    assert!(request.is_valid(
        &json!({"to": 3, "mapping": {"nodes": {"a": "b"}, "required_variables": ["x.y"]}})
    ));
    assert!(!request.is_valid(&json!({"to": 0})));
    assert!(!request.is_valid(&json!({"to": 3, "mapping": {"unknown": 1}})));
    assert!(!request.is_valid(&json!({"mapping": {}})));
    let migration = component_validator(&root, "Migration");
    let a_migration = json!({"state": "running", "moved": 3, "skipped": 1, "failed": 0, "started_at": "2026-11-12T08:00:00Z"});
    assert!(migration.is_valid(&a_migration));
    let mut paused = a_migration.clone();
    paused["state"] = json!("paused");
    assert!(!migration.is_valid(&paused), "the wire has three states");
    let unmoved = component_validator(&root, "MigrationUnmoved");
    assert!(unmoved.is_valid(
        &json!({"run": "k3J9x0Qw7Zr2Lm8Pa1Bc4D", "reason": "bpm_versioning.unmapped_node"})
    ));
    assert!(
        !unmoved
            .is_valid(&json!({"run": "k3J9x0Qw7Zr2Lm8Pa1Bc4D", "reason": "x.y", "variables": {}}))
    );
    let detail = component_validator(&root, "ProcessVersionDetail");
    assert!(!detail.is_valid(
        &json!({"version": 1, "state": "published", "published_at": null, "published_by": null,
        "note": "", "running_runs": 0, "definition": "x", "hash": "nothex"})
    ));

    // Insight needs no operation: the three engine report names are in the `report_list` text.
    let text = root["paths"]["/api/v1/report"]["get"]["description"]
        .as_str()
        .must();
    for name in [
        "bpm_cycle_time_by_step",
        "bpm_stuck_queue",
        "bpm_throughput_by_week",
    ] {
        assert!(text.contains(name), "report_list names {name}");
    }
}

/// Contract 0.14.0 (WP-CON-03.T22), additive: `graph_query` and `lineage_trace` and their five
/// error codes. No server yet; this pins the shape a server must keep.
#[test]
fn the_0_14_0_graph_and_lineage_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);

    for (id, path) in [
        ("graph_query", "/api/v1/graph/query"),
        ("lineage_trace", "/api/v1/lineage"),
    ] {
        let op = &root["paths"][path]["post"];
        assert_eq!(op["operationId"], id, "{path}");
        assert!(op.get("security").is_none(), "{id} needs the bearer");
        let parameters = op["parameters"].as_array().map_or(0, Vec::len);
        assert_eq!(parameters, 0, "{id} takes no Idempotency-Key, it is a read");
    }
    for (code, status, retryable) in [
        ("graph.not_found", 404, false),
        ("graph.query_invalid", 422, false),
        ("graph.too_large", 422, false),
        ("graph.unavailable", 503, true),
        ("lineage.kind_unknown", 422, false),
    ] {
        assert_eq!(root["x-spot-error-codes"][code][0], status, "{code}");
        assert_eq!(root["x-spot-error-codes"][code][1], retryable, "{code}");
    }

    let query = component_validator(&root, "GraphQuery");
    let start = json!({"doctype": "sales_invoice", "id": "0192f3c4-0000-7000-8000-00000000a001"});
    assert!(
        query.is_valid(&json!({"start": start, "direction": "both", "depth": 3, "limit": 200}))
    );
    assert!(query.is_valid(&json!({
        "start": start, "direction": "out", "depth": 8, "limit": 1000,
        "edge_types": ["bills"], "return_edge_properties": true,
        "node_filter": {"and": [
            {"field": "status", "op": "eq", "value": "open"},
            {"not": {"field": "grand_total", "is_null": true}},
            {"field": "party", "in": ["a", "b"]}
        ]},
    })));
    assert!(
        !query.is_valid(&json!({"start": start, "direction": "both", "depth": 0, "limit": 200}))
    );
    assert!(
        !query.is_valid(&json!({"start": start, "direction": "both", "depth": 9, "limit": 200}))
    );
    assert!(
        !query.is_valid(&json!({"start": start, "direction": "both", "depth": 3, "limit": 1001}))
    );
    assert!(
        !query.is_valid(&json!({"start": start, "direction": "sideways", "depth": 3, "limit": 5}))
    );
    assert!(!query.is_valid(&json!({"direction": "both", "depth": 3, "limit": 5})));
    assert!(!query.is_valid(
        &json!({"start": start, "direction": "both", "depth": 3, "limit": 5, "extra": 1})
    ));
    let many = vec!["e"; 17];
    assert!(!query.is_valid(
        &json!({"start": start, "direction": "both", "depth": 3, "limit": 5, "edge_types": many})
    ));
    // A filter operator the crate does not have is not in the schema.
    assert!(!query.is_valid(&json!({
        "start": start, "direction": "both", "depth": 3, "limit": 5,
        "node_filter": {"field": "name", "op": "like", "value": "x%"}
    })));
    assert!(!query.is_valid(&json!({
        "start": start, "direction": "both", "depth": 3, "limit": 5,
        "node_filter": {"field": "name", "op": "eq", "value": 1.5}
    })));

    let answer = component_validator(&root, "GraphAnswer");
    let edge_with_properties =
        json!({"from": start, "to": start, "type": "bills", "properties": {"qty": "2.5"}});
    assert!(answer.is_valid(&json!({
        "nodes": [start], "edges": [edge_with_properties], "truncated": true,
        "truncation_reason": "paths", "withheld": false
    })));
    assert!(!answer.is_valid(&json!({
        "nodes": [start], "edges": [], "truncated": true, "truncation_reason": "time", "withheld": false
    })));
    assert!(!answer.is_valid(&json!({
        "nodes": [{"doctype": "sales_invoice", "id": "0192f3c4-0000-7000-8000-00000000a001", "grand_total": "5"}],
        "edges": [], "truncated": false, "withheld": false
    })), "a node is doctype and id only");
    assert!(
        !answer.is_valid(&json!({
            "nodes": [], "edges": [], "truncated": false, "withheld": 3
        })),
        "withheld is a flag, never a count"
    );

    let lineage = component_validator(&root, "LineageRequest");
    let item = "0192f3c4-0000-7000-8000-00000000c003";
    assert!(lineage.is_valid(
        &json!({"kind": "bom", "doctype": "item", "id": item, "depth": 12, "limit": 5000})
    ));
    assert!(lineage.is_valid(&json!({"kind": "chain", "doctype": "sales_invoice", "id": item})));
    assert!(!lineage.is_valid(&json!({"kind": "sideways", "doctype": "item", "id": item})));
    assert!(!lineage.is_valid(&json!({"kind": "bom", "doctype": "item", "id": item, "depth": 13})));
    assert!(
        !lineage.is_valid(&json!({"kind": "bom", "doctype": "item", "id": item, "chain": {}})),
        "no client chain configuration"
    );
    let lineage_answer = component_validator(&root, "LineageAnswer");
    assert!(lineage_answer.is_valid(&json!({
        "levels": [{"depth": 0, "nodes": [{"doctype": "item", "id": item, "quantity": "2.500"}], "node_count": 1, "quantity_total": "2.500"}],
        "truncated": false, "withheld": true
    })));
    assert!(!lineage_answer.is_valid(&json!({
        "levels": [{"depth": 0, "nodes": [{"doctype": "item", "id": item, "quantity": 2.5}], "node_count": 1}],
        "truncated": false, "withheld": false
    })), "a quantity is a decimal string, never a number");
}

/// Contract 0.15.0 (WP-CON-03.T66), additive: the two webhook signing-secret operations. No new
/// error code. No server yet; this pins the shape a server must keep.
#[test]
fn the_0_15_0_webhook_signing_secret_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);

    let path = "/api/v1/webhooks/{name}/signing-secret";
    for (method, id) in [
        ("put", "webhook_signing_secret_put"),
        ("get", "webhook_signing_secret_get"),
    ] {
        let op = &root["paths"][path][method];
        assert_eq!(op["operationId"], id, "{method} {path}");
        assert!(op.get("security").is_none(), "{id} needs the bearer");
        assert_eq!(
            op["responses"]["200"]["headers"]["Cache-Control"]["schema"]["const"],
            "private, no-store",
            "{id} is never cached"
        );
        assert!(
            op.get("parameters").is_none(),
            "{id} takes no Idempotency-Key or If-Match"
        );
    }
    // The routes answer with codes that are already registered; no webhook_secret.* code is a
    // registry code (they ride in `details.reason`).
    for code in [
        "permission.denied",
        "document.not_found",
        "api.invalid_request",
        "server.unavailable",
        "auth.required",
    ] {
        assert!(root["x-spot-error-codes"][code].is_array(), "{code}");
    }
    assert!(
        root["x-spot-error-codes"]
            .as_object()
            .must()
            .keys()
            .all(|code| !code.starts_with("webhook_secret.")),
        "the catalogue codes are details.reason values"
    );

    let put = component_validator(&root, "WebhookSigningSecretPut");
    assert!(put.is_valid(&json!({"generate": true})));
    assert!(put.is_valid(&json!({"secret": "k".repeat(16)})));
    assert!(put.is_valid(&json!({"secret": "k".repeat(256)})));
    assert!(!put.is_valid(&json!({"secret": "k".repeat(15)})));
    assert!(!put.is_valid(&json!({"secret": "k".repeat(257)})));
    assert!(!put.is_valid(&json!({"generate": false})));
    assert!(
        !put.is_valid(&json!({"generate": true, "secret": "k".repeat(16)})),
        "never both"
    );
    assert!(!put.is_valid(&json!({})), "never neither");
    assert!(!put.is_valid(&json!({"generate": true, "note": "x"})));

    let answer = component_validator(&root, "WebhookSigningSecretPutResponse");
    assert!(answer.is_valid(&json!({"data": {"key_version": 2, "fingerprint": "0123456789ab"}})));
    assert!(answer.is_valid(&json!({"data": {
        "key_version": 2, "fingerprint": "0123456789ab",
        "secret": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
    }})));
    assert!(
        !answer.is_valid(&json!({"data": {"key_version": 2, "fingerprint": "not hex at all"}}))
    );
    assert!(!answer.is_valid(
        &json!({"data": {"key_version": 2, "fingerprint": "0123456789ab", "value": "x"}})
    ));
    let state = component_validator(&root, "WebhookSigningSecretStateResponse");
    assert!(state.is_valid(
        &json!({"data": {"set": true, "key_version": 2, "fingerprint": "0123456789ab"}})
    ));
    assert!(state.is_valid(&json!({"data": {"set": false, "key_version": 0, "fingerprint": ""}})));
    assert!(
        !state.is_valid(&json!({"data": {"set": true, "key_version": 2, "fingerprint": "0123456789ab", "secret": "x"}})),
        "a read never carries the value"
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

/// The registered session group is guarded like every registered group: no credential is `401`
/// (nothing reaches the port), a malformed id is refused before the port is asked (`400`, never the
/// port's `404`), an id the port does not know is `404 auth.session_not_found` (the one answer
/// for an unknown, another user's or an already ended session), ending the current session is
/// allowed, and every answer, success or not, is never cacheable.
#[tokio::test]
async fn the_registered_session_group_is_authenticated_and_private() {
    let app = app_with_files();
    let root = parse_root();
    let gap = not_yet_served(&root);
    let registered = spot_http::api::router::routes::registry_serves();
    for id in ["auth_session_list", "auth_session_end"] {
        assert!(
            !gap.contains(id) && registered.contains(id),
            "{id} is served by the registered session group"
        );
    }
    for (method, path) in [
        ("GET", "/api/v1/auth/sessions".to_owned()),
        (
            "DELETE",
            format!("/api/v1/auth/sessions/{CONFORMANCE_SESSION_OTHER}"),
        ),
    ] {
        let none = harness::call(&app, method, &path, &[], None).await;
        none.expect_status(401, "no credential");
        assert!(none.body.get("data").is_none(), "{}", none.raw);
        assert!(
            !none.raw.contains(CONFORMANCE_SESSION_CURRENT),
            "{}",
            none.raw
        );
    }
    let list = harness::call(
        &app,
        "GET",
        "/api/v1/auth/sessions",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    list.expect_status(200, "the sessions");
    assert_eq!(list.body["data"][0]["id"], CONFORMANCE_SESSION_CURRENT);
    assert_eq!(list.body["data"][0]["current"], true);
    assert_eq!(list.header("cache-control"), Some("private, no-store"));
    // A malformed id never reaches the port: it would answer 404, the route answers 400.
    let malformed = harness::call(
        &app,
        "DELETE",
        "/api/v1/auth/sessions/bad.id",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    malformed.expect_status(400, "a malformed id");
    assert_eq!(malformed.body["error"]["code"], "api.invalid_request");
    assert_eq!(malformed.header("cache-control"), Some("private, no-store"));
    let unknown = harness::call(
        &app,
        "DELETE",
        "/api/v1/auth/sessions/ses_nobody",
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    unknown.expect_status(404, "an unknown id");
    assert_eq!(unknown.body["error"]["code"], "auth.session_not_found");
    assert_eq!(unknown.header("cache-control"), Some("private, no-store"));
    let ended = harness::call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/sessions/{CONFORMANCE_SESSION_OTHER}"),
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    ended.expect_status(204, "ending another of the user's sessions");
    assert_eq!(ended.header("cache-control"), Some("private, no-store"));
    let again = harness::call(
        &app,
        "DELETE",
        &format!("/api/v1/auth/sessions/{CONFORMANCE_SESSION_OTHER}"),
        &[harness::bearer(TOKEN_ALICE)],
        None,
    )
    .await;
    again.expect_status(404, "an ended session is gone");
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
        // SCIM is served at /scim/v2 by its own server, not a native API operation.
        if id == "scim_v2" {
            continue;
        }
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
        "auth_session_list",
        "auth_session_end",
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
        "identity_user_list",
        "identity_user_create",
        "identity_user_get",
        "identity_user_patch",
        "identity_user_disable",
        "identity_user_enable",
        "identity_user_anonymise",
        "identity_user_recover",
        "identity_invitation_list",
        "identity_invitation_revoke",
        "identity_invitation_resend",
        "identity_role_list",
        "identity_role_get",
        "identity_role_customise",
        "identity_role_upgrade_diff",
        "identity_assignment_list",
        "identity_assignment_create",
        "identity_assignment_delete",
        "identity_assignment_rules_get",
        "identity_assignment_rules_put",
        "identity_assignment_rules_preview",
        "identity_assignment_rules_run",
        "identity_api_key_list",
        "identity_api_key_create",
        "identity_api_key_delete",
        "identity_scim_credential_list",
        "identity_scim_credential_create",
        "identity_scim_credential_delete",
        "identity_session_list",
        "identity_session_end",
        "identity_user_sessions_end_all",
        "identity_impersonation_start",
        "identity_impersonation_end",
        "sso_provider_list",
        "sso_begin",
        "sso_callback",
        "sso_acs",
        "sso_complete",
        "sso_backchannel_logout",
        "sso_slo_get",
        "sso_slo_post",
        "sso_saml_metadata",
        "sso_admin_provider_list",
        "sso_admin_provider_create",
        "sso_admin_provider_get",
        "sso_admin_provider_put",
        "sso_admin_provider_delete",
        "sso_admin_provider_test",
        "sso_admin_policy_get",
        "sso_admin_policy_put",
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

/// Contract 0.16.0 (WP-CON-03.T30), additive: consumer selectors (`classes`, `entities`), the
/// `served_from` field of a pull answer and the three journal access operations with their three
/// error codes. No server yet; this pins the shape a server must keep.
#[test]
fn the_0_16_0_journal_and_selector_additions_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);

    for (id, path) in [
        ("journal_info_get", "/api/v1/journal"),
        (
            "journal_manifest_get",
            "/api/v1/journal/manifest/{generation}",
        ),
        (
            "journal_segment_get",
            "/api/v1/journal/segments/{generation}/{first_seq}-{last_seq}",
        ),
    ] {
        let op = &root["paths"][path]["get"];
        assert_eq!(op["operationId"], id, "{path}");
        assert!(op.get("security").is_none(), "{id} needs the bearer");
        let text = op["description"]
            .as_str()
            .must()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("journal_reader"), "{id} names the role");
        assert!(
            text.contains("Tenant Administrator"),
            "{id} names the administrator"
        );
        assert!(
            text.contains("grant_expired"),
            "{id} names the expired grant reason"
        );
        assert!(text.contains("501"), "{id} names the no-journal answer");
    }
    let segment =
        &root["paths"]["/api/v1/journal/segments/{generation}/{first_seq}-{last_seq}"]["get"];
    assert!(
        segment["description"]
            .as_str()
            .must()
            .contains("WHOLE TENANT")
    );
    assert!(segment["responses"]["200"]["headers"]["ETag"].is_object());
    assert!(
        segment["responses"]["206"].is_object(),
        "Range is supported"
    );

    for (code, status, retryable) in [
        ("journal.range_unavailable", 404, false),
        ("journal.commit_outcome_unknown", 503, true),
        ("journal.halted", 503, true),
    ] {
        assert_eq!(root["x-spot-error-codes"][code][0], status, "{code}");
        assert_eq!(root["x-spot-error-codes"][code][1], retryable, "{code}");
    }

    let spec = component_validator(&root, "ConsumerSpec");
    let base = json!({"topics": ["doc_posted"], "partitions": 1, "payload_mode": "ids_only"});
    assert!(
        spec.is_valid(&base),
        "a 0.15.0 consumer spec is still valid"
    );
    let mut with = base.clone();
    with["classes"] = json!(["state", "domain", "audit"]);
    with["entities"] = json!(["sales_invoice"]);
    assert!(spec.is_valid(&with));
    with["classes"] = json!(["telemetry"]);
    assert!(
        !spec.is_valid(&with),
        "telemetry is never a class a consumer can ask for"
    );
    with["classes"] = json!([]);
    assert!(!spec.is_valid(&with));
    with["classes"] = json!(["state", "state"]);
    assert!(!spec.is_valid(&with));

    let batch = component_validator(&root, "PullBatch");
    let pulled =
        json!({"events": [], "next_position": "v1.abc", "skipped": 0, "window_full": false});
    assert!(
        batch.is_valid(&pulled),
        "served_from is optional for an old server"
    );
    let mut from = pulled.clone();
    from["served_from"] = json!("journal");
    assert!(batch.is_valid(&from));
    from["served_from"] = json!("cache");
    assert!(!batch.is_valid(&from));

    let info = component_validator(&root, "JournalInfoResponse");
    assert!(info.is_valid(&json!({"data": {
        "generations": [{"generation": 1, "first_seq": 0, "last_seq": 9, "sealed_until_seq": 5, "opened_at": "2026-11-12T10:00:00Z"}],
        "policy": "sync_confirm", "region": "eu"}})));
    assert!(!info.is_valid(
        &json!({"data": {"generations": [], "policy": "x", "region": "eu", "extra": 1}})
    ));
}

/// One operation group of the contract (table driven): the operations exist where the pin says,
/// are public only when the pin says, carry the security text, and the codes are registered.
fn pin_group(
    root: &Json,
    ops: &[(&str, &str, &str, bool, bool)],
    codes: &[(&str, u16, bool)],
    schemas: &[&str],
) {
    for (id, method, path, public, step_up) in ops {
        let op = &root["paths"][*path][*method];
        assert_eq!(op["operationId"], *id, "{method} {path}");
        assert_eq!(
            op.get("security").is_some(),
            *public,
            "{id}: public only when pinned"
        );
        let text = op["description"]
            .as_str()
            .must()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["Audience:", "Permission:", "Rate class"] {
            assert!(text.contains(word), "{id} states {word}");
        }
        if *method != "get" {
            assert!(text.contains("Audited:"), "{id} names its audit");
        }
        assert_eq!(
            text.contains("`403 auth.step_up_required`"),
            *step_up,
            "{id}: step-up as pinned"
        );
        assert!(
            op["responses"]["default"].is_object(),
            "{id} has the default error"
        );
    }
    for (code, status, retryable) in codes {
        assert_eq!(root["x-spot-error-codes"][*code][0], *status, "{code}");
        assert_eq!(root["x-spot-error-codes"][*code][1], *retryable, "{code}");
    }
    for name in schemas {
        assert!(
            root["components"]["schemas"][*name].is_object(),
            "schema {name}"
        );
    }
}

/// Contract 0.17.0 (WP-CON-03.T40), additive: single sign-on (OIDC and SAML 2.0) and the provider and sign-in policy administration, plus step-up. No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_17_0_sso_and_step_up_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[
            (
                "sso_provider_list",
                "get",
                "/api/v1/auth/sso/providers",
                true,
                false,
            ),
            (
                "sso_begin",
                "post",
                "/api/v1/auth/sso/{provider}/begin",
                true,
                false,
            ),
            (
                "sso_callback",
                "get",
                "/api/v1/auth/sso/{provider}/callback",
                true,
                false,
            ),
            (
                "sso_acs",
                "post",
                "/api/v1/auth/sso/{provider}/acs",
                true,
                false,
            ),
            (
                "sso_complete",
                "post",
                "/api/v1/auth/sso/complete",
                true,
                false,
            ),
            (
                "sso_backchannel_logout",
                "post",
                "/api/v1/auth/sso/{provider}/backchannel-logout",
                true,
                false,
            ),
            (
                "sso_slo_get",
                "get",
                "/api/v1/auth/sso/{provider}/slo",
                true,
                false,
            ),
            (
                "sso_slo_post",
                "post",
                "/api/v1/auth/sso/{provider}/slo",
                true,
                false,
            ),
            (
                "sso_saml_metadata",
                "get",
                "/api/v1/auth/sso/{provider}/saml-metadata",
                true,
                false,
            ),
            ("auth_step_up", "post", "/api/v1/auth/step-up", false, false),
            (
                "sso_admin_provider_list",
                "get",
                "/api/v1/identity/sso/providers",
                false,
                false,
            ),
            (
                "sso_admin_provider_create",
                "post",
                "/api/v1/identity/sso/providers",
                false,
                true,
            ),
            (
                "sso_admin_provider_get",
                "get",
                "/api/v1/identity/sso/providers/{id}",
                false,
                false,
            ),
            (
                "sso_admin_provider_put",
                "put",
                "/api/v1/identity/sso/providers/{id}",
                false,
                true,
            ),
            (
                "sso_admin_provider_delete",
                "delete",
                "/api/v1/identity/sso/providers/{id}",
                false,
                true,
            ),
            (
                "sso_admin_provider_test",
                "post",
                "/api/v1/identity/sso/providers/{id}/test",
                false,
                true,
            ),
            (
                "sso_admin_policy_get",
                "get",
                "/api/v1/identity/sso/policy",
                false,
                false,
            ),
            (
                "sso_admin_policy_put",
                "put",
                "/api/v1/identity/sso/policy",
                false,
                true,
            ),
        ],
        &[
            ("auth.step_up_required", 403, false),
            ("sso.required", 403, false),
            ("sso.sign_in_failed", 401, false),
            ("sso.not_allowed", 403, false),
            ("sso.state_invalid", 400, false),
            ("sso.request_invalid", 400, false),
            ("sso.provider_not_found", 404, false),
            ("sso.provider_unavailable", 503, true),
            ("sso.logout_rejected", 400, false),
            ("sso.provider_invalid", 422, false),
            ("sso.would_lock_out", 409, false),
        ],
        &[
            "AuthStepUpRequest",
            "AuthStepUpResponse",
            "SsoAcsRequest",
            "SsoAdminPolicyGetResponse",
            "SsoAdminPolicyPutRequest",
            "SsoAdminPolicyPutResponse",
            "SsoAdminProviderCreateRequest",
            "SsoAdminProviderCreateResponse",
            "SsoAdminProviderGetResponse",
            "SsoAdminProviderListResponse",
            "SsoAdminProviderPutRequest",
            "SsoAdminProviderPutResponse",
            "SsoAdminProviderTestResponse",
            "SsoBackchannelLogoutRequest",
            "SsoBeginRequest",
            "SsoBeginResponse",
            "SsoCompleteResponse",
            "SsoPolicyInput",
            "SsoProviderInput",
            "SsoProviderListResponse",
            "SsoProviderPublic",
            "SsoProviderTestResult",
            "SsoProviderView",
            "SsoSamlMetadataResponse",
            "SsoSloPostRequest",
        ],
    );
}

/// Contract 0.18.0 (WP-CON-03.T41), additive: identity administration (users, invitations, roles, assignments, assignment rules, API keys, SCIM credentials, sessions, impersonation) and TOTP enrolment. No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_18_0_identity_administration_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[
            (
                "identity_user_list",
                "get",
                "/api/v1/identity/users",
                false,
                false,
            ),
            (
                "identity_user_create",
                "post",
                "/api/v1/identity/users",
                false,
                true,
            ),
            (
                "identity_user_get",
                "get",
                "/api/v1/identity/users/{id}",
                false,
                false,
            ),
            (
                "identity_user_patch",
                "patch",
                "/api/v1/identity/users/{id}",
                false,
                true,
            ),
            (
                "identity_user_disable",
                "post",
                "/api/v1/identity/users/{id}/disable",
                false,
                true,
            ),
            (
                "identity_user_enable",
                "post",
                "/api/v1/identity/users/{id}/enable",
                false,
                true,
            ),
            (
                "identity_user_anonymise",
                "post",
                "/api/v1/identity/users/{id}/anonymise",
                false,
                true,
            ),
            (
                "identity_user_recover",
                "post",
                "/api/v1/identity/users/{id}/recover",
                false,
                true,
            ),
            (
                "identity_invitation_list",
                "get",
                "/api/v1/identity/invitations",
                false,
                false,
            ),
            (
                "identity_invitation_revoke",
                "delete",
                "/api/v1/identity/invitations/{id}",
                false,
                true,
            ),
            (
                "identity_invitation_resend",
                "post",
                "/api/v1/identity/invitations/{id}/resend",
                false,
                true,
            ),
            (
                "auth_invitation_accept",
                "post",
                "/api/v1/auth/invitations/accept",
                true,
                false,
            ),
            (
                "identity_role_list",
                "get",
                "/api/v1/identity/roles",
                false,
                false,
            ),
            (
                "identity_role_get",
                "get",
                "/api/v1/identity/roles/{id}",
                false,
                false,
            ),
            (
                "identity_role_customise",
                "post",
                "/api/v1/identity/roles/{id}/customise",
                false,
                true,
            ),
            (
                "identity_role_upgrade_diff",
                "get",
                "/api/v1/identity/roles/{id}/upgrade-diff",
                false,
                false,
            ),
            (
                "identity_assignment_list",
                "get",
                "/api/v1/identity/assignments",
                false,
                false,
            ),
            (
                "identity_assignment_create",
                "post",
                "/api/v1/identity/assignments",
                false,
                true,
            ),
            (
                "identity_assignment_delete",
                "delete",
                "/api/v1/identity/assignments/{id}",
                false,
                true,
            ),
            (
                "identity_assignment_rules_get",
                "get",
                "/api/v1/identity/assignment-rules",
                false,
                false,
            ),
            (
                "identity_assignment_rules_put",
                "put",
                "/api/v1/identity/assignment-rules",
                false,
                true,
            ),
            (
                "identity_assignment_rules_preview",
                "post",
                "/api/v1/identity/assignment-rules/preview",
                false,
                false,
            ),
            (
                "identity_assignment_rules_run",
                "post",
                "/api/v1/identity/assignment-rules/run",
                false,
                true,
            ),
            (
                "identity_api_key_list",
                "get",
                "/api/v1/identity/api-keys",
                false,
                false,
            ),
            (
                "identity_api_key_create",
                "post",
                "/api/v1/identity/api-keys",
                false,
                true,
            ),
            (
                "identity_api_key_delete",
                "delete",
                "/api/v1/identity/api-keys/{id}",
                false,
                true,
            ),
            (
                "identity_scim_credential_list",
                "get",
                "/api/v1/identity/scim-credentials",
                false,
                false,
            ),
            (
                "identity_scim_credential_create",
                "post",
                "/api/v1/identity/scim-credentials",
                false,
                true,
            ),
            (
                "identity_scim_credential_delete",
                "delete",
                "/api/v1/identity/scim-credentials/{id}",
                false,
                true,
            ),
            (
                "identity_session_list",
                "get",
                "/api/v1/identity/sessions",
                false,
                false,
            ),
            (
                "identity_session_end",
                "delete",
                "/api/v1/identity/sessions/{id}",
                false,
                true,
            ),
            (
                "identity_user_sessions_end_all",
                "post",
                "/api/v1/identity/users/{id}/sessions/end-all",
                false,
                true,
            ),
            (
                "identity_impersonation_start",
                "post",
                "/api/v1/identity/impersonation",
                false,
                true,
            ),
            (
                "identity_impersonation_end",
                "delete",
                "/api/v1/identity/impersonation",
                false,
                false,
            ),
            (
                "auth_totp_enrol",
                "post",
                "/api/v1/auth/totp/enrol",
                false,
                false,
            ),
            (
                "auth_totp_confirm",
                "post",
                "/api/v1/auth/totp/confirm",
                false,
                false,
            ),
            (
                "auth_totp_disable",
                "post",
                "/api/v1/auth/totp/disable",
                false,
                false,
            ),
            (
                "auth_recovery_codes_regenerate",
                "post",
                "/api/v1/auth/recovery-codes/regenerate",
                false,
                false,
            ),
        ],
        &[
            ("identity.not_found", 404, false),
            ("identity.invalid", 422, false),
            ("identity.user_exists", 409, false),
            ("identity.last_administrator", 409, false),
            ("identity.sod_conflict", 409, false),
            ("identity.scope_exceeds_owner", 422, false),
            ("identity.invitation_invalid", 400, false),
            ("identity.impersonation_refused", 403, false),
            ("identity.totp_state", 409, false),
            ("identity.rules_invalid", 422, false),
        ],
        &[
            "AuthInvitationAcceptRequest",
            "AuthInvitationAcceptResponse",
            "AuthRecoveryCodesRegenerateRequest",
            "AuthRecoveryCodesRegenerateResponse",
            "AuthTotpConfirmRequest",
            "AuthTotpConfirmResponse",
            "AuthTotpDisableRequest",
            "AuthTotpEnrolResponse",
            "IdentityApiKey",
            "IdentityApiKeyCreateRequest",
            "IdentityApiKeyCreateResponse",
            "IdentityApiKeyListResponse",
            "IdentityAssignment",
            "IdentityAssignmentCreateRequest",
            "IdentityAssignmentCreateResponse",
            "IdentityAssignmentListResponse",
            "IdentityAssignmentRules",
            "IdentityAssignmentRulesGetResponse",
            "IdentityAssignmentRulesPreviewRequest",
            "IdentityAssignmentRulesPreviewResponse",
            "IdentityAssignmentRulesPutRequest",
            "IdentityAssignmentRulesPutResponse",
            "IdentityAssignmentRulesRunRequest",
            "IdentityAssignmentRulesRunResponse",
            "IdentityImpersonationStartRequest",
            "IdentityImpersonationStartResponse",
            "IdentityInvitation",
            "IdentityInvitationListResponse",
            "IdentityRole",
            "IdentityRoleCustomiseRequest",
            "IdentityRoleCustomiseResponse",
            "IdentityRoleGetResponse",
            "IdentityRoleListResponse",
            "IdentityRoleUpgradeDiffResponse",
            "IdentityScimCredential",
            "IdentityScimCredentialCreateResponse",
            "IdentityScimCredentialListResponse",
            "IdentitySession",
            "IdentitySessionListResponse",
            "IdentityUser",
            "IdentityUserAnonymiseRequest",
            "IdentityUserAnonymiseResponse",
            "IdentityUserCreateRequest",
            "IdentityUserCreateResponse",
            "IdentityUserDisableResponse",
            "IdentityUserEnableResponse",
            "IdentityUserGetResponse",
            "IdentityUserListResponse",
            "IdentityUserPatchRequest",
            "IdentityUserPatchResponse",
            "IdentityUserRecoverRequest",
            "IdentityUserSessionsEndAllResponse",
        ],
    );
}

/// Contract 0.19.0 (WP-CON-03.T42), additive: access governance (grant requests, segregation of duties, access reviews, the four questions, emergency access). No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_19_0_access_governance_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[
            (
                "governance_grant_request_list",
                "get",
                "/api/v1/governance/grant-requests",
                false,
                false,
            ),
            (
                "governance_grant_request_create",
                "post",
                "/api/v1/governance/grant-requests",
                false,
                false,
            ),
            (
                "governance_grant_request_decide",
                "post",
                "/api/v1/governance/grant-requests/{id}/decide",
                false,
                true,
            ),
            (
                "governance_sod_rules_get",
                "get",
                "/api/v1/governance/sod/rules",
                false,
                false,
            ),
            (
                "governance_sod_rules_put",
                "put",
                "/api/v1/governance/sod/rules",
                false,
                true,
            ),
            (
                "governance_sod_analysis",
                "get",
                "/api/v1/governance/sod/analysis",
                false,
                false,
            ),
            (
                "governance_sod_override_list",
                "get",
                "/api/v1/governance/sod/overrides",
                false,
                false,
            ),
            (
                "governance_sod_override_create",
                "post",
                "/api/v1/governance/sod/overrides",
                false,
                true,
            ),
            (
                "governance_review_list",
                "get",
                "/api/v1/governance/access-reviews",
                false,
                false,
            ),
            (
                "governance_review_create",
                "post",
                "/api/v1/governance/access-reviews",
                false,
                true,
            ),
            (
                "governance_review_get",
                "get",
                "/api/v1/governance/access-reviews/{id}",
                false,
                false,
            ),
            (
                "governance_review_items",
                "get",
                "/api/v1/governance/access-reviews/{id}/items",
                false,
                false,
            ),
            (
                "governance_review_item_decide",
                "post",
                "/api/v1/governance/access-reviews/{id}/items/{item}/decide",
                false,
                true,
            ),
            (
                "governance_review_close",
                "post",
                "/api/v1/governance/access-reviews/{id}/close",
                false,
                true,
            ),
            (
                "governance_review_report",
                "get",
                "/api/v1/governance/access-reviews/{id}/report",
                false,
                false,
            ),
            (
                "governance_question_user_permissions",
                "get",
                "/api/v1/governance/questions/user-permissions",
                false,
                false,
            ),
            (
                "governance_question_who_can",
                "get",
                "/api/v1/governance/questions/who-can",
                false,
                false,
            ),
            (
                "governance_question_who_can_see",
                "get",
                "/api/v1/governance/questions/who-can-see",
                false,
                false,
            ),
            (
                "governance_question_changes",
                "get",
                "/api/v1/governance/questions/changes",
                false,
                false,
            ),
            (
                "governance_emergency_create",
                "post",
                "/api/v1/governance/emergency-access",
                false,
                true,
            ),
            (
                "governance_emergency_list",
                "get",
                "/api/v1/governance/emergency-access",
                false,
                false,
            ),
            (
                "governance_emergency_review",
                "post",
                "/api/v1/governance/emergency-access/{id}/review",
                false,
                true,
            ),
        ],
        &[
            ("governance.not_found", 404, false),
            ("governance.invalid", 422, false),
            ("governance.self_approval", 403, false),
            ("governance.request_not_pending", 409, false),
            ("governance.override_invalid", 422, false),
            ("governance.review_closed", 409, false),
            ("governance.not_reviewer", 403, false),
            ("governance.controller_is_beneficiary", 403, false),
        ],
        &[
            "GovernanceEmergency",
            "GovernanceEmergencyCreateRequest",
            "GovernanceEmergencyCreateResponse",
            "GovernanceEmergencyListResponse",
            "GovernanceEmergencyReviewRequest",
            "GovernanceEmergencyReviewResponse",
            "GovernanceGrantRequest",
            "GovernanceGrantRequestCreateRequest",
            "GovernanceGrantRequestCreateResponse",
            "GovernanceGrantRequestDecideRequest",
            "GovernanceGrantRequestDecideResponse",
            "GovernanceGrantRequestListResponse",
            "GovernanceQuestionChangesResponse",
            "GovernanceQuestionUserPermissionsResponse",
            "GovernanceQuestionWhoCanResponse",
            "GovernanceQuestionWhoCanSeeResponse",
            "GovernanceReview",
            "GovernanceReviewCloseResponse",
            "GovernanceReviewCreateRequest",
            "GovernanceReviewCreateResponse",
            "GovernanceReviewGetResponse",
            "GovernanceReviewItem",
            "GovernanceReviewItemDecideRequest",
            "GovernanceReviewItemDecideResponse",
            "GovernanceReviewItemsResponse",
            "GovernanceReviewListResponse",
            "GovernanceReviewReportResponse",
            "GovernanceSodAnalysisResponse",
            "GovernanceSodOverride",
            "GovernanceSodOverrideCreateRequest",
            "GovernanceSodOverrideCreateResponse",
            "GovernanceSodOverrideListResponse",
            "GovernanceSodRules",
            "GovernanceSodRulesGetResponse",
            "GovernanceSodRulesPutRequest",
            "GovernanceSodRulesPutResponse",
        ],
    );
}

/// Contract 0.20.0 (WP-CON-03.T43), additive: security administration (data policy and retention, protected groups and node keys, support access consent, audit chain and compliance evidence). No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_20_0_security_administration_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[
            (
                "security_data_policy_get",
                "get",
                "/api/v1/security/data-policy",
                false,
                false,
            ),
            (
                "security_data_policy_put",
                "put",
                "/api/v1/security/data-policy",
                false,
                true,
            ),
            (
                "security_retention_run_list",
                "get",
                "/api/v1/security/retention/runs",
                false,
                false,
            ),
            (
                "security_retention_run_create",
                "post",
                "/api/v1/security/retention/runs",
                false,
                true,
            ),
            (
                "security_retention_hold_list",
                "get",
                "/api/v1/security/retention/holds",
                false,
                false,
            ),
            (
                "security_retention_hold_create",
                "post",
                "/api/v1/security/retention/holds",
                false,
                true,
            ),
            (
                "security_retention_hold_delete",
                "delete",
                "/api/v1/security/retention/holds/{id}",
                false,
                true,
            ),
            (
                "security_retention_proof_list",
                "get",
                "/api/v1/security/retention/proofs",
                false,
                false,
            ),
            (
                "security_residency_check",
                "post",
                "/api/v1/security/residency/check",
                false,
                false,
            ),
            (
                "security_protected_group_list",
                "get",
                "/api/v1/security/protected-groups",
                false,
                false,
            ),
            (
                "security_protected_group_sync_class",
                "put",
                "/api/v1/security/protected-groups/{group}/sync-class",
                false,
                true,
            ),
            (
                "security_node_key_list",
                "get",
                "/api/v1/security/node-keys",
                false,
                false,
            ),
            (
                "security_node_key_revoke",
                "post",
                "/api/v1/security/node-keys/{node}/revoke",
                false,
                true,
            ),
            (
                "security_support_request_list",
                "get",
                "/api/v1/security/support-access/requests",
                false,
                false,
            ),
            (
                "security_support_request_consent",
                "post",
                "/api/v1/security/support-access/requests/{id}/consent",
                false,
                true,
            ),
            (
                "security_support_request_revoke",
                "post",
                "/api/v1/security/support-access/requests/{id}/revoke",
                false,
                true,
            ),
            (
                "security_support_session_list",
                "get",
                "/api/v1/security/support-access/sessions",
                false,
                false,
            ),
            (
                "security_support_session_recording",
                "get",
                "/api/v1/security/support-access/sessions/{id}/recording",
                false,
                true,
            ),
            (
                "security_audit_chain_verify",
                "post",
                "/api/v1/security/audit/chain/verify",
                false,
                false,
            ),
            (
                "security_audit_entries",
                "get",
                "/api/v1/security/audit/entries",
                false,
                false,
            ),
            (
                "security_audit_export",
                "get",
                "/api/v1/security/audit/export",
                false,
                true,
            ),
            (
                "security_compliance_evidence_pack",
                "post",
                "/api/v1/security/compliance/evidence-pack",
                false,
                false,
            ),
            (
                "security_compliance_controls",
                "get",
                "/api/v1/security/compliance/controls",
                false,
                false,
            ),
        ],
        &[
            ("security.not_found", 404, false),
            ("security.invalid", 422, false),
            ("security.plan_required", 422, false),
            ("security.plan_changed", 409, false),
            ("security.consent_invalid", 422, false),
            ("security.request_not_pending", 409, false),
        ],
        &[
            "SecurityAuditChainVerifyRequest",
            "SecurityAuditChainVerifyResponse",
            "SecurityAuditEntriesResponse",
            "SecurityAuditExportResponse",
            "SecurityComplianceControlsResponse",
            "SecurityComplianceEvidencePackRequest",
            "SecurityComplianceEvidencePackResponse",
            "SecurityDataPolicy",
            "SecurityDataPolicyGetResponse",
            "SecurityDataPolicyPutRequest",
            "SecurityDataPolicyPutResponse",
            "SecurityNodeKey",
            "SecurityNodeKeyListResponse",
            "SecurityNodeKeyRevokeResponse",
            "SecurityProtectedGroup",
            "SecurityProtectedGroupListResponse",
            "SecurityProtectedGroupSyncClassRequest",
            "SecurityProtectedGroupSyncClassResponse",
            "SecurityResidencyCheckRequest",
            "SecurityResidencyCheckResponse",
            "SecurityRetentionHold",
            "SecurityRetentionHoldCreateRequest",
            "SecurityRetentionHoldCreateResponse",
            "SecurityRetentionHoldListResponse",
            "SecurityRetentionProofListResponse",
            "SecurityRetentionRun",
            "SecurityRetentionRunCreateRequest",
            "SecurityRetentionRunCreateResponse",
            "SecurityRetentionRunListResponse",
            "SecuritySupportRequest",
            "SecuritySupportRequestConsentRequest",
            "SecuritySupportRequestConsentResponse",
            "SecuritySupportRequestListResponse",
            "SecuritySupportRequestRevokeResponse",
            "SecuritySupportSession",
            "SecuritySupportSessionListResponse",
            "SecuritySupportSessionRecordingResponse",
        ],
    );
}

/// Contract 0.21.0 (WP-CON-03.T50), additive: the data import operations (upload, dry run, run with progress, errors, cancel). No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_21_0_data_import_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[
            (
                "import_limits",
                "get",
                "/api/v1/import/limits",
                false,
                false,
            ),
            ("import_create", "post", "/api/v1/import", false, false),
            ("import_get", "get", "/api/v1/import/{id}", false, false),
            (
                "import_errors",
                "get",
                "/api/v1/import/{id}/errors",
                false,
                false,
            ),
            (
                "import_run",
                "post",
                "/api/v1/import/{id}/run",
                false,
                false,
            ),
            (
                "import_cancel",
                "post",
                "/api/v1/import/{id}/cancel",
                false,
                false,
            ),
        ],
        &[
            ("import.file_unreadable", 422, false),
            ("import.mapping_invalid", 422, false),
            ("import.limit_exceeded", 413, false),
            ("import.not_ready", 409, false),
            ("import.digest_changed", 409, false),
            ("import.cancelled", 409, false),
            ("import.right_missing", 403, false),
        ],
        &[
            "ImportCancelResponse",
            "ImportCreateRequest",
            "ImportCreateResponse",
            "ImportErrorsResponse",
            "ImportGetResponse",
            "ImportJob",
            "ImportLimitsResponse",
            "ImportMapping",
            "ImportRunRequest",
            "ImportRunResponse",
        ],
    );
}

/// Contract 0.22.0 (WP-CON-03.T51), additive: full-text search on the resource query operation (search, rank order, facets, snippets) and the searchable and facetable fields in the metadata. No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_22_0_search_additions_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[],
        &[
            ("search.unsupported", 501, false),
            ("search.term_too_long", 422, false),
            ("search.field_not_searchable", 422, false),
            ("search.projection_behind", 503, true),
        ],
        &[],
    );
}

/// Contract 0.23.0 (WP-CON-03.T52), additive: booking holds (claim, confirm, release) and availability over the occupancy entity. No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_23_0_booking_hold_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[
            (
                "occupancy_claim",
                "post",
                "/api/v1/occupancy/{entity}/holds",
                false,
                false,
            ),
            (
                "occupancy_confirm",
                "post",
                "/api/v1/occupancy/{entity}/holds/{id}/confirm",
                false,
                false,
            ),
            (
                "occupancy_release",
                "delete",
                "/api/v1/occupancy/{entity}/holds/{id}",
                false,
                false,
            ),
            (
                "occupancy_availability",
                "get",
                "/api/v1/occupancy/{entity}/availability",
                false,
                false,
            ),
        ],
        &[
            ("occupancy.conflict", 409, false),
            ("occupancy.hold_expired", 409, false),
            ("occupancy.invalid_range", 422, false),
            ("occupancy.ttl_too_long", 422, false),
            ("occupancy.not_holder", 403, false),
        ],
        &[
            "OccupancyAvailabilityResponse",
            "OccupancyClaimRequest",
            "OccupancyClaimResponse",
            "OccupancyConfirmResponse",
            "OccupancyHold",
        ],
    );
}

/// Contract 0.24.0 (WP-CON-03.T53), additive: the MCP endpoint contract (contracts/native-api/mcp.md) and its pointer in openapi.yaml. No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_24_0_mcp_contract_document_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(&root, &[], &[], &[]);
}

/// Contract 0.25.0 (WP-CON-03.T54), additive: the installed apps list and the connector operations (list, get, invoke, run log, inbound). No server yet; this pins the shape a
/// server must keep.
#[test]
fn the_0_25_0_app_and_connector_operations_are_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[
            ("app_list", "get", "/api/v1/apps", false, false),
            ("connector_list", "get", "/api/v1/connectors", false, false),
            (
                "connector_get",
                "get",
                "/api/v1/connectors/{name}",
                false,
                false,
            ),
            (
                "connector_invoke",
                "post",
                "/api/v1/connectors/{name}/operations/{op}",
                false,
                false,
            ),
            (
                "connector_runs",
                "get",
                "/api/v1/connectors/{name}/runs",
                false,
                false,
            ),
            (
                "connector_inbound",
                "post",
                "/api/v1/connectors/{name}/inbound",
                true,
                false,
            ),
        ],
        &[
            ("connector.not_found", 404, false),
            ("connector.disabled", 409, false),
            ("connector.operation_unknown", 404, false),
            ("connector.input_invalid", 422, false),
            ("connector.upstream_failed", 502, true),
            ("connector.deadline_exceeded", 504, false),
            ("connector.signature_invalid", 401, false),
            ("connector.replayed", 409, false),
        ],
        &[
            "AppListResponse",
            "Connector",
            "ConnectorGetResponse",
            "ConnectorInboundRequest",
            "ConnectorInboundResponse",
            "ConnectorInvokeRequest",
            "ConnectorInvokeResponse",
            "ConnectorListResponse",
            "ConnectorRunsResponse",
            "InstalledApp",
        ],
    );
}

/// Constant-answer fakes for the identity administration group (the shapes of
/// `tests/identity_routes.rs`, without its recording): every operation answers one valid value.
mod identity_fake {
    use spot_http::api::ports::identity::*;
    use spot_http::api::service::{BoxFuture, RequestCtx};

    pub const U1: &str = "0190a1b2-c3d4-7e5f-8a6b-1c2d3e4f5a6b";
    pub const U2: &str = "0190a1b2-c3d4-7e5f-8a6b-1c2d3e4f5a6c";
    const SECRET: &str = "conformance-secret-value";

    /// One value implementing the admin, permission, step-up and audit ports.
    pub struct IdFake;

    /// A fresh fake (it holds no state).
    pub fn fake() -> std::sync::Arc<IdFake> {
        std::sync::Arc::new(IdFake)
    }

    #[rustfmt::skip]
    fn user() -> UserOut {
        UserOut { id: U1.into(), email: "a@acme.com".into(), full_name: "Ayesha".into(), audience: "staff".into(), status: "active".into(), roles: vec!["Manager".into()], second_factor: true, last_sign_in_at: None, created_at: "2026-11-01T08:00:00Z".into(), version: 7 }
    }
    #[rustfmt::skip]
    fn role() -> RoleOut {
        RoleOut { id: U1.into(), name: "Manager".into(), description: None, builtin: true, customised_from: None, permissions: vec!["a.b".into()], version: 2 }
    }
    #[rustfmt::skip]
    fn assignment() -> AssignmentOut {
        AssignmentOut { id: U1.into(), user_id: U1.into(), role_id: U2.into(), scope: None, granted_by: U2.into(), granted_at: "2026-11-01T08:00:00Z".into(), expires_at: None, source: Some("direct".into()) }
    }
    #[rustfmt::skip]
    fn api_key() -> ApiKeyOut {
        ApiKeyOut { id: U1.into(), name: "ci".into(), owner_id: U2.into(), scopes: vec!["read".into()], created_at: "2026-11-01T08:00:00Z".into(), last_used_at: None, expires_at: None, prefix: Some("sk_ab12".into()) }
    }
    fn page<T>(items: Vec<T>) -> Page<T> {
        Page {
            items,
            next_cursor: None,
        }
    }

    macro_rules! ok {
        ($self:ident, $ctx:ident, $op:expr, $val:expr) => {{
            let _ = ($self, $ctx, $op);
            Box::pin(async move { Ok($val) })
        }};
    }

    #[rustfmt::skip]
    impl UserAdmin for IdFake {
        fn user_list<'a>(&'a self, ctx: &'a RequestCtx, _q: UserListIn) -> BoxFuture<'a, IdResult<Page<UserOut>>> {
            ok!(self, ctx, "user_list", page(vec![user()]))
        }
        fn user_create<'a>(&'a self, ctx: &'a RequestCtx, _i: UserCreateIn) -> BoxFuture<'a, IdResult<UserOut>> {
            ok!(self, ctx, "user_create", user())
        }
        fn user_get<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<UserOut>> {
            ok!(self, ctx, "user_get", user())
        }
        fn user_patch<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str, _v: u64, _p: UserPatchIn) -> BoxFuture<'a, IdResult<UserOut>> {
            ok!(self, ctx, "user_patch", user())
        }
        fn user_disable<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<UserChangeOut>> {
            ok!(
                self,
                ctx,
                "user_disable",
                UserChangeOut {
                    user: user(),
                    leaver_actions: vec![("sessions_ended".into(), 2)]
                }
            )
        }
        fn user_enable<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<UserChangeOut>> {
            ok!(
                self,
                ctx,
                "user_enable",
                UserChangeOut {
                    user: user(),
                    leaver_actions: vec![]
                }
            )
        }
        fn user_anonymise<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str, _c: String) -> BoxFuture<'a, IdResult<UserOut>> {
            ok!(self, ctx, "user_anonymise", user())
        }
        fn user_recover<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str, _k: String) -> BoxFuture<'a, IdResult<RecoverOut>> {
            ok!(self, ctx, "user_recover", RecoverOut::NeedsApproval)
        }
        fn invitation_list<'a>(&'a self, ctx: &'a RequestCtx, _p: PageIn) -> BoxFuture<'a, IdResult<Page<InvitationOut>>> {
            ok!(
                self,
                ctx,
                "invitation_list",
                page(vec![InvitationOut {
                    id: U1.into(),
                    user_id: U2.into(),
                    email: "a@acme.com".into(),
                    status: "pending".into(),
                    expires_at: "2026-11-09T08:00:00Z".into(),
                    sent_at: "2026-11-02T08:00:00Z".into(),
                }])
            )
        }
        fn invitation_revoke<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
            ok!(self, ctx, "invitation_revoke", ())
        }
        fn invitation_resend<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
            ok!(self, ctx, "invitation_resend", ())
        }
    }

    #[rustfmt::skip]
    impl RoleAdmin for IdFake {
        fn role_list<'a>(&'a self, ctx: &'a RequestCtx, _p: PageIn) -> BoxFuture<'a, IdResult<Page<RoleOut>>> {
            ok!(self, ctx, "role_list", page(vec![role()]))
        }
        fn role_get<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<RoleOut>> {
            ok!(self, ctx, "role_get", role())
        }
        fn role_customise<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str, _n: String) -> BoxFuture<'a, IdResult<RoleOut>> {
            ok!(self, ctx, "role_customise", role())
        }
        fn role_upgrade_diff<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<UpgradeDiffOut>> {
            ok!(
                self,
                ctx,
                "role_upgrade_diff",
                UpgradeDiffOut {
                    added: vec!["x".into()],
                    removed: vec![],
                    from_version: 1,
                    to_version: 2
                }
            )
        }
    }

    #[rustfmt::skip]
    impl AssignmentAdmin for IdFake {
        fn assignment_list<'a>(&'a self, ctx: &'a RequestCtx, _q: AssignmentListIn) -> BoxFuture<'a, IdResult<Page<AssignmentOut>>> {
            ok!(self, ctx, "assignment_list", page(vec![assignment()]))
        }
        fn assignment_create<'a>(&'a self, ctx: &'a RequestCtx, _i: AssignmentCreateIn) -> BoxFuture<'a, IdResult<GrantOut>> {
                    ok!(
                self,
                ctx,
                "assignment_create",
                GrantOut::Granted(assignment())
            )
        }
        fn assignment_delete<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
            ok!(self, ctx, "assignment_delete", ())
        }
        fn rules_get<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<RulesOut>> {
            ok!(
                self,
                ctx,
                "rules_get",
                RulesOut {
                    rules: vec![],
                    version: 3
                }
            )
        }
        fn rules_put<'a>(&'a self, ctx: &'a RequestCtx, v: u64, rules: Vec<RuleIn>) -> BoxFuture<'a, IdResult<RulesOut>> {
            ok!(
                self,
                ctx,
                "rules_put",
                RulesOut {
                    rules,
                    version: v + 1
                }
            )
        }
        fn rules_preview<'a>(&'a self, ctx: &'a RequestCtx, _r: Option<Vec<RuleIn>>) -> BoxFuture<'a, IdResult<PlanOut>> {
            ok!(
                self,
                ctx,
                "rules_preview",
                PlanOut {
                    grants: vec![(U1.into(), U2.into(), None)],
                    revokes: vec![],
                    truncated: false,
                    plan_digest: "abc123".into()
                }
            )
        }
        fn rules_run<'a>(&'a self, ctx: &'a RequestCtx, _d: String) -> BoxFuture<'a, IdResult<RunOut>> {
            ok!(
                self,
                ctx,
                "rules_run",
                RunOut {
                    granted: 1,
                    revoked: 0,
                    refused: 0
                }
            )
        }
    }

    #[rustfmt::skip]
    impl KeyAdmin for IdFake {
        fn api_key_list<'a>(&'a self, ctx: &'a RequestCtx, _p: PageIn) -> BoxFuture<'a, IdResult<Page<ApiKeyOut>>> {
            ok!(self, ctx, "api_key_list", page(vec![api_key()]))
        }
        fn api_key_create<'a>(&'a self, ctx: &'a RequestCtx, _i: ApiKeyCreateIn) -> BoxFuture<'a, IdResult<NewApiKey>> {
            ok!(
                self,
                ctx,
                "api_key_create",
                NewApiKey {
                    key: api_key(),
                    secret: SECRET.into()
                }
            )
        }
        fn api_key_delete<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
            ok!(self, ctx, "api_key_delete", ())
        }
        fn scim_list<'a>(&'a self, ctx: &'a RequestCtx, _p: PageIn) -> BoxFuture<'a, IdResult<Page<ScimOut>>> {
            ok!(self, ctx, "scim_list", page(vec![scim()]))
        }
        fn scim_create<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<NewScimCredential>> {
            ok!(
                self,
                ctx,
                "scim_create",
                NewScimCredential {
                    credential: scim(),
                    secret: SECRET.into()
                }
            )
        }
        fn scim_delete<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
            ok!(self, ctx, "scim_delete", ())
        }
    }

    fn scim() -> ScimOut {
        ScimOut {
            id: U1.into(),
            created_at: "2026-11-01T08:00:00Z".into(),
            last_used_at: None,
            prefix: Some("scim_ab".into()),
        }
    }

    #[rustfmt::skip]
    impl SessionAdmin for IdFake {
        fn session_list<'a>(&'a self, ctx: &'a RequestCtx, _q: SessionListIn) -> BoxFuture<'a, IdResult<Page<AdminSessionOut>>> {
            ok!(
                self,
                ctx,
                "session_list",
                page(vec![AdminSessionOut {
                    id: U1.into(),
                    user_id: U2.into(),
                    created_at: "2026-11-01T08:00:00Z".into(),
                    last_seen_at: None,
                    client: None,
                    method: Some("password".into()),
                    impersonated_by: None,
                }])
            )
        }
        fn session_end<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
            ok!(self, ctx, "session_end", ())
        }
        fn sessions_end_all<'a>(&'a self, ctx: &'a RequestCtx, _u: &'a str) -> BoxFuture<'a, IdResult<u64>> {
            ok!(self, ctx, "sessions_end_all", 4)
        }
        fn impersonation_start<'a>(&'a self, ctx: &'a RequestCtx, _i: ImpersonationIn) -> BoxFuture<'a, IdResult<String>> {
            ok!(
                self,
                ctx,
                "impersonation_start",
                "2026-11-12T10:00:00Z".to_owned()
            )
        }
        fn impersonation_end<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<()>> {
            ok!(self, ctx, "impersonation_end", ())
        }
    }

    #[rustfmt::skip]
    impl IdentityAdminPort for IdFake {
        fn users(&self) -> Option<&dyn UserAdmin> {
            Some(self as &dyn UserAdmin)
        }
        fn roles(&self) -> Option<&dyn RoleAdmin> {
            Some(self as &dyn RoleAdmin)
        }
        fn assignments(&self) -> Option<&dyn AssignmentAdmin> {
            Some(self as &dyn AssignmentAdmin)
        }
        fn keys(&self) -> Option<&dyn KeyAdmin> {
            Some(self as &dyn KeyAdmin)
        }
        fn sessions(&self) -> Option<&dyn SessionAdmin> {
            Some(self as &dyn SessionAdmin)
        }
    }

    #[rustfmt::skip]
    impl IdentityPermissionPort for IdFake {
        fn check<'a>(&'a self, _ctx: &'a RequestCtx, _p: IdentityPermission) -> BoxFuture<'a, IdResult<IdentityCaller>> {
            Box::pin(async move {
                Ok(IdentityCaller { user_id: U2.into(), audience: CallerAudience::Staff })
            })
        }
    }

    #[rustfmt::skip]
    impl IdentityStepUpPort for IdFake {
        fn is_recent<'a>(&'a self, _ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<bool>> {
            Box::pin(async move { Ok(true) })
        }
    }

    #[rustfmt::skip]
    impl IdentityAuditPort for IdFake {
        fn record<'a>(&'a self, _ctx: &'a RequestCtx, entry: AuditEntry) -> BoxFuture<'a, IdResult<()>> {
            Box::pin(async move { let _ = entry; Ok(()) })
        }
    }
}

/// Contract-valid fakes of the two single sign-on ports (the shapes of `tests/sso_routes.rs`,
/// without its recording): the begin attempt is bound by the cookie `bind-1`, the callback and the
/// assertion consumer hand over `hand-1`, `complete` mints one session.
mod sso_fake {
    use serde_json::{Map, Value, json};
    use spot_http::api::ports::sso::*;
    use spot_http::api::service::{ApiError, BoxFuture, RequestCtx, Session, UserInfo};

    /// The public port.
    pub struct SsoFake;

    /// The administration port: step-up is always recent, every answer is contract-valid.
    pub struct SsoAdminFake;

    fn handover(location: &str) -> Handover {
        Handover {
            location: location.into(),
            cookie: "hand-1".into(),
        }
    }

    #[rustfmt::skip]
    impl SsoPort for SsoFake {
        fn providers<'a>(&'a self, _ctx: &'a RequestCtx) -> BoxFuture<'a, Result<ProviderList, SsoFailure>> {
            Box::pin(async move {
                Ok(ProviderList { providers: vec![ProviderPublic { id: "okta".into(), name: "Okta".into(), kind: "oidc".into() }], password_login: true })
            })
        }
        fn begin<'a>(&'a self, _ctx: &'a RequestCtx, _provider: &'a str, _return_to: Option<&'a str>) -> BoxFuture<'a, Result<Begin, SsoFailure>> {
            Box::pin(async move {
                Ok(Begin { redirect_url: "https://idp.example.com/authorize?x=1".into(), binding: "bind-1".into() })
            })
        }
        fn callback<'a>(&'a self, _ctx: &'a RequestCtx, _provider: &'a str, _params: &'a CallbackParams) -> BoxFuture<'a, Result<Handover, SsoFailure>> {
            Box::pin(async move { Ok(handover("/desk")) })
        }
        fn acs<'a>(&'a self, _ctx: &'a RequestCtx, _provider: &'a str, _params: &'a AcsParams) -> BoxFuture<'a, Result<Handover, SsoFailure>> {
            Box::pin(async move { Ok(handover("/")) })
        }
        fn complete<'a>(&'a self, _ctx: &'a RequestCtx, _handover: &'a str) -> BoxFuture<'a, Result<SignedIn, SsoFailure>> {
            Box::pin(async move {
                Ok(SignedIn {
                    session: Session {
                        token: "tok-conformance-sso".into(),
                        expires_at: "2026-11-12T20:00:00Z".into(),
                        user: UserInfo { id: spot_storage_contract::RecordId::new_v7(), email: "a@acme.example".into(), full_name: "A".into() },
                    },
                })
            })
        }
        fn backchannel_logout<'a>(&'a self, _ctx: &'a RequestCtx, _provider: &'a str, _logout_token: &'a str) -> BoxFuture<'a, Result<(), SsoFailure>> {
            Box::pin(async move { Ok(()) })
        }
        fn single_logout<'a>(&'a self, _ctx: &'a RequestCtx, _provider: &'a str, params: &'a SloParams) -> BoxFuture<'a, Result<SloOutcome, SsoFailure>> {
            // The redirect binding answers 303 to the provider; the POST binding is `200`, as the contract lists.
            let redirect = params.binding == SloBinding::Redirect;
            Box::pin(async move {
                Ok(if redirect { SloOutcome::Redirect("https://idp.example.com/slo".into()) } else { SloOutcome::Done })
            })
        }
        fn saml_metadata<'a>(&'a self, _ctx: &'a RequestCtx, _provider: &'a str) -> BoxFuture<'a, Result<String, SsoFailure>> {
            Box::pin(async move { Ok("<EntityDescriptor/>".to_owned()) })
        }
    }

    fn provider() -> Value {
        json!({"id": "p1", "kind": "oidc", "name": "Okta", "audience": ["staff"], "enabled": true,
               "issuer": "https://idp.example.com", "client_id": "cid", "client_secret_set": false,
               "scopes": ["openid"], "jit": "off", "version": 3})
    }

    fn one(data: Value, etag: Option<&str>) -> Result<AdminReply, ApiError> {
        Ok(AdminReply {
            data,
            etag: etag.map(str::to_owned),
            next_cursor: None,
        })
    }

    #[rustfmt::skip]
    impl SsoAdminPort for SsoAdminFake {
        fn step_up_current<'a>(&'a self, _ctx: &'a RequestCtx) -> BoxFuture<'a, Result<bool, ApiError>> {
            Box::pin(async move { Ok(true) })
        }
        fn list_providers<'a>(&'a self, _ctx: &'a RequestCtx, _page: &'a AdminPage) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
            Box::pin(async move { Ok(AdminReply { data: json!([provider()]), etag: None, next_cursor: Some(None) }) })
        }
        fn create_provider<'a>(&'a self, _ctx: &'a RequestCtx, _key: &'a str, _body: &'a Map<String, Value>) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
            Box::pin(async move { one(provider(), Some("\"3\"")) })
        }
        fn get_provider<'a>(&'a self, _ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
            Box::pin(async move { one(provider(), Some("\"3\"")) })
        }
        fn put_provider<'a>(&'a self, _ctx: &'a RequestCtx, _id: &'a str, _if_match: &'a str, _body: &'a Map<String, Value>) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
            Box::pin(async move { one(provider(), Some("\"4\"")) })
        }
        fn delete_provider<'a>(&'a self, _ctx: &'a RequestCtx, _id: &'a str, _if_match: &'a str) -> BoxFuture<'a, Result<(), ApiError>> {
            Box::pin(async move { Ok(()) })
        }
        fn test_provider<'a>(&'a self, _ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
            Box::pin(async move { one(json!({"ok": true, "checks": [{"name": "discovery", "ok": true, "code": null}]}), None) })
        }
        fn get_policy<'a>(&'a self, _ctx: &'a RequestCtx) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
            Box::pin(async move { one(json!({"mode": "sso_optional", "break_glass_users": [], "version": 1}), Some("\"1\"")) })
        }
        fn put_policy<'a>(&'a self, _ctx: &'a RequestCtx, _if_match: &'a str, _body: &'a Map<String, Value>) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
            Box::pin(async move { one(json!({"mode": "sso_optional", "break_glass_users": [], "version": 2}), None) })
        }
    }
}

/// Contract 0.26.0 (WP-CON-03.T67), additive: the external peer's inbound operation `link_inbound` (public, signed, one uniform `401`). No server group yet; this pins the shape the
/// route file of WP-CTL-11.T4 must keep once it becomes a registry group.
#[test]
fn the_0_26_0_link_inbound_operation_is_additive_and_pinned() {
    let root = parse_root();
    assert_eq!(root["info"]["version"], "0.26.1");
    assert_eq!(api_operations(&root).len(), 189);
    assert_eq!(root["x-spot-error-codes"].as_object().must().len(), 149);
    pin_group(
        &root,
        &[(
            "link_inbound",
            "post",
            "/api/v1/link/inbound/{peer}",
            true,
            false,
        )],
        &[("link_inbound.unauthorized", 401, false)],
        &[
            "LinkInboundRequest",
            "LinkInboundResponse",
            "LinkInboundError",
        ],
    );
    let op = &root["paths"]["/api/v1/link/inbound/{peer}"]["post"];
    for status in ["202", "401", "429", "default"] {
        assert!(op["responses"][status].is_object(), "answers {status}");
    }
    assert!(
        root["x-spot-error-codes"]["api.rate_limited"].is_array(),
        "the 429 reuses api.rate_limited"
    );
}

const INVITATION_TOKEN: &str = "tok_0123456789abcdefghij";

fn conformance_journal()
-> std::sync::Arc<dyn spot_http::api::ports::journal_access::JournalAccessPort> {
    use spot_http::api::ports::journal_access::{
        JournalGeneration, JournalInfo, MemoryJournalAccess,
    };
    let port = std::sync::Arc::new(MemoryJournalAccess::new());
    port.allow(TOKEN_ALICE);
    port.set_info(JournalInfo {
        generations: vec![JournalGeneration {
            generation: 1,
            first_seq: 1,
            last_seq: 200,
            sealed_until_seq: 200,
            opened_at: "2026-11-01T00:00:00Z".to_owned(),
        }],
        policy: "standard".to_owned(),
        region: "eu".to_owned(),
    });
    port.set_manifest("1", json!({"generation": 1, "segments": []}));
    port.set_segment("1", "1-100", (0u8..=255).collect());
    port
}

/// An external-link inbound port that admits and accepts every call.
struct ConformanceLinkInbound;

impl spot_http::api::ports::link_inbound::LinkInboundPort for ConformanceLinkInbound {
    fn admit(
        &self,
        _call: &spot_http::api::ports::link_inbound::InboundCall,
    ) -> Result<usize, spot_http::api::ports::link_inbound::LinkInboundRefusal> {
        Ok(1024)
    }

    fn receive<'a>(
        &'a self,
        _call: spot_http::api::ports::link_inbound::InboundCall,
        _body: axum::body::Bytes,
    ) -> spot_http::api::service::BoxFuture<
        'a,
        Result<(), spot_http::api::ports::link_inbound::LinkInboundRefusal>,
    > {
        Box::pin(async { Ok(()) })
    }
}
