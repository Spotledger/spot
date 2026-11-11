//! A storage rig over the platform app's DocTypes: an engine with a secrets vault, the four
//! stores, and the handful of raw-storage moves a test needs (create a user, issue a key, read a
//! stored row behind the engine's back).
//!
//! The same rig builds on `spot_storage_fake` and on PostgreSQL (`SPOT_TEST_PG_URL`), so every
//! property in `tests/storage.rs` and `tests/sign_in_table.rs` runs on both when a database is
//! configured. Nothing here is a product path: a test may take a `SecretAccess` as
//! `SecretCaller::Test` (the engine's `testing` feature) to seed the credential it is about to
//! attack.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

pub use std::sync::Arc;

pub use spot_app_api::Document;
pub use spot_app_api::context::{Actor, DocumentService, Invocation};
pub use spot_app_api::prelude::{Name, RecordId, SiteId, Value};
pub use spot_auth::{HashParams, OpaqueToken};
pub use spot_core::{Clock, ManualClock};
pub use spot_document::engine::{Engine, V7Ids};
pub use spot_document::secrets::{SecretAccess, SecretCaller};
pub use spot_login::{
    Argon2Hasher, IdentityEvents, LoginConfig, LoginService, LoginStores, PasswordHasher,
    UserDirectory,
};
pub use spot_meta::{EffectiveMeta, Registry};
pub use spot_platform_app::doctype::{API_KEY, LOGIN_ATTEMPT, SESSION, USER};
pub use spot_postgres::PgBackend;
pub use spot_postgres::testing::TestDb;
pub use spot_seams::RegistryBuilder;
pub use spot_secrets::{MemoryKeyring, SecretBox, SecretString};
pub use spot_sessions::store::Touched;
pub use spot_sessions::{
    ClientInfo, SaltKey, SessionManager, SessionPolicy, SessionStore, ThrottleConfig,
    ThrottleStore, UserKey,
};
pub use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, Fields, Isolation, QueryExecutor, RowLock,
    StorageBackend, Transaction, TxOptions, Wait,
};
pub use spot_storage_fake::FakeBackend;
pub use time::OffsetDateTime;

/// A name, panicking on a bad one (they are all static).
pub fn n(text: &str) -> Name {
    Name::new(text).expect("a valid name")
}

/// The platform app's metadata, compiled the way the serving engine compiles it.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(spot_platform_app::definitions().expect("the platform app's DocTypes"))
        .expect("a valid registry")
        .effective()
}

/// The catalog of `meta`.
pub fn catalog(meta: &EffectiveMeta) -> Arc<Catalog> {
    Arc::new(
        meta.catalog(Collation::new("und").expect("a collation"))
            .expect("the catalog compiles"),
    )
}

/// The site every rig works in.
pub fn site() -> SiteId {
    SiteId::new("acme.test").expect("a valid site id")
}

/// The start of a rig's clock.
pub fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_000_000).expect("a valid instant")
}

/// One site's engine, stores and clock.
pub struct Rig<B: StorageBackend> {
    pub engine: Arc<Engine<B>>,
    pub stores: Arc<LoginStores<B>>,
    pub clock: Arc<ManualClock>,
    pub catalog: Arc<Catalog>,
    /// The scratch database, held so that dropping the rig drops it.
    _db: Option<TestDb>,
}

fn keyring() -> Arc<MemoryKeyring> {
    Arc::new(MemoryKeyring::new("test-kek").expect("a key ring"))
}

fn invocation(clock: &Arc<ManualClock>) -> Invocation {
    Invocation {
        site: site(),
        actor: Actor::System {
            component: n("login_service"),
        },
        command_id: None,
        language: None,
        now: clock.now_utc(),
    }
}

pub fn build<B: StorageBackend>(backend: B, db: Option<TestDb>, at: OffsetDateTime) -> Rig<B> {
    let meta = meta();
    let catalog = catalog(&meta);
    let clock = Arc::new(ManualClock::new(at));
    let engine = Arc::new(
        Engine::new(
            backend,
            meta,
            Arc::new(RegistryBuilder::new().build().expect("an empty registry")),
            clock.clone(),
            Arc::new(V7Ids),
        )
        .expect("an engine")
        .with_secrets(Arc::new(SecretBox::new(keyring()))),
    );
    let stores = Arc::new(LoginStores::new(
        engine.clone(),
        catalog.clone(),
        &invocation(&clock),
        clock.clone(),
    ));
    Rig {
        engine,
        stores,
        clock,
        catalog,
        _db: db,
    }
}

/// A rig on the in-memory backend.
pub async fn fake() -> Rig<FakeBackend> {
    fake_at(start()).await
}

/// A rig on the in-memory backend, with its clock at `at`.
pub async fn fake_at(at: OffsetDateTime) -> Rig<FakeBackend> {
    let rig = build(FakeBackend::new(), None, at);
    rig.engine.prepare().await.expect("the fake backend");
    rig
}

/// A rig on PostgreSQL, or `None` when no test database is configured.
pub async fn postgres() -> Option<Rig<PgBackend>> {
    postgres_at(start()).await
}

/// A rig on PostgreSQL with its clock at `at`, or `None` when none is configured.
pub async fn postgres_at(at: OffsetDateTime) -> Option<Rig<PgBackend>> {
    let db = TestDb::create().await?;
    let backend = if std::env::var_os("SPOT_PG_UNCHECKED").is_some() {
        PgBackend::connect_unchecked(db.config()).expect("a checked-out connection")
    } else {
        PgBackend::connect(db.config())
            .await
            .expect("a PostgreSQL 18 connection")
    };
    let rig = build(backend, Some(db), at);
    rig.engine.prepare().await.expect("the PostgreSQL schema");
    Some(rig)
}

impl<B: StorageBackend> Rig<B> {
    /// The invocation the service acts under.
    pub fn inv(&self) -> Invocation {
        invocation(&self.clock)
    }

    /// A `SecretAccess` for tests: the engine's own `SecretCaller::Test`, so a test can seed a
    /// credential the way a hostile or a legacy writer could.
    pub fn access(&self) -> SecretAccess {
        self.engine.secret_access(SecretCaller::Test, &self.inv())
    }

    /// A user record with `user_name` and `state`.
    pub async fn user(&self, user_name: &str, state: &str) -> RecordId {
        let mut doc = Document::new(n(USER), RecordId::new_v7());
        doc.set(n("user_name"), Value::Text(user_name.to_owned()));
        doc.set(n("full_name"), Value::Text(user_name.to_owned()));
        doc.set(
            n("email"),
            Value::Text(format!("{}@example.test", user_name.replace('@', "."))),
        );
        doc.set(n("state"), Value::Text(state.to_owned()));
        let saved = self
            .engine
            .create(&self.inv(), doc)
            .await
            .expect("a user record")
            .value;
        saved.id()
    }

    /// Sets a user's password through the directory (the product path for a credential write).
    pub async fn set_password(&self, user: RecordId, password: &str) {
        let hasher = Argon2Hasher::new(HashParams::for_tests());
        let hash = hasher.hash(&spot_auth::Password::new(password));
        self.stores
            .user_directory()
            .set_password_hash(user, hash)
            .await
            .expect("the password is stored");
    }

    /// Enrols a TOTP secret through the directory, answering its base32 form (a `TotpSecret`
    /// is not `Clone`: it is secret-class material).
    pub async fn enrol_totp(&self, user: RecordId) -> String {
        let base32 = spot_auth::TotpSecret::generate().to_base32();
        let secret = spot_auth::TotpSecret::from_base32(&base32).expect("a fresh secret");
        self.stores
            .user_directory()
            .set_second_factor(user, Some(secret), Vec::new())
            .await
            .expect("the second factor is stored");
        base32
    }

    /// Issues an `api_key` record for `owner`, with the sealed digest of `token`.
    pub async fn issue_key(
        &self,
        owner: RecordId,
        key_id: &str,
        token: &OpaqueToken,
        state: &str,
        expires: Option<i64>,
    ) {
        let mut doc = Document::new(n(API_KEY), RecordId::new_v7());
        doc.set(n("key_id"), Value::Text(key_id.to_owned()));
        doc.set(n("owner"), Value::Id(owner));
        doc.set(n("state"), Value::Text(state.to_owned()));
        if let Some(expires) = expires {
            doc.set(
                n("expires_at"),
                Value::DateTime(
                    OffsetDateTime::from_unix_timestamp(expires).expect("a valid instant"),
                ),
            );
        }
        self.engine
            .create_with_secrets(
                &self.access(),
                doc,
                vec![(
                    "digest".to_owned(),
                    SecretString::new(hex(token.digest().as_bytes())),
                )],
            )
            .await
            .expect("an api_key record");
    }

    /// Records one `login_attempt` row, the way the service's event sink does.
    pub async fn attempt(&self, account: &str, source_digest: &str, result: &str, at: i64) {
        let mut doc = Document::new(n(LOGIN_ATTEMPT), RecordId::new_v7());
        doc.set(n("account"), Value::Text(account.to_owned()));
        doc.set(n("source_digest"), Value::Text(source_digest.to_owned()));
        doc.set(n("result_code"), Value::Text(result.to_owned()));
        doc.set(
            n("at"),
            Value::DateTime(OffsetDateTime::from_unix_timestamp(at).expect("a valid instant")),
        );
        self.engine
            .create(&self.inv(), doc)
            .await
            .expect("a login attempt");
    }

    /// The stored row of `entity`/`id` and the version it was read at.
    pub async fn row(
        &self,
        entity: &str,
        id: RecordId,
    ) -> (DocumentData, spot_storage_contract::Version) {
        let mut tx = self
            .engine
            .backend()
            .begin(
                self.catalog.clone(),
                TxOptions::new(n("test_raw"), Isolation::ReadCommitted).read_only(),
            )
            .await
            .expect("a transaction");
        let row = tx
            .load(&n(entity), id, RowLock::None)
            .await
            .expect("a load");
        let _ = tx.rollback().await;
        let row = row.expect("the record exists");
        (row.data, row.version)
    }

    /// The stored row of `entity`/`id`, secret columns included, read behind the engine.
    pub async fn raw(&self, entity: &str, id: RecordId) -> DocumentData {
        self.row(entity, id).await.0
    }

    /// The record id of the `api_key` row whose public id is `key_id`.
    pub async fn api_key_row_id(&self, key_id: &str) -> RecordId {
        use spot_storage_contract::{Expr, Query, QueryPath, Selection};
        let mut tx = self
            .engine
            .backend()
            .begin(
                self.catalog.clone(),
                TxOptions::new(n("test_key_id"), Isolation::ReadCommitted).read_only(),
            )
            .await
            .expect("a transaction");
        let alias = n("k");
        let mut query = Query::from(&n(API_KEY), &alias, QueryPath::Report);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &n("id")),
            alias: n("v"),
        }];
        query.filter =
            Some(Expr::field(&alias, &n("key_id")).eq(Expr::value(Value::Text(key_id.to_owned()))));
        query.limit = Some(1);
        let rows = tx.query(&query).await.expect("the key query");
        let _ = tx.rollback().await;
        rows.rows
            .into_iter()
            .flatten()
            .find_map(|value| match value {
                Value::Id(id) => Some(id),
                _ => None,
            })
            .expect("the api_key row")
    }

    /// Overwrites one column of a stored row behind the engine's back (an attacker, or a
    /// migration that copied a blob to the wrong place).
    pub async fn poke(&self, entity: &str, id: RecordId, column: &str, value: Value) {
        let mut tx = self
            .engine
            .backend()
            .begin(
                self.catalog.clone(),
                TxOptions::new(n("test_poke"), Isolation::ReadCommitted),
            )
            .await
            .expect("a transaction");
        let row = tx
            .load(&n(entity), id, RowLock::Update(Wait::Block))
            .await
            .expect("a load")
            .expect("the record exists");
        let mut data = row.data;
        data.fields.insert(n(column), value);
        tx.update(&data, row.version).await.expect("a write");
        tx.commit().await.expect("a commit");
    }

    /// The stored value of one column, when it is bytes.
    pub async fn bytes(&self, entity: &str, id: RecordId, column: &str) -> Option<Vec<u8>> {
        match self.raw(entity, id).await.fields.get(&n(column)) {
            Some(Value::Bytes(bytes)) => Some(bytes.clone()),
            _ => None,
        }
    }

    /// The engine's secret audit log, rendered.
    pub fn audit(&self) -> String {
        format!("{:?}", self.engine.secret_audit_log())
    }

    /// The session store, erased to the contract's object.
    pub fn sessions(&self) -> Arc<dyn SessionStore> {
        Arc::new(self.stores.session_store())
    }

    /// The throttle store, erased to the contract's object.
    pub fn throttle(&self, config: &ThrottleConfig) -> Arc<dyn ThrottleStore> {
        Arc::new(self.stores.throttle_store(config.window))
    }

    /// A session manager over this rig's store.
    pub fn manager(&self, now_policy: SessionPolicy) -> SessionManager<Arc<dyn SessionStore>> {
        SessionManager::new(self.sessions(), now_policy, Arc::new(SaltKey::generate()))
    }

    /// Enrols a fixed TOTP secret (base32), so a case can use a known RFC vector.
    pub async fn set_totp(&self, user: RecordId, base32: &str) {
        let secret = spot_auth::TotpSecret::from_base32(base32).expect("a valid seed");
        self.stores
            .user_directory()
            .set_second_factor(user, Some(secret), Vec::new())
            .await
            .expect("the second factor is stored");
    }

    /// Issues an `api_key` record whose digest is `digest`.
    pub async fn issue_key_with(
        &self,
        owner: RecordId,
        key_id: &str,
        digest: &spot_auth::TokenDigest,
        state: &str,
        expires: Option<i64>,
    ) {
        let mut doc = Document::new(n(API_KEY), RecordId::new_v7());
        doc.set(n("key_id"), Value::Text(key_id.to_owned()));
        doc.set(n("owner"), Value::Id(owner));
        doc.set(n("state"), Value::Text(state.to_owned()));
        if let Some(expires) = expires {
            doc.set(
                n("expires_at"),
                Value::DateTime(
                    OffsetDateTime::from_unix_timestamp(expires).expect("a valid instant"),
                ),
            );
        }
        self.engine
            .create_with_secrets(
                &self.access(),
                doc,
                vec![(
                    "digest".to_owned(),
                    SecretString::new(hex(digest.as_bytes())),
                )],
            )
            .await
            .expect("an api_key record");
    }

    /// The whole sign-in service over the storage stores.
    pub fn service(
        &self,
        events: Arc<dyn IdentityEvents>,
        hasher: Arc<dyn PasswordHasher>,
    ) -> LoginService {
        self.service_with(LoginConfig::for_tests(), events, hasher)
    }

    /// The same, with an explicit configuration.
    pub fn service_with(
        &self,
        config: LoginConfig,
        events: Arc<dyn IdentityEvents>,
        hasher: Arc<dyn PasswordHasher>,
    ) -> LoginService {
        let throttle = self.throttle(&config.throttle);
        let clock: Arc<dyn Clock> = self.clock.clone();
        LoginService::new(
            config,
            self.sessions(),
            throttle,
            Arc::new(self.stores.user_directory()),
            Arc::new(self.stores.key_store()),
            events,
            clock,
            hasher,
        )
        .expect("the service builds")
    }
}

/// A session store that owns the platform `user` records the contract suite's opaque user keys
/// (`"u1"`, `"u2"`) stand for.
///
/// The suite keys sessions by an arbitrary `UserKey` text; the platform app's `session.user` is
/// a `link` to a real `user` record, which storage checks at commit. This adapter is the site's
/// user directory for the suite: it creates one user record per key on first use and relabels
/// every record it returns with the key it was asked about.
///
/// The adapter also remembers the salted **source digest** of the sessions it created, because
/// the platform app's `session` DocType has no column for it (the store's
/// `list_for_user`/`active_sources` answer with the zero digest; the report lists the missing
/// column). Everything else it returns is exactly the storage store's own record: the
/// properties the suite proves are the store's, not the adapter's.
pub struct Linked<B: StorageBackend> {
    inner: spot_login::StorageSessionStore<B>,
    engine: Arc<Engine<B>>,
    inv: Invocation,
    users: std::sync::Mutex<std::collections::BTreeMap<String, RecordId>>,
    sources: std::sync::Mutex<std::collections::BTreeMap<String, spot_sessions::SourceHash>>,
}

impl<B: StorageBackend> Linked<B> {
    /// A contract-suite view over `rig`'s session store.
    pub fn new(rig: &Rig<B>) -> Self {
        Self {
            inner: rig.stores.session_store(),
            engine: rig.engine.clone(),
            inv: rig.inv(),
            users: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            sources: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    fn sources(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, spot_sessions::SourceHash>>
    {
        self.sources
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Remembers the source digest of a session the suite created.
    fn remember_source(&self, record: &spot_sessions::Session) {
        self.sources()
            .insert(record.id.clone(), record.source.clone());
    }

    /// Restores the source digest the platform app has no column for.
    fn restore_source(&self, record: &mut spot_sessions::Session) {
        if let Some(source) = self.sources().get(&record.id) {
            record.source = source.clone();
        }
    }

    fn users(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeMap<String, RecordId>> {
        self.users
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The record id `key` stands for, creating its user record once.
    async fn id_for(&self, key: &str) -> RecordId {
        if let Some(id) = self.users().get(key) {
            return *id;
        }
        let mut doc = Document::new(n(USER), RecordId::new_v7());
        doc.set(n("user_name"), Value::Text(format!("contract-{key}")));
        doc.set(n("full_name"), Value::Text(format!("contract {key}")));
        doc.set(n("state"), Value::Text("active".to_owned()));
        let saved = self
            .engine
            .create(&self.inv, doc)
            .await
            .expect("a contract user record")
            .value;
        let id = saved.id();
        self.users().insert(key.to_owned(), id);
        id
    }

    /// The key text `id` stands for, when the suite asked about it.
    fn key_for(&self, id: &str) -> String {
        self.users()
            .iter()
            .find(|(_, value)| value.to_string() == id)
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    fn relabel(&self, record: &mut spot_sessions::Session) {
        if let Ok(key) = UserKey::new(self.key_for(record.user.as_str())) {
            record.user = key;
        }
    }
}

impl<B: StorageBackend> SessionStore for Linked<B> {
    fn create<'a>(
        &'a self,
        mut session: spot_sessions::Session,
    ) -> spot_sessions::store::SessionFuture<'a, spot_sessions::Session> {
        Box::pin(async move {
            let key = session.user.as_str().to_owned();
            let id = self.id_for(&key).await;
            session.user = UserKey::new(id.to_string()).expect("a record id is a user key");
            let source = session.source.clone();
            let mut stored = self.inner.create(session).await?;
            stored.source = source;
            self.remember_source(&stored);
            self.relabel(&mut stored);
            Ok(stored)
        })
    }

    fn find_by_digest<'a>(
        &'a self,
        digest: &'a spot_auth::TokenDigest,
    ) -> spot_sessions::store::SessionFuture<'a, Option<spot_sessions::Session>> {
        Box::pin(async move {
            let mut found = self.inner.find_by_digest(digest).await?;
            if let Some(record) = &mut found {
                self.restore_source(record);
                self.relabel(record);
            }
            Ok(found)
        })
    }

    fn touch<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        idle: i64,
        absolute_expires: i64,
    ) -> spot_sessions::store::SessionFuture<'a, spot_sessions::store::Touched> {
        Box::pin(async move { self.inner.touch(id, now, idle, absolute_expires).await })
    }

    fn rotate<'a>(
        &'a self,
        id: &'a str,
        new_digest: spot_auth::TokenDigest,
    ) -> spot_sessions::store::SessionFuture<'a, ()> {
        Box::pin(async move { self.inner.rotate(id, new_digest).await })
    }

    fn revoke<'a>(
        &'a self,
        id: &'a str,
        reason: String,
    ) -> spot_sessions::store::SessionFuture<'a, spot_sessions::store::RevokeReport> {
        Box::pin(async move { self.inner.revoke(id, reason).await })
    }

    fn revoke_all_for_user<'a>(
        &'a self,
        user: &'a UserKey,
        except: Option<&'a str>,
        reason: String,
    ) -> spot_sessions::store::SessionFuture<'a, spot_sessions::store::RevokeReport> {
        Box::pin(async move {
            let key = UserKey::new(self.id_for(user.as_str()).await.to_string())
                .expect("a record id is a user key");
            self.inner.revoke_all_for_user(&key, except, reason).await
        })
    }

    fn list_for_user<'a>(
        &'a self,
        user: &'a UserKey,
    ) -> spot_sessions::store::SessionFuture<'a, Vec<spot_sessions::Session>> {
        Box::pin(async move {
            let key = UserKey::new(self.id_for(user.as_str()).await.to_string())
                .expect("a record id is a user key");
            let mut listed = self.inner.list_for_user(&key).await?;
            let text = user.as_str().to_owned();
            for record in &mut listed {
                self.restore_source(record);
                record.user = UserKey::new(text.clone()).expect("the key it was asked about");
            }
            Ok(listed)
        })
    }

    fn purge_expired<'a>(
        &'a self,
        now: i64,
        limit: usize,
    ) -> spot_sessions::store::SessionFuture<'a, usize> {
        Box::pin(async move { self.inner.purge_expired(now, limit).await })
    }

    fn oldest_active<'a>(
        &'a self,
        user: &'a UserKey,
        now: i64,
    ) -> spot_sessions::store::SessionFuture<'a, Option<spot_sessions::Session>> {
        Box::pin(async move {
            let key = UserKey::new(self.id_for(user.as_str()).await.to_string())
                .expect("a record id is a user key");
            let mut found = self.inner.oldest_active(&key, now).await?;
            if let Some(record) = &mut found {
                self.restore_source(record);
                record.user = UserKey::new(user.as_str()).expect("the key it was asked about");
            }
            Ok(found)
        })
    }

    fn exists<'a>(
        &'a self,
        digest: &'a spot_auth::TokenDigest,
    ) -> spot_sessions::store::SessionFuture<'a, bool> {
        Box::pin(async move { self.inner.exists(digest).await })
    }

    fn active_sources<'a>(
        &'a self,
        user: &'a UserKey,
    ) -> spot_sessions::store::SessionFuture<'a, Vec<spot_sessions::SourceHash>> {
        Box::pin(async move {
            let key = UserKey::new(self.id_for(user.as_str()).await.to_string())
                .expect("a record id is a user key");
            self.inner.active_sources(&key).await
        })
    }
}

/// The stored fields of a row with the engine-managed secret columns left out.
pub fn render_public(fields: &Fields) -> Vec<u8> {
    let public: Fields = fields
        .iter()
        .filter(|(name, _)| {
            !name.as_str().ends_with("__sealed") && !name.as_str().ends_with("__bidx")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    render(&public)
}

/// Base16, the stored form of a digest.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Base16 of every field of a stored row (names, values as `Debug`, text and bytes).
pub fn render(fields: &Fields) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(name.as_str().as_bytes());
        out.extend_from_slice(format!("{value:?}").as_bytes());
        match value {
            Value::Text(text) | Value::Json(text) => out.extend_from_slice(text.as_bytes()),
            Value::Bytes(bytes) => {
                out.extend_from_slice(hex(bytes).as_bytes());
                out.extend_from_slice(bytes);
            }
            _ => {}
        }
    }
    out
}

/// Whether `haystack` holds `needle`.
pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// The record id of a hyphenated id (a session's id as the manager wrote it).
pub fn record_id(text: &str) -> RecordId {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    let bytes: Vec<u8> = hex
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex"), 16).expect("a byte")
        })
        .collect();
    RecordId::from_bytes(bytes.try_into().expect("16 bytes")).expect("a record id")
}

/// A user key for an id.
pub fn user_key(id: RecordId) -> UserKey {
    UserKey::new(id.to_string()).expect("a valid user key")
}

/// The client label every rig session carries.
pub fn client() -> ClientInfo {
    ClientInfo::default()
}
