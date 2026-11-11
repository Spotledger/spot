//! Test support: a small world, a host and helpers for the conformance, catalog, isolation and
//! limit tests.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::Value as Json;
use spot_agent_tiers::Tier;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::Document;
use spot_app_api::meta::{DocTypeDef, FieldKind};
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Value};
use spot_app_api::stub::StubHost;
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId};
use spot_functions::{
    Caller, Context, FunctionDecl, FunctionHandler, FunctionKey, FunctionRegistry,
    FunctionRegistryBuilder, HandlerFuture, HttpVerb,
};
use spot_iam_access::{AllowRule, KeyAction, KeyScope};
use spot_jobs::InMemoryJobStore;
use spot_meta::{
    Customisations, Definition, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec, Registry,
};
use spot_permissions::{
    AssignmentContext, FieldGrant, FieldRight, FieldTarget, GroupName, MaskFormat, Model,
    PermissionEngine, Principal, ProtectionGroup, Right, Rights, Role, RoleGrant, Snapshot,
};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, Fields, Isolation, Name as SName, Query,
    ReadSource, Rows, StorageBackend, Transaction, TxOptions, Value as SValue,
};

use spot_mcp::audit::MemoryAudit;
use spot_mcp::principal::{Authenticator, Credential, McpPrincipal};
use spot_mcp::query::QueryPort;
use spot_mcp::server::{MetaState, Server, ServerParts};
use spot_mcp::{Limits, McpError};

use spot_storage_fake::FakeBackend;

/// The DocType every test uses.
pub const DOCTYPE: &str = "note";

/// The three documents' ids, in seed order.
pub fn doc_ids() -> [RecordId; 3] {
    [id(1), id(2), id(3)]
}

fn id(n: u8) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = n;
    RecordId::from_bytes(bytes).unwrap()
}

fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

/// The `note` definition, with a protected `salary` field.
pub fn definition() -> Definition {
    let mut def = Definition::new(DOCTYPE);
    def.label = Some("Note".to_owned());
    def.title_field = Some("title".to_owned());
    let mut title = field("title", FieldType::Text);
    title.required = true;
    let body = field("body", FieldType::Text);
    let mut salary = field("salary", FieldType::Decimal);
    salary.scale = Some(2);
    salary.precision = Some(18);
    salary.protection_group = Some("pay".to_owned());
    salary.masking = Some(MaskSpec::placeholder());
    let mut secret = field("secret", FieldType::Text);
    secret.protection_group = Some("secret".to_owned());
    def.fields = vec![title, body, salary, secret];
    def.protection_groups = vec![
        ProtectionGroupSpec::new("pay", "Pay"),
        ProtectionGroupSpec::new("secret", "Secret"),
    ];
    def
}

/// The registry over the sample metadata.
pub fn registry() -> Registry {
    Registry::new(vec![definition()]).unwrap()
}

/// The permission model: a reader sees `salary` masked, a manager sees it, a writer may write.
pub fn model() -> Model {
    let rights = |rs: &[Right]| Rights::new(rs.iter().copied());
    let grant = |role: &str, rs: &[Right]| RoleGrant {
        doctype: DOCTYPE.to_owned(),
        role: Role(role.to_owned()),
        rights: rights(rs),
        if_owner: false,
    };
    let field_grant = |role: &str, rights: &[FieldRight]| FieldGrant {
        doctype: DOCTYPE.to_owned(),
        role: Role(role.to_owned()),
        target: FieldTarget::Group(GroupName("pay".to_owned())),
        access: rights.iter().copied().collect::<BTreeSet<_>>(),
        deny: false,
        condition: None,
    };
    Model {
        grants: vec![
            grant("reader", &[Right::Read]),
            grant("manager", &[Right::Read]),
            grant("writer", &[Right::Read, Right::Write, Right::Create]),
        ],
        field_grants: vec![
            field_grant("reader", &[FieldRight::ReadMasked]),
            field_grant("manager", &[FieldRight::Read, FieldRight::Reveal]),
            field_grant("writer", &[FieldRight::ReadMasked]),
            FieldGrant {
                doctype: DOCTYPE.to_owned(),
                role: Role("manager".to_owned()),
                target: FieldTarget::Group(GroupName("secret".to_owned())),
                access: [FieldRight::Read].into_iter().collect(),
                deny: false,
                condition: None,
            },
        ],
        groups: vec![
            ProtectionGroup {
                name: GroupName("pay".to_owned()),
                trace: false,
            },
            ProtectionGroup {
                name: GroupName("secret".to_owned()),
                trace: false,
            },
        ],
        shares: Vec::new(),
    }
}

/// The permission engine over the sample metadata and model.
pub fn engine() -> Arc<PermissionEngine> {
    let meta = registry().effective();
    Arc::new(engine_for_meta(&meta, &model()))
}

/// Builds an engine whose snapshot carries the metadata's protection facts (groups, tags,
/// masks) — the mapping the platform's field guard does.
pub fn engine_for_meta(meta: &spot_meta::EffectiveMeta, model: &Model) -> PermissionEngine {
    let mut snapshot = Snapshot::build(meta, model).unwrap();
    for view in meta.doctypes() {
        let name = view.name.as_str();
        let prefix = format!("{name}.");
        let facts: Vec<spot_permissions::FieldFacts> = meta
            .field_protection()
            .iter()
            .filter_map(|p| {
                let field = p.path.strip_prefix(&prefix)?;
                Some(spot_permissions::FieldFacts {
                    path: field.to_owned(),
                    groups: p.groups.iter().cloned().map(GroupName).collect(),
                    tags: p.tags.iter().cloned().map(spot_permissions::Tag).collect(),
                    mask: p.mask.as_deref().map(parse_mask),
                })
            })
            .collect();
        snapshot = snapshot.with_field_facts(name, facts).unwrap();
    }
    PermissionEngine::new(Arc::new(snapshot))
}

fn parse_mask(text: &str) -> MaskFormat {
    text.strip_prefix("last_chars(")
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|n| n.parse::<u8>().ok())
        .map_or(MaskFormat::Placeholder, MaskFormat::LastChars)
}

/// The metadata and engine as a [`MetaState`].
pub fn meta_state() -> Arc<MetaState> {
    Arc::new(MetaState {
        registry: Arc::new(registry()),
        engine: engine(),
    })
}

fn actor() -> Actor {
    Actor::User {
        id: RecordId::new_v7(),
        via: None,
    }
}

fn assignment(role: &str) -> Vec<AssignmentContext> {
    vec![AssignmentContext::unscoped("a1", &[role])]
}

/// The three test principals.
pub fn principals() -> BTreeMap<String, McpPrincipal> {
    let mut out = BTreeMap::new();
    out.insert(
        "alice".to_owned(),
        McpPrincipal::new("alice", actor(), assignment("reader")).at(1_700_000_000),
    );
    out.insert(
        "bob".to_owned(),
        McpPrincipal::new("bob", actor(), assignment("manager")).at(1_700_000_000),
    );
    out.insert(
        "carol".to_owned(),
        McpPrincipal::new("carol", actor(), assignment("writer")).at(1_700_000_000),
    );
    let scope = KeyScope {
        allow: vec![AllowRule::new(DOCTYPE, [KeyAction::Read])],
        scope: Vec::new(),
        expires_at: None,
    };
    out.insert(
        "key-alice".to_owned(),
        McpPrincipal::new("alice", actor(), assignment("reader"))
            .at(1_700_000_000)
            .with_key("key-alice", scope),
    );
    out.insert(
        "agent0".to_owned(),
        McpPrincipal::new_agent("carol", actor(), assignment("writer"), "agent-1", Tier::T0)
            .at(1_700_000_000),
    );
    out
}

/// An authenticator over a fixed set of principals.
pub struct TestAuth {
    principals: BTreeMap<String, McpPrincipal>,
}

impl TestAuth {
    /// The standard principals.
    pub fn new() -> Self {
        Self {
            principals: principals(),
        }
    }
}

impl Default for TestAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl Authenticator for TestAuth {
    fn authenticate(
        &self,
        credential: &Credential,
        _now_unix: i64,
    ) -> Result<McpPrincipal, McpError> {
        let key = match credential {
            Credential::Bearer(token) => token.clone(),
            Credential::ApiKey { id, .. } => id.clone(),
        };
        self.principals
            .get(&key)
            .cloned()
            .ok_or_else(|| McpError::unauthorized("unknown credential"))
    }
}

/// A query port over the in-memory fake backend.
pub struct BackendPort {
    backend: FakeBackend,
}

impl BackendPort {
    /// Wraps a backend.
    pub fn new(backend: FakeBackend) -> Self {
        Self { backend }
    }
}

impl QueryPort for BackendPort {
    fn read<'a>(
        &'a self,
        _inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> spot_mcp::BoxFuture<'a, Result<Rows, McpError>> {
        Box::pin(async move {
            self.backend
                .read(catalog, ReadSource::Primary, query)
                .await
                .map_err(|e| McpError::internal(e.to_string()))
        })
    }
}

/// An echo handler for whitelisted functions.
pub struct EchoHandler;

impl FunctionHandler for EchoHandler {
    fn call<'a>(&'a self, _ctx: &'a Context, args: Json) -> HandlerFuture<'a> {
        Box::pin(async move { Ok(args) })
    }
}

/// The sample function registry: one GET function and one job-only function.
pub fn functions() -> FunctionRegistry {
    let mut builder = FunctionRegistryBuilder::new();
    builder.register_decl(
        "demo",
        FunctionDecl {
            key: FunctionKey::from_parts("demo", "greet", "hello"),
            verbs: Some(vec![HttpVerb::Get]),
            guest: false,
            limit: None,
            arguments: Some(serde_json::json!({
                "type": "object",
                "properties": { "who": { "type": "string" } },
                "required": ["who"],
            })),
        },
        Arc::new(EchoHandler),
    );
    builder.register_decl(
        "demo",
        FunctionDecl {
            key: FunctionKey::from_parts("demo", "jobs", "run"),
            verbs: None,
            guest: false,
            limit: None,
            arguments: Some(serde_json::json!({ "type": "object" })),
        },
        Arc::new(EchoHandler),
    );
    builder.build().unwrap()
}

/// A seeded world: the document service, the query port and the ids.
pub struct World {
    /// The document service with the three notes.
    pub service: Arc<StubHost>,
    /// The query port with the same three notes.
    pub port: Arc<BackendPort>,
    /// The ids of the three notes.
    pub ids: [RecordId; 3],
}

/// Builds the world: three notes in both the document service and the query backend.
pub async fn world() -> World {
    let registry = registry();
    let effective = registry.effective();
    let catalog = Arc::new(effective.catalog(Collation::new("und").unwrap()).unwrap());
    let backend = FakeBackend::new();
    backend.prepare(&catalog).await.unwrap();
    let host = StubHost::new();
    let def: &Arc<DocTypeDef> = &effective.doctype(DOCTYPE).unwrap();
    host.define((**def).clone());

    let ids = doc_ids();
    let rows = [
        ("alice", "one", "100.00", "secret-one"),
        ("bob", "two", "200.00", "secret-two"),
        ("carol", "three", "300.00", "secret-three"),
    ];

    let mut tx = backend
        .begin(
            catalog.clone(),
            TxOptions::new(SName::new("seed").unwrap(), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    for (i, (_owner, title, salary, secret)) in rows.iter().enumerate() {
        let mut fields = Fields::new();
        fields.insert(
            SName::new("title").unwrap(),
            SValue::Text((*title).to_owned()),
        );
        fields.insert(
            SName::new("body").unwrap(),
            SValue::Text(format!("body of {title}")),
        );
        fields.insert(
            SName::new("salary").unwrap(),
            SValue::Decimal((*salary).parse().unwrap()),
        );
        fields.insert(
            SName::new("secret").unwrap(),
            SValue::Text((*secret).to_owned()),
        );
        tx.insert(&DocumentData {
            entity: SName::new(DOCTYPE).unwrap(),
            id: ids[i],
            fields,
            children: BTreeMap::new(),
        })
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    let inv = Invocation {
        site: site(),
        actor: actor(),
        command_id: Some(CommandId::new("seed").unwrap()),
        language: None,
        now: time::OffsetDateTime::UNIX_EPOCH,
    };
    for (i, (_owner, title, salary, secret)) in rows.iter().enumerate() {
        let mut doc = Document::new(Name::new(DOCTYPE).unwrap(), ids[i]);
        doc.set(
            Name::new("title").unwrap(),
            Value::Text((*title).to_owned()),
        );
        doc.set(
            Name::new("body").unwrap(),
            Value::Text(format!("body of {title}")),
        );
        doc.set(
            Name::new("salary").unwrap(),
            Value::Decimal((*salary).parse().unwrap()),
        );
        doc.set(
            Name::new("secret").unwrap(),
            Value::Text((*secret).to_owned()),
        );
        host.create(&inv, doc).await.unwrap();
    }

    World {
        service: Arc::new(host),
        port: Arc::new(BackendPort::new(backend)),
        ids,
    }
}

/// The test site.
pub fn site() -> SiteId {
    SiteId::new("acme").unwrap()
}

/// Builds the server over a world, with a swappable metadata source.
pub async fn server() -> (Arc<Server>, Arc<spot_mcp::server::SwappableMeta>) {
    server_with(Limits::default(), None).await
}

/// Builds the server with custom limits and, optionally, a custom query port.
pub async fn server_with(
    limits: Limits,
    query: Option<Arc<dyn QueryPort>>,
) -> (Arc<Server>, Arc<spot_mcp::server::SwappableMeta>) {
    server_full(limits, query, Arc::new(MemoryAudit::new()), None).await
}

/// Builds the server with a caller-owned audit sink.
pub async fn server_with_audit(
    audit: Arc<MemoryAudit>,
) -> (Arc<Server>, Arc<spot_mcp::server::SwappableMeta>) {
    server_full(Limits::default(), None, audit, None).await
}

/// Builds the server with any audit sink and, optionally, a failure policy and alert sink.
#[allow(dead_code)]
pub async fn server_with_sink(
    audit: Arc<dyn spot_mcp::AuditSink>,
    policy: spot_mcp::AuditFailurePolicy,
    alerts: Arc<dyn spot_mcp::AuditAlerts>,
) -> (Arc<Server>, Arc<spot_mcp::server::SwappableMeta>) {
    server_full(Limits::default(), None, audit, Some((policy, alerts))).await
}

async fn server_full(
    limits: Limits,
    query: Option<Arc<dyn QueryPort>>,
    audit: Arc<dyn spot_mcp::AuditSink>,
    audit_config: Option<(spot_mcp::AuditFailurePolicy, Arc<dyn spot_mcp::AuditAlerts>)>,
) -> (Arc<Server>, Arc<spot_mcp::server::SwappableMeta>) {
    let world = world().await;
    let meta = Arc::new(spot_mcp::server::SwappableMeta::new(meta_state()));
    let coordinator = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("test").unwrap(),
        Arc::new(spot_core::SystemClock::new()),
    )
    .await
    .unwrap();
    let port: Arc<dyn QueryPort> = match query {
        Some(query) => query,
        None => world.port.clone(),
    };
    let server = Server::new(ServerParts {
        meta: meta.clone(),
        site: site(),
        service: world.service.clone(),
        query: port,
        functions: Arc::new(functions()),
        jobs: Arc::new(InMemoryJobStore::new()),
        coordinator,
        audit,
        auth: Arc::new(TestAuth::new()),
        limits,
        instructions: "Spotledger MCP".to_owned(),
    });
    let server = match audit_config {
        Some((policy, alerts)) => server.with_audit_policy(policy).with_audit_alerts(alerts),
        None => server,
    };
    (Arc::new(server), meta)
}

/// The JSON-RPC `_meta` every modern test request carries.
pub fn meta_params() -> Json {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": spot_mcp::PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "0.1.0" },
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

/// A request body with `_meta` merged into its params.
pub fn request(id: i64, method: &str, params: Json) -> String {
    let mut params = params;
    if let Some(obj) = params.as_object_mut() {
        obj.insert("_meta".to_owned(), meta_params());
    }
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        .to_string()
}

/// Extracts the `result` of a single-response transcript.
pub fn result_of(messages: &[Json]) -> Json {
    messages
        .iter()
        .find_map(|m| m.get("result").cloned())
        .unwrap_or(Json::Null)
}

/// Extracts the `error` of a single-response transcript.
pub fn error_of(messages: &[Json]) -> Json {
    messages
        .iter()
        .find_map(|m| m.get("error").cloned())
        .unwrap_or(Json::Null)
}

/// The real salary values, for the isolation checks.
pub fn salary_values() -> [&'static str; 3] {
    ["100.00", "200.00", "300.00"]
}

/// The real secret values, for the isolation checks.
pub fn secret_values() -> [&'static str; 3] {
    ["secret-one", "secret-two", "secret-three"]
}

/// Whether a principal may read `field` unmasked.
pub fn may_read_field(engine: &PermissionEngine, principal: &Principal, field: &str) -> bool {
    let Some(facts) = engine.snapshot().facts(DOCTYPE) else {
        return false;
    };
    let Some(field_facts) = facts.field(field).cloned() else {
        return false;
    };
    let doc = spot_permissions::DocFacts {
        doctype: DOCTYPE.to_owned(),
        created_by: Some(principal.user.clone()),
        ..spot_permissions::DocFacts::default()
    };
    matches!(
        engine
            .field_access(principal, DOCTYPE, &field_facts, &doc)
            .level,
        spot_permissions::FieldLevel::Read | spot_permissions::FieldLevel::ReadReveal
    )
}

/// The engine a test's own registry/model build, for isolation checks.
pub fn engine_for(registry: &Registry, model: &Model) -> PermissionEngine {
    engine_for_meta(&registry.effective(), model)
}

/// Unused marker so the imports above are exercised by the crate's own tests.
pub fn _unused() {
    let _ = (
        Caller::User("x".to_owned()),
        Tier::T0,
        MaskFormat::Placeholder,
        Collation::new("und").unwrap(),
        Customisations::new(),
        FieldKind::Bool,
        InMemoryJobStore::new(),
        SName::new("x").unwrap(),
    );
}
