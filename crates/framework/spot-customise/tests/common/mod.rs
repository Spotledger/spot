#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! The rig the suites share: a site with a few app DocTypes, a fake storage backend under a real
//! document engine, the real native API router over it (reading metadata from the live snapshot
//! on every request), and a `CustomiseService` wired to all of it.

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value as Json, json};
use spot_api_services::{EngineResources, MetaResources};
use spot_app_api::context::Actor;
use spot_core::{ManualClock, SiteId};
use spot_customise::{
    Caller, CustomiseService, Fill, ListenerFuture, MemoryHost, MemoryRecordStore, Parts,
    RefusedStep, SchemaBackend, SchemaFuture, SchemaPlan, StorageSchema, SwapListener,
};
use spot_document::engine::{Engine, V7Ids};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::registry::Code;
use spot_http::api::router::{ApiServices, router};
use spot_http::api::service::*;
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_meta::{BuildOptions, Definition};
use spot_meta_live::{
    Follower, InvalidationBus, LiveMeta, MemoryVersionStore, MetaLoader, PollSchedule, PollingBus,
    Snapshot, Version, VersionStore,
};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{Catalog, Collation, RecordId};
use spot_storage_fake::FakeBackend;
use tower::ServiceExt;

pub const PARTY: &str = r#"
name = "party"
label = "Party"
title_field = "party_name"
[[fields]]
name = "party_name"
type = "text"
required = true
max_chars = 140
"#;

pub const INVOICE: &str = r#"
name = "invoice"
label = "Invoice"
title_field = "title"
[[fields]]
name = "title"
type = "text"
required = true
max_chars = 80
[[fields]]
name = "amount"
type = "decimal"
scale = 2
precision = 12
default = "0.00"
[[fields]]
name = "party"
type = "link"
options = "party"
[[fields]]
name = "ref_party"
type = "text"
fetch_from = "party.party_name"
permlevel = 2
[[fields]]
name = "tax_id"
type = "text"
max_chars = 30
protection_group = "secret"
classification = ["pii"]
[[protection_groups]]
name = "secret"
label = "Secret"
"#;

/// A custom DocType with a link, a child table and a select.
pub const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
title_field = "subject"
[[fields]]
name = "subject"
type = "text"
required = true
max_chars = 120
[[fields]]
name = "priority"
type = "select"
options = "low\nnormal\nurgent"
default = "normal"
[[fields]]
name = "customer"
type = "link"
options = "party"
[[fields]]
name = "notes"
type = "table"
options = "ticket_note"
"#;

pub const TICKET_NOTE: &str = r#"
name = "ticket_note"
is_child = true
[[fields]]
name = "text"
type = "text"
max_chars = 500
"#;

pub fn base_definitions() -> Vec<Definition> {
    [("party", PARTY), ("invoice", INVOICE)]
        .into_iter()
        .map(|(name, text)| Definition::parse(&format!("{name}.toml"), text).unwrap())
        .collect()
}

pub fn site() -> SiteId {
    SiteId::new("test").unwrap()
}

pub fn admin() -> Caller {
    Caller::new("alice", &["System Manager"])
}

pub fn clerk() -> Caller {
    Caller::new("clerk", &["Accounts User"])
}

pub fn actor() -> Actor {
    Actor::User {
        id: RecordId::from_bytes([0x01, 0x97, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1])
            .unwrap(),
        via: None,
    }
}

/// Points the engine at each new snapshot, as a host does.
pub struct EngineListener(pub Arc<Engine<FakeBackend>>);

impl SwapListener for EngineListener {
    fn swapped<'a>(&'a self, snapshot: &'a Arc<Snapshot>) -> ListenerFuture<'a> {
        Box::pin(async move {
            self.0
                .set_meta(snapshot.meta().clone())
                .await
                .map_err(|e| e.to_string())
        })
    }
}

/// `ResourceService` that builds its `EngineResources` from the live snapshot on every call, so
/// a swap is visible to the next request with no rebuild of the router.
pub struct LiveResources {
    pub engine: Arc<Engine<FakeBackend>>,
    pub live: Arc<LiveMeta>,
    pub clock: Arc<ManualClock>,
}

impl LiveResources {
    fn current(&self) -> EngineResources<Engine<FakeBackend>> {
        EngineResources::new(
            self.engine.clone(),
            self.live.current().meta().clone(),
            self.clock.clone(),
            Arc::new(|_| Ok(actor())),
        )
    }
}

impl ResourceService for LiveResources {
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current().get(ctx, doctype, id).await })
    }
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move { self.current().list(ctx, doctype, spec).await })
    }
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move { self.current().query(ctx, doctype, spec).await })
    }
    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        document: &'a spot_http::api::codec::ApiDocument,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current().create(ctx, doctype, document).await })
    }
    fn update<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        patch: &'a spot_http::api::codec::DocumentPatchApi,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            self.current()
                .update(ctx, doctype, id, patch, expected)
                .await
        })
    }
    fn delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<Vec<Message>, ApiError>> {
        Box::pin(async move { self.current().delete(ctx, doctype, id, expected).await })
    }
    fn post<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current().post(ctx, doctype, id, expected).await })
    }
    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current().cancel(ctx, doctype, id, expected).await })
    }
    fn correct<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move { self.current().correct(ctx, doctype, id, expected).await })
    }
    fn rename<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            self.current()
                .rename(ctx, doctype, id, new_name, expected)
                .await
        })
    }
    fn call_method<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<spot_app_api::prelude::Version>,
    ) -> BoxFuture<'a, Result<(MethodOutcome, Vec<Message>), ApiError>> {
        Box::pin(async move {
            self.current()
                .call_method(ctx, doctype, id, method, args, expected)
                .await
        })
    }
    fn call_function<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<Message>), ApiError>> {
        Box::pin(async move {
            self.current()
                .call_function(ctx, app, module, function, args, via_get)
                .await
        })
    }
    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>> {
        Box::pin(async move { self.current().run_report(ctx, report, filters, limit).await })
    }
}

/// `MetaService` reading the live snapshot on every call.
pub struct LiveMetaService(pub Arc<LiveMeta>);

impl MetaService for LiveMetaService {
    fn doctype_meta<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
    ) -> BoxFuture<'a, Result<spot_http::api::meta::DocTypeMeta, ApiError>> {
        Box::pin(async move {
            MetaResources::new(self.0.current().meta().clone())
                .doctype_meta(ctx, doctype)
                .await
        })
    }
}

struct Unused;
impl AuthService for Unused {
    fn login<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>> {
        Box::pin(async { Err(ApiError::from_code(Code::AuthRequired)) })
    }
    fn logout<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async { Ok(()) })
    }
    fn session<'a>(&'a self, _: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>> {
        Box::pin(async { Err(ApiError::from_code(Code::AuthRequired)) })
    }
}
impl LookupService for Unused {
    fn search<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
        _: &'a LookupRequest,
    ) -> BoxFuture<'a, Result<LookupPage, ApiError>> {
        Box::pin(async { Err(ApiError::from_code(Code::AuthRequired)) })
    }
}

/// One node of the site: the live metadata, the engine and the API router over it.
pub struct Rig {
    pub service: Arc<CustomiseService>,
    pub live: Arc<LiveMeta>,
    pub engine: Arc<Engine<FakeBackend>>,
    pub backend: FakeBackend,
    pub versions: Arc<MemoryVersionStore>,
    pub bus: Arc<PollingBus>,
    pub records: Arc<MemoryRecordStore>,
    pub host: Arc<MemoryHost>,
    pub schema: Arc<StorageSchema<FakeBackend>>,
    pub flaky: Arc<Flaky>,
    pub router: Router,
    pub clock: Arc<ManualClock>,
}

/// A schema backend that can be told to fail or to refuse, for the rollback paths.
pub struct Flaky {
    pub inner: Arc<StorageSchema<FakeBackend>>,
    pub fail_apply: std::sync::atomic::AtomicBool,
    pub refuse: std::sync::atomic::AtomicBool,
}

impl SchemaBackend for Flaky {
    fn plan<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, SchemaPlan> {
        Box::pin(async move {
            let mut plan = self.inner.plan(wanted).await?;
            if self.refuse.load(std::sync::atomic::Ordering::SeqCst) {
                plan.refused.push(RefusedStep {
                    entity: "invoice".into(),
                    field: Some("amount".into()),
                    reason: "dropping a field removes data".into(),
                });
            }
            Ok(plan)
        })
    }
    fn apply<'a>(&'a self, wanted: &'a Catalog) -> SchemaFuture<'a, ()> {
        Box::pin(async move {
            if self.fail_apply.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("the database is not reachable".into());
            }
            self.inner.apply(wanted).await
        })
    }
    fn has_rows<'a>(&'a self, doctype: &'a str) -> SchemaFuture<'a, bool> {
        self.inner.has_rows(doctype)
    }
    fn backfill<'a>(&'a self, wanted: &'a Catalog, fills: &'a [Fill]) -> SchemaFuture<'a, u64> {
        self.inner.backfill(wanted, fills)
    }
}

/// What a restart keeps: the database and the shared stores.
#[derive(Clone)]
pub struct Durable {
    pub backend: FakeBackend,
    pub versions: Arc<MemoryVersionStore>,
    pub records: Arc<MemoryRecordStore>,
    pub host: Arc<MemoryHost>,
    pub schema: Arc<StorageSchema<FakeBackend>>,
    pub flaky: Arc<Flaky>,
}

impl Durable {
    /// A fresh database holding the base DocTypes.
    pub async fn new() -> Durable {
        let backend = FakeBackend::new();
        let snapshot = Snapshot::build(
            base_definitions(),
            &spot_meta::Customisations::new(),
            &BuildOptions::new(),
            Version::ZERO,
        )
        .unwrap();
        let catalog = snapshot
            .meta()
            .catalog(Collation::new("und").unwrap())
            .unwrap();
        spot_storage_contract::StorageBackend::prepare(&backend, &catalog)
            .await
            .unwrap();
        let schema = Arc::new(StorageSchema::new(Arc::new(backend.clone()), catalog));
        Durable {
            flaky: Arc::new(Flaky {
                inner: schema.clone(),
                fail_apply: Default::default(),
                refuse: Default::default(),
            }),
            schema,
            backend,
            versions: Arc::new(MemoryVersionStore::new()),
            records: Arc::new(MemoryRecordStore::new()),
            host: Arc::new(MemoryHost::new()),
        }
    }
}

impl Rig {
    pub async fn new() -> Rig {
        Rig::boot(&Durable::new().await).await
    }

    /// A node (a process) over the durable parts: what a restart builds again.
    pub async fn boot(durable: &Durable) -> Rig {
        let clock = Arc::new(ManualClock::new(
            time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
        ));
        let live = Arc::new(LiveMeta::new(site(), base_definitions()).unwrap());
        let engine = Arc::new(
            Engine::new(
                durable.backend.clone(),
                live.current().meta().clone(),
                Arc::new(RegistryBuilder::new().build().unwrap()),
                clock.clone(),
                Arc::new(V7Ids),
            )
            .unwrap(),
        );
        let bus = Arc::new(PollingBus::new(durable.versions.clone()));
        let service = Arc::new(
            CustomiseService::new(Parts {
                site: site(),
                base: base_definitions(),
                options: BuildOptions::new(),
                live: live.clone(),
                bus: bus.clone(),
                versions: durable.versions.clone(),
                definitions: durable.versions.clone(),
                records: durable.records.clone(),
                schema: durable.flaky.clone(),
            })
            .with_host(durable.host.clone())
            .with_listener(Arc::new(EngineListener(engine.clone()))),
        );
        let services = ApiServices::new(
            Arc::new(LiveMetaService(live.clone())),
            Arc::new(LiveResources {
                engine: engine.clone(),
                live: live.clone(),
                clock: clock.clone(),
            }),
            Arc::new(Unused),
            Arc::new(Unused),
            Arc::new(MemoryIdempotencyStore::new(clock.clone())),
        );
        let router = ServerBuilder::new(
            ServerConfig::default(),
            StaticSites::new().add("test.example", site()),
        )
        .site_routes(router(services))
        .build();
        Rig {
            service,
            live,
            engine,
            backend: durable.backend.clone(),
            versions: durable.versions.clone(),
            bus,
            records: durable.records.clone(),
            host: durable.host.clone(),
            schema: durable.schema.clone(),
            flaky: durable.flaky.clone(),
            router,
            clock,
        }
    }

    /// A second node over the same database: a follower that learns of changes by polling.
    pub fn follower(&self) -> Follower {
        Follower::with_initial(
            site(),
            base_definitions(),
            Version::ZERO,
            self.bus.clone() as Arc<dyn InvalidationBus>,
            self.versions.clone() as Arc<dyn VersionStore>,
            self.versions.clone() as Arc<dyn MetaLoader>,
            PollSchedule::new(std::time::Duration::from_secs(30)),
        )
        .unwrap()
    }

    pub async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Option<Json>,
    ) -> Call {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "test.example")
            .header("authorization", "Bearer token");
        for (key, value) in headers {
            request = request.header(*key, value);
        }
        let body = match body {
            Some(body) => {
                request = request.header("content-type", "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        let response = self
            .router
            .clone()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let bytes = to_bytes(response.into_body(), 4_000_000).await.unwrap();
        let body = if bytes.is_empty() {
            Json::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        Call {
            status,
            headers,
            body,
        }
    }
}

pub struct Call {
    pub status: u16,
    pub headers: axum::http::HeaderMap,
    pub body: Json,
}

impl Call {
    pub fn status(&self, n: u16) -> &Self {
        assert_eq!(self.status, n, "{}", self.body);
        self
    }
    pub fn id(&self) -> String {
        self.body["data"]["id"].as_str().unwrap().to_owned()
    }
    pub fn etag(&self) -> String {
        self.headers["etag"].to_str().unwrap().to_owned()
    }
}

pub fn json_none() -> Option<Json> {
    None
}

pub fn body(value: Json) -> Option<Json> {
    Some(value)
}

pub fn _use_json() -> Json {
    json!(null)
}
