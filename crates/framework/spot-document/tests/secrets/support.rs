//! Shared by every module of the secret-path suite: the DocTypes, the rig (an engine with a
//! vault on the fake backend or on PostgreSQL), and the helpers that look for a secret in
//! everything the engine and storage could show.

pub use std::sync::Arc;
pub use std::sync::atomic::{AtomicBool, Ordering};
pub use std::time::Duration;

pub use crate::common::{n, now};
pub use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation};
pub use spot_app_api::controller::{StepDecl, StepName};
pub use spot_app_api::prelude::{RecordId, SiteId, Value, Version};
pub use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, DocumentPatch, ErrorClass, HookPoint, Phase, Target,
};
pub use spot_core::ManualClock;
pub use spot_document::engine::{Engine, RetryPolicy, V7Ids};
pub use spot_document::secrets::{RewrapBatch, SecretAccess, SecretCaller, SecretOp};
pub use spot_events_contract::lifecycle::Operation;
pub use spot_meta::{Definition, FieldLookup, FieldSpec, FieldType, Registry};
pub use spot_postgres::PgBackend;
pub use spot_postgres::testing::TestDb;
pub use spot_seams::RegistryBuilder;
pub use spot_secrets::{MemoryKeyring, SecretBox, SecretString};
pub use spot_storage_contract::{
    Catalog, ChangeFeed, Collation, DocumentData, DocumentStore, FeedPosition, Isolation, RowLock,
    StorageBackend, Transaction, TxOptions,
};
pub use spot_storage_fake::FakeBackend;

pub const SITE: &str = "acme.test";
pub const PASSWORD: &str = "correct horse battery staple 7731";
pub const TOKEN: &str = "tok_live_8f3a9c1e5d7b2046";

// ---------------------------------------------------------------------------------------------
// The DocTypes
// ---------------------------------------------------------------------------------------------

pub fn secret(name: &str, lookup: bool, unique: bool) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Secret);
    if lookup {
        f.lookup = Some(FieldLookup::Index(true));
    }
    f.unique = unique;
    f
}

pub fn definitions() -> Vec<Definition> {
    let mut account = Definition::new("account");
    // Posted, so the suite can post and cancel an account.
    account.is_posted = true;
    account.fields = vec![
        FieldSpec::new("label", FieldType::Text),
        // Not a lookup field.
        secret("password", false, false),
        // A lookup field that is unique (a session token's digest).
        secret("token", true, true),
        // A lookup field that is not unique.
        secret("api_key", true, false),
    ];
    let mut crate_ = Definition::new("crate_box");
    crate_.fields = vec![{
        let mut f = FieldSpec::new("lines", FieldType::Table);
        f.options = Some("crate_line".into());
        f
    }];
    let mut line = Definition::new("crate_line");
    line.is_child = true;
    line.fields = vec![FieldSpec::new("what", FieldType::Text)];
    // A DocType with child tables and a secret (a rewrap must keep the rows).
    let mut vault_box = Definition::new("vault_box");
    vault_box.fields = vec![
        FieldSpec::new("label", FieldType::Text),
        secret("token", true, true),
        {
            let mut f = FieldSpec::new("entries", FieldType::Table);
            f.options = Some("vault_entry".into());
            f
        },
    ];
    let mut entry = Definition::new("vault_entry");
    entry.is_child = true;
    entry.fields = vec![FieldSpec::new("what", FieldType::Text)];
    vec![account, crate_, line, vault_box, entry]
}

pub fn meta() -> Arc<spot_meta::EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

pub fn catalog() -> Arc<Catalog> {
    Arc::new(meta().catalog(Collation::new("und").unwrap()).unwrap())
}

// ---------------------------------------------------------------------------------------------
// A controller that can be told to fail after a write, to test rollback
// ---------------------------------------------------------------------------------------------

pub struct Tripwire {
    pub armed: Arc<AtomicBool>,
}

impl Controller for Tripwire {
    fn doctype(&self) -> &str {
        "account"
    }

    fn steps(&self) -> Vec<StepDecl> {
        [Operation::Create, Operation::Update]
            .into_iter()
            .map(|op| StepDecl {
                name: StepName::new(&format!("probe:trip_{op:?}").to_lowercase()).unwrap(),
                point: HookPoint::of(Phase::After, Target::Op(op)),
                priority: 0,
                budget: Duration::from_secs(5),
            })
            .collect()
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        let armed = self.armed.load(Ordering::SeqCst);
        Box::pin(async move {
            if armed {
                Err(AppError::new(
                    ErrorClass::Validation,
                    "probe.tripwire",
                    "The tripwire fired after the write.",
                ))
            } else {
                Ok(())
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The rig
// ---------------------------------------------------------------------------------------------

pub struct Rig<B: StorageBackend> {
    pub engine: Engine<B>,
    pub keyring: Arc<MemoryKeyring>,
    pub tripwire: Arc<AtomicBool>,
    /// The scratch database, held so that dropping the rig drops it. Nothing reads it: the
    /// stored row is read back through the storage contract, because ADR-045 (and the G5 lint)
    /// keep hand-written SQL out of this crate.
    _db: Option<TestDb>,
}

pub fn build<B: StorageBackend>(
    backend: B,
    keyring: Arc<MemoryKeyring>,
    db: Option<TestDb>,
) -> Rig<B> {
    let vault = Arc::new(SecretBox::new(keyring.clone()));
    build_with(backend, keyring, vault, db)
}

/// A rig whose engine uses `vault` (a double that wraps the real one, for instance).
pub fn build_with<B: StorageBackend>(
    backend: B,
    keyring: Arc<MemoryKeyring>,
    vault: Arc<dyn spot_document::secrets::SecretVault>,
    db: Option<TestDb>,
) -> Rig<B> {
    let tripwire = Arc::new(AtomicBool::new(false));
    let mut builder = RegistryBuilder::new();
    builder.add_controller(
        "probe",
        Arc::new(Tripwire {
            armed: tripwire.clone(),
        }),
    );
    let engine = Engine::new(
        backend,
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_retry_policy(RetryPolicy {
        max_retries: 3,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    })
    .with_secrets(vault);
    Rig {
        engine,
        keyring,
        tripwire,
        _db: db,
    }
}

pub fn keyring() -> Arc<MemoryKeyring> {
    Arc::new(MemoryKeyring::new("test-kek").unwrap())
}

pub async fn rig_fake() -> Option<Rig<FakeBackend>> {
    let rig = build(FakeBackend::new(), keyring(), None);
    rig.engine.prepare().await.unwrap();
    Some(rig)
}

pub async fn rig_pg() -> Option<Rig<PgBackend>> {
    let Some(db) = TestDb::create().await else {
        // `SPOT_REQUIRE_PG=1` (set where a PostgreSQL is expected) turns a silent skip into a
        // failure, so the PostgreSQL variants cannot go unrun without anyone noticing.
        assert!(
            std::env::var_os("SPOT_REQUIRE_PG").is_none(),
            "SPOT_REQUIRE_PG is set but SPOT_TEST_PG_URL gives no database"
        );
        return None;
    };
    // The product requires PostgreSQL 18, and `connect` refuses any other. A developer machine
    // that has only an older server may say so with `SPOT_PG_UNCHECKED=1` (never set in CI).
    let backend = if std::env::var_os("SPOT_PG_UNCHECKED").is_some() {
        PgBackend::connect_unchecked(db.config()).unwrap()
    } else {
        PgBackend::connect(db.config()).await.unwrap()
    };
    let rig = build(backend, keyring(), Some(db));
    rig.engine.prepare().await.unwrap();
    Some(rig)
}

pub fn inv_at(site: &str) -> Invocation {
    Invocation {
        site: SiteId::new(site).unwrap(),
        actor: Actor::System {
            component: n("login_service"),
        },
        command_id: None,
        language: None,
        now: now(),
    }
}

pub fn inv() -> Invocation {
    inv_at(SITE)
}

pub fn s(text: &str) -> SecretString {
    SecretString::new(text)
}

impl<B: StorageBackend + ChangeFeed> Rig<B> {
    pub fn access(&self) -> SecretAccess {
        self.engine.secret_access(SecretCaller::Test, &inv())
    }

    pub fn access_at(&self, site: &str) -> SecretAccess {
        self.engine.secret_access(SecretCaller::Test, &inv_at(site))
    }

    /// A saved account with a label and no secret.
    pub async fn account(&self, label: &str) -> (RecordId, Version) {
        let mut doc = spot_app_api::Document::new(n("account"), RecordId::new_v7());
        doc.set(n("label"), label);
        let saved = self.engine.create(&inv(), doc).await.unwrap().value;
        (saved.id(), saved.version().unwrap())
    }

    /// The stored row, secret columns included, read behind the engine's back.
    pub async fn raw(&self, id: RecordId) -> DocumentData {
        let mut tx = self
            .engine
            .backend()
            .begin(
                catalog(),
                TxOptions::new(n("raw_read"), Isolation::ReadCommitted),
            )
            .await
            .unwrap();
        let row = tx.load(&n("account"), id, RowLock::None).await.unwrap();
        tx.rollback().await.unwrap();
        row.unwrap().data
    }

    /// Overwrites one column of the stored row behind the engine's back (to model an attacker
    /// who can write to the table).
    pub async fn poke(&self, id: RecordId, column: &str, value: Value) {
        let mut tx = self
            .engine
            .backend()
            .begin(
                catalog(),
                TxOptions::new(n("raw_write"), Isolation::ReadCommitted),
            )
            .await
            .unwrap();
        let mut row = tx
            .load(
                &n("account"),
                id,
                RowLock::Update(spot_storage_contract::Wait::Block),
            )
            .await
            .unwrap()
            .unwrap();
        row.data.fields.insert(n(column), value);
        tx.update(&row.data, row.version).await.unwrap();
        tx.commit().await.unwrap();
    }

    pub async fn sealed(&self, id: RecordId, field: &str) -> Option<Vec<u8>> {
        match self
            .raw(id)
            .await
            .fields
            .get(&n(&format!("{field}__sealed")))
        {
            Some(Value::Bytes(b)) => Some(b.clone()),
            _ => None,
        }
    }

    pub async fn bidx(&self, id: RecordId, field: &str) -> Option<Vec<u8>> {
        match self.raw(id).await.fields.get(&n(&format!("{field}__bidx"))) {
            Some(Value::Bytes(b)) => Some(b.clone()),
            _ => None,
        }
    }

    /// Everything the engine and storage could show about the given records, as one text: the
    /// stored rows (every declared column, as bytes and as text), the documents as the engine
    /// loads them, the outbox, the secret audit log and the extra strings (errors). A secret
    /// field has no plain column at all (ADR-068 section 2), so the declared columns of the row
    /// are the whole of the table — and reading them needs no statement of our own (ADR-045).
    pub async fn everything(&self, ids: &[RecordId], extra: &[String]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        for id in ids {
            let row = self.raw(*id).await;
            for (name, value) in &row.fields {
                out.extend_from_slice(name.as_str().as_bytes());
                out.extend_from_slice(format!("{value:?}").as_bytes());
                match value {
                    Value::Text(t) | Value::Json(t) => out.extend_from_slice(t.as_bytes()),
                    Value::Bytes(b) => out.extend_from_slice(b),
                    _ => {}
                }
            }
            let doc = self.engine.peek("account", *id).await.unwrap();
            out.extend_from_slice(format!("{doc:?}").as_bytes());
            let loaded = self.engine.load(&inv(), "account", *id).await.unwrap();
            out.extend_from_slice(format!("{loaded:?}").as_bytes());
        }
        let events = self
            .engine
            .backend()
            .read_outbox(FeedPosition::START, 10_000)
            .await
            .unwrap();
        for record in events {
            out.extend_from_slice(format!("{:?}", record.event).as_bytes());
            out.extend_from_slice(record.event.payload.as_bytes());
        }
        out.extend_from_slice(format!("{:?}", self.engine.secret_audit_log()).as_bytes());
        for e in extra {
            out.extend_from_slice(e.as_bytes());
        }
        out
    }
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The forms in which a plaintext or a sealed value could show up in a text.
pub fn forms_of_plain(plain: &str) -> Vec<Vec<u8>> {
    vec![
        plain.as_bytes().to_vec(),
        hex(plain.as_bytes()).into_bytes(),
    ]
}

pub fn forms_of_sealed(sealed: &[u8]) -> Vec<Vec<u8>> {
    let text = spot_secrets::Sealed::from_bytes(sealed)
        .unwrap()
        .to_string();
    let body = text.trim_start_matches("spsec:v1:").to_owned();
    vec![
        hex(sealed).into_bytes(),
        body.into_bytes(),
        // A slice of the ciphertext, in case it is shown as a prefix.
        sealed[sealed.len() - 12..].to_vec(),
    ]
}

pub fn assert_absent(haystack: &[u8], needles: &[Vec<u8>], what: &str) {
    for needle in needles {
        assert!(
            !contains(haystack, needle),
            "{what} holds a secret form: {}",
            String::from_utf8_lossy(needle)
        );
    }
}

pub fn code_of<T: std::fmt::Debug>(r: Result<T, AppError>) -> String {
    match r {
        Ok(v) => panic!("expected an error, got {v:?}"),
        Err(e) => e.code.clone(),
    }
}

pub async fn futures_sealed<B: StorageBackend + ChangeFeed>(
    rig: &Rig<B>,
    ids: &[RecordId],
) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for id in ids {
        out.push(rig.sealed(*id, "password").await.unwrap());
    }
    out
}

/// Runs each named case on the in-memory backend and, when `SPOT_TEST_PG_URL` is set, on
/// PostgreSQL. Expands to two test modules in the calling module.
macro_rules! cases {
    ($($case:ident),+ $(,)?) => {
        mod on_the_fake_backend {
            use super::*;
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn $case() {
                    let rig = rig_fake().await.expect("the fake backend");
                    super::$case(&rig).await;
                }
            )+
        }
        mod on_postgres {
            use super::*;
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                async fn $case() {
                    let Some(rig) = rig_pg().await else { return };
                    super::$case(&rig).await;
                }
            )+
        }
    };
}
pub(crate) use cases;
