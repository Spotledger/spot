//! The engine-backed rig of `tests/secret_field.rs` (WP-PLT-05.T10): a scratch database, the
//! document engine on PostgreSQL with the sealed-field vault built the way the secrets service
//! hosts it (`SecretBox` over a keyring), the real `spot_http` router over `EngineResources`,
//! and the real `PgIdempotencyStore` on its own pool.
//!
//! Included with `#[path = "common/engine.rs"] mod engine;` next to `mod common;`, because
//! `common/mod.rs` belongs to WP-PLT-05.T7.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, header};
use deadpool_postgres::Pool;
use serde_json::Value as Json;
use spot_api_services::{EngineResources, MetaResources};
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{Query, RecordId, SiteId, Value};
use spot_app_api::{AppResult, BoxFuture as AppFuture, DocTypeDef, Document};
use spot_core::ManualClock;
use spot_document::engine::{
    AllowAllAuthorizer, Authorizer, Engine, QueryPlan, RetryPolicy, V7Ids, WriteChange,
};
use spot_document::secrets::SecretCaller;
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, AuthService, BoxFuture, LoginBody, LookupService, RequestCtx, RevealService, Session,
    SessionInfo,
};
use spot_http::api::{ApiServices, IdempotencyStore, router};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_idempotency_pg::PgIdempotencyStore;
use spot_meta::{Definition, FieldLookup, FieldSpec, FieldType, Registry};
use spot_permissions::{Action, Decision, DocFacts, Principal};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_seams::RegistryBuilder;
use spot_secrets::{MemoryKeyring, SecretBox, SecretString};
use spot_storage_contract::{
    Collation, DocumentStore, Isolation, Name, RowLock, StorageBackend, Transaction, TxOptions,
};
use tower::ServiceExt as _;

use crate::common;

/// The host the router resolves to [`SITE`].
pub const HOST: &str = "erp.acme.test";
/// The site (tenant) of every request.
pub const SITE: &str = "acme";
/// The secret every test writes. Failure messages never print it: they assert on booleans.
pub const PASSWORD: &str = "correct horse battery staple 7731";
/// A second secret value, for the conflict case.
pub const OTHER_PASSWORD: &str = "tr0ub4dor and three more words 4096";
/// The value of the lookup secret field.
pub const API_KEY: &str = "ak_live_c0ffee5eed1234567890abcdef";
/// The caller that creates and reads accounts and holds no reveal right.
pub const TOKEN_WRITER: &str = "tok_writer_5c1d9e";
/// The caller that holds the reveal right.
pub const TOKEN_REVEALER: &str = "tok_revealer_77a3b0";
/// The `reader` caller class (WP-PLT-05.T13): a user for whom the [`MaskNote`] authorizer drops
/// `internal_note`. The writer token is the `admin` class, who sees every field.
pub const TOKEN_READER: &str = "tok_reader_2e8f44";
/// The text every test puts in `internal_note`. Failure messages never print it.
pub const NOTE: &str = "internal note: renegotiate the Zeta contract before March";

fn name(text: &str) -> Name {
    Name::new(text).unwrap()
}

fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_793_000_000).unwrap()
}

/// The user id behind both tokens.
fn user() -> RecordId {
    spot_http::api::codec::parse_record_id("01970000-0000-7000-8000-000000000001").unwrap()
}

/// The reader's user id, distinct from [`user`].
fn reader_user() -> RecordId {
    spot_http::api::codec::parse_record_id("01970000-0000-7000-8000-000000000002").unwrap()
}

/// The `Authorizer` double of WP-PLT-05.T13: it delegates every decision to
/// [`AllowAllAuthorizer`] except two. `principal_for` resolves the reader's user id to a
/// non-system principal named `reader` (every other actor is the system principal, the `admin`
/// class), and `readable` drops the text field `internal_note` for a non-system principal, the
/// way a permission mask hides a field.
pub struct MaskNote;

impl Authorizer for MaskNote {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> AppFuture<'a, Principal> {
        Box::pin(async move {
            match &inv.actor {
                Actor::User { id, .. } if *id == reader_user() => {
                    Principal::user("reader", Vec::new())
                }
                _ => Principal::system(),
            }
        })
    }

    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> AppFuture<'a, Decision> {
        AllowAllAuthorizer.decide(principal, action, facts)
    }

    fn authorize<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
    ) -> AppFuture<'a, AppResult<()>> {
        AllowAllAuthorizer.authorize(principal, action, doc)
    }

    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [WriteChange],
    ) -> AppFuture<'a, AppResult<()>> {
        AllowAllAuthorizer.authorize_write(principal, action, doc, changes)
    }

    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        _def: &'a DocTypeDef,
    ) -> AppFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            if principal.is_system {
                return Ok(doc);
            }
            let Some(version) = doc.version() else {
                return Ok(doc);
            };
            let mut fields = doc.fields().clone();
            fields.remove(&name("internal_note"));
            Ok(Document::loaded(
                doc.doctype().clone(),
                doc.id(),
                version,
                doc.state(),
                fields,
                doc.tables().clone(),
            ))
        })
    }

    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a Query,
    ) -> AppFuture<'a, AppResult<Option<QueryPlan>>> {
        AllowAllAuthorizer.readable_query(principal, query)
    }
}

/// One DocType, `account`: a text `label`, a text `internal_note` (hidden from the `reader`
/// class by [`MaskNote`]), a `password` (a sealed field, not a lookup) and an
/// `api_key` (a sealed field that is a lookup, so the engine writes a blind-index digest). It
/// is a posted DocType with a title field, so `post` and `rename` have routes to answer.
fn definitions() -> Vec<Definition> {
    let mut account = Definition::new("account");
    account.is_posted = true;
    account.title_field = Some("label".to_owned());
    let mut api_key = FieldSpec::new("api_key", FieldType::Secret);
    api_key.lookup = Some(FieldLookup::Index(true));
    account.fields = vec![
        FieldSpec::new("label", FieldType::Text),
        FieldSpec::new("internal_note", FieldType::Text),
        FieldSpec::new("password", FieldType::Secret),
        api_key,
    ];
    vec![account]
}

pub fn meta() -> Arc<spot_meta::EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
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
        Box::pin(async { Err(ApiError::from_code(Code::AuthRequired)) })
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

/// The reveal route over the engine's open capability. Only the revealer's token holds the
/// right; every other caller is refused the way the router answers any refusal.
struct EngineReveal {
    engine: Arc<Engine<PgBackend>>,
}

impl RevealService for EngineReveal {
    fn reveal<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        field: &'a str,
        _reason: &'a str,
    ) -> BoxFuture<'a, Result<Json, ApiError>> {
        Box::pin(async move {
            let held = ctx
                .credential
                .as_ref()
                .is_some_and(|c| c.token == TOKEN_REVEALER);
            if !held {
                return Err(ApiError::new(Code::PermissionDenied, "no reveal right"));
            }
            let inv = invocation();
            let access = self.engine.secret_access(SecretCaller::Test, &inv);
            let opened = self
                .engine
                .open_secret(&access, doctype, id, field)
                .await
                .map_err(|_| ApiError::new(Code::PermissionDenied, "not revealable"))?;
            Ok(opened.map_or(Json::Null, |s| Json::String(s.expose().to_owned())))
        })
    }
}

fn invocation() -> Invocation {
    Invocation {
        site: SiteId::new(SITE).unwrap(),
        actor: Actor::User {
            id: user(),
            via: None,
        },
        command_id: None,
        language: None,
        now: now(),
    }
}

/// An engine on a fresh backend over `db`, sealing with a vault on `keyring` (the construction
/// the host uses: `SecretBox::new(keyring)` handed to `Engine::with_secrets`).
async fn build_engine(db: &TestDb, keyring: &Arc<MemoryKeyring>) -> Arc<Engine<PgBackend>> {
    let backend = if std::env::var_os("SPOT_PG_UNCHECKED").is_some() {
        PgBackend::connect_unchecked(db.config()).unwrap()
    } else {
        PgBackend::connect(db.config()).await.unwrap()
    };
    let vault = Arc::new(SecretBox::new(keyring.clone()));
    let engine = Engine::new(
        backend,
        meta(),
        Arc::new(RegistryBuilder::new().build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(RetryPolicy {
        max_retries: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    })
    .with_authorizer(Arc::new(MaskNote))
    .with_secrets(vault);
    engine.prepare().await.unwrap();
    Arc::new(engine)
}

/// One serving node: a router over the engine and the PostgreSQL store on its own pool.
pub struct Node {
    pub router: Router,
    pub store: Arc<PgIdempotencyStore>,
    pub pool: Pool,
    pub engine: Arc<Engine<PgBackend>>,
}

/// The scratch database, the keyring (the engine's data is sealed under it) and a node.
pub struct Rig {
    pub db: TestDb,
    pub clock: Arc<ManualClock>,
    pub keyring: Arc<MemoryKeyring>,
    pub node: Node,
}

impl Rig {
    /// A rig on a new scratch database, or `None` when no server is configured (the skip
    /// `TestDb::create` reports; it panics instead under `SPOT_REQUIRE_PG=1`).
    pub async fn new() -> Option<Rig> {
        let db = TestDb::create().await?;
        let clock = common::clock();
        let keyring = Arc::new(MemoryKeyring::new("test-kek").unwrap());
        let node = node(&db, &keyring, &clock, "idem_secret_a").await;
        Some(Rig {
            db,
            clock,
            keyring,
            node,
        })
    }

    /// A restart: the router, the store, its pool and the engine are dropped and built again
    /// over the same database and the same keyring. Nothing is carried in memory.
    pub async fn restart(&mut self, app: &str) {
        self.node.pool.close();
        let fresh = node(&self.db, &self.keyring, &self.clock, app).await;
        self.node = fresh;
    }

    /// One request through the whole router.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, String)],
        body: Option<Json>,
    ) -> Reply {
        call(&self.node.router, method, path, headers, body).await
    }

    /// `POST /api/v1/resource/account` by the writer, keyed by `key`.
    pub async fn create(&self, key: &str, body: Json) -> Reply {
        self.call(
            "POST",
            "/api/v1/resource/account",
            &[bearer(TOKEN_WRITER), idem_key(key)],
            Some(body),
        )
        .await
    }

    /// An account with both secrets set, made behind the router the way the secrets service
    /// does it (`create_with_secrets`): an ordinary POST cannot carry a secret (the router
    /// refuses it, `document.validation_failed`). Returns its id and ETag.
    pub async fn seed(&self, label: &str) -> (RecordId, String) {
        let inv = invocation();
        let access = self.node.engine.secret_access(SecretCaller::Test, &inv);
        let mut doc = spot_app_api::Document::new(name("account"), RecordId::new_v7());
        doc.set(name("label"), label);
        let saved = self
            .node
            .engine
            .create_with_secrets(
                &access,
                doc,
                vec![
                    ("password".to_owned(), SecretString::new(PASSWORD)),
                    ("api_key".to_owned(), SecretString::new(API_KEY)),
                ],
            )
            .await
            .unwrap();
        (saved.id(), format!("\"{}\"", saved.version().unwrap().0))
    }

    /// How many rows `spot_idempotency_key` holds.
    pub async fn rows(&self) -> i64 {
        self.db
            .raw_client()
            .await
            .query_one("SELECT count(*) FROM spot_idempotency_key", &[])
            .await
            .unwrap()
            .get(0)
    }

    /// The one row of `spot_idempotency_key` of `key`: `(state, status, body, headers)`.
    pub async fn row_of(&self, key: &str) -> Stored {
        let rows = self
            .db
            .raw_client()
            .await
            .query(
                "SELECT state, status, body, headers::text FROM spot_idempotency_key \
                 WHERE key = $1",
                &[&key],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 1, "exactly one row for the key");
        let row = &rows[0];
        Stored {
            state: row.get(0),
            status: row.get::<_, Option<i32>>(1),
            body: row.get::<_, Option<Vec<u8>>>(2).unwrap_or_default(),
            headers: row.get::<_, Option<String>>(3).unwrap_or_default(),
        }
    }

    /// How many `account` documents exist, counted through the storage contract.
    pub async fn accounts(&self) -> usize {
        // The id list is read through the engine's query path so no SQL of ours names the
        // engine's table.
        let resp = self
            .call(
                "GET",
                "/api/v1/resource/account?limit=100",
                &[bearer(TOKEN_WRITER)],
                None,
            )
            .await;
        assert_eq!(resp.status, 200, "listing accounts");
        resp.json["data"].as_array().map_or(0, Vec::len)
    }

    /// The stored row of an account behind the engine's back, secret columns included.
    pub async fn raw(&self, id: RecordId) -> spot_storage_contract::DocumentData {
        let catalog = Arc::new(meta().catalog(Collation::new("und").unwrap()).unwrap());
        let mut tx = self
            .node
            .engine
            .backend()
            .begin(
                catalog,
                TxOptions::new(name("raw_read"), Isolation::ReadCommitted),
            )
            .await
            .unwrap();
        let row = tx
            .load(&name("account"), id, RowLock::None)
            .await
            .unwrap()
            .unwrap();
        tx.rollback().await.unwrap();
        row.data
    }

    /// A bytes column of the stored row (`password__sealed`, `api_key__bidx`).
    pub async fn bytes_column(&self, id: RecordId, column: &str) -> Option<Vec<u8>> {
        match self.raw(id).await.fields.get(&name(column)) {
            Some(Value::Bytes(b)) => Some(b.clone()),
            _ => None,
        }
    }

    /// Every `(table, column)` of the scratch database that holds `needle` in any form of
    /// [`forms_of`]: the text, its hex, its base64 standard and URL-safe forms (padded and not).
    /// Each form is searched as text and as the hex of its UTF-8 bytes, so a `bytea` column, a
    /// `jsonb` column and a text column are all covered. Reports names, never the value.
    pub async fn wire_hits(&self, needle: &str) -> Vec<(String, String)> {
        let mut found: Vec<(String, String)> = Vec::new();
        for form in forms_of(needle) {
            let form = String::from_utf8(form).unwrap();
            for hit in self.hits_by_column(&form).await {
                if !found.contains(&hit) {
                    found.push(hit);
                }
            }
        }
        found.sort();
        found
    }

    /// The `(table, column)` pairs whose cells carry `needle` as text, or as the hex of its
    /// UTF-8 bytes (a `bytea` cell renders as `\x` hex). One fixed statement, one bound
    /// parameter; the table and column lists come from the catalogue inside the server.
    pub async fn hits_by_column(&self, needle: &str) -> Vec<(String, String)> {
        let rows = self
            .db
            .raw_client()
            .await
            .query(
                "SELECT c.relname::text, a.attname::text, \
                   (xpath('/row/n/text()', query_to_xml(format( \
                     'SELECT count(*) AS n FROM %I.%I AS t \
                      WHERE strpos(t.%I::text, %L) > 0 \
                         OR strpos(t.%I::text, encode(convert_to(%L, ''UTF8''), ''hex'')) > 0', \
                     ns.nspname, c.relname, a.attname, $1::text, a.attname, $1::text), \
                     false, true, '')))[1]::text::bigint \
                 FROM pg_class c JOIN pg_namespace ns ON ns.oid = c.relnamespace \
                 JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
                 WHERE c.relkind = 'r' AND ns.nspname = current_schema()",
                &[&needle],
            )
            .await
            .unwrap();
        rows.iter()
            .filter(|r| r.get::<_, i64>(2) > 0)
            .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
            .collect()
    }

    /// How many rows, over every table of the scratch database, carry `needle` in any column
    /// (text, json and bytea, the latter in its hex output form and as raw hex of the text).
    /// One fixed statement with one bound parameter; the table list comes from the catalogue
    /// inside the server. Returns `(table, hits)` for every table with a hit.
    pub async fn hits_anywhere(&self, needle: &str) -> Vec<(String, i64)> {
        let rows = self
            .db
            .raw_client()
            .await
            .query(
                "SELECT c.relname::text, \
                   (xpath('/row/n/text()', query_to_xml(format( \
                     'SELECT count(*) AS n FROM %I.%I AS t \
                      WHERE strpos(t::text, %L) > 0 \
                         OR strpos(t::text, encode(convert_to(%L, ''UTF8''), ''hex'')) > 0', \
                     ns.nspname, c.relname, $1::text, $1::text), false, true, '')))[1]::text::bigint \
                 FROM pg_class c JOIN pg_namespace ns ON ns.oid = c.relnamespace \
                 WHERE c.relkind = 'r' AND ns.nspname = current_schema()",
                &[&needle],
            )
            .await
            .unwrap();
        rows.iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)))
            .filter(|(_, hits)| *hits > 0)
            .collect()
    }
}

/// What the store holds for one key.
pub struct Stored {
    pub state: String,
    pub status: Option<i32>,
    pub body: Vec<u8>,
    pub headers: String,
}

/// A node over `db`: a new engine, a new store on a new pool, a new router.
pub async fn node(
    db: &TestDb,
    keyring: &Arc<MemoryKeyring>,
    clock: &Arc<ManualClock>,
    app: &str,
) -> Node {
    let engine = build_engine(db, keyring).await;
    let pool = common::pool(db, 16, app);
    let store = Arc::new(common::store_on(pool.clone(), clock, common::config()).await);
    let router = router_over(&engine, store.clone(), clock);
    Node {
        router,
        store,
        pool,
        engine,
    }
}

fn router_over(
    engine: &Arc<Engine<PgBackend>>,
    store: Arc<dyn IdempotencyStore>,
    clock: &Arc<ManualClock>,
) -> Router {
    let meta = meta();
    let resources = EngineResources::new(
        engine.clone(),
        meta.clone(),
        clock.clone(),
        Arc::new(|ctx: &RequestCtx| {
            let known = ctx.credential.as_ref().is_some_and(|c| {
                c.token == TOKEN_WRITER || c.token == TOKEN_REVEALER || c.token == TOKEN_READER
            });
            if ctx.site.as_str() != SITE || !known {
                return Err(ApiError::from_code(Code::AuthRequired));
            }
            let is_reader = ctx
                .credential
                .as_ref()
                .is_some_and(|c| c.token == TOKEN_READER);
            Ok(Actor::User {
                id: if is_reader { reader_user() } else { user() },
                via: None,
            })
        }),
    );
    let services = ApiServices::new(
        Arc::new(MetaResources::new(meta)),
        Arc::new(resources),
        Arc::new(Unused),
        Arc::new(Unused),
        store,
    )
    .with_reveal(Arc::new(EngineReveal {
        engine: engine.clone(),
    }));
    let site = SiteId::new(SITE).unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build()
}

pub fn bearer(token: &str) -> (&'static str, String) {
    ("authorization", format!("Bearer {token}"))
}

pub fn idem_key(key: &str) -> (&'static str, String) {
    ("idempotency-key", key.to_owned())
}

pub fn if_match(etag: &str) -> (&'static str, String) {
    ("if-match", etag.to_owned())
}

/// A response: status, headers, the exact body bytes and the parsed JSON.
pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub raw: Vec<u8>,
    pub json: Json,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn error_code(&self) -> &str {
        self.json["error"]["code"].as_str().unwrap_or("")
    }

    pub fn id(&self) -> RecordId {
        spot_http::api::codec::parse_record_id(self.json["data"]["id"].as_str().unwrap()).unwrap()
    }

    pub fn etag(&self) -> String {
        self.header("etag").unwrap().to_owned()
    }
}

/// One request through the whole router (site resolution, request id, authentication and the
/// idempotency middleware all run).
pub async fn call(
    router: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, String)],
    body: Option<Json>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    let mut bytes = Vec::new();
    if let Some(body) = body {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        bytes = serde_json::to_vec(&body).unwrap();
    }
    for (name, value) in headers {
        builder = builder.header(*name, value.clone());
    }
    let request = builder.body(Body::from(bytes)).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let raw = axum::body::to_bytes(response.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    let json = serde_json::from_slice::<Json>(&raw).unwrap_or(Json::Null);
    Reply {
        status,
        headers,
        raw,
        json,
    }
}

// ---------------------------------------------------------------------------------------------
// Looking for a secret
// ---------------------------------------------------------------------------------------------

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every form in which `plain` could show up in a stored body: the text, its hex, its base64
/// and its base64url (padded and not).
pub fn forms_of(plain: &str) -> Vec<Vec<u8>> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let b = plain.as_bytes();
    vec![
        b.to_vec(),
        hex(b).into_bytes(),
        STANDARD.encode(b).into_bytes(),
        STANDARD_NO_PAD.encode(b).into_bytes(),
        URL_SAFE.encode(b).into_bytes(),
        URL_SAFE_NO_PAD.encode(b).into_bytes(),
    ]
}

/// Whether `haystack` carries `plain` in any form, or any run of 8 or more bytes of it.
/// Reports a boolean, never the value.
pub fn leaks(haystack: &[u8], plain: &str) -> bool {
    const RUN: usize = 8;
    if forms_of(plain).iter().any(|form| contains(haystack, form)) {
        return true;
    }
    let bytes = plain.as_bytes();
    bytes.windows(RUN).any(|run| contains(haystack, run))
}

/// Whether `haystack` carries `secret` (raw bytes, such as a digest or ciphertext), its hex or
/// its base64, or a run of 8 or more bytes of it.
pub fn leaks_bytes(haystack: &[u8], secret: &[u8]) -> bool {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    let forms: [Vec<u8>; 5] = [
        secret.to_vec(),
        hex(secret).into_bytes(),
        STANDARD.encode(secret).into_bytes(),
        STANDARD_NO_PAD.encode(secret).into_bytes(),
        URL_SAFE_NO_PAD.encode(secret).into_bytes(),
    ];
    forms.iter().any(|form| contains(haystack, form))
        || secret.windows(8).any(|run| contains(haystack, run))
}
