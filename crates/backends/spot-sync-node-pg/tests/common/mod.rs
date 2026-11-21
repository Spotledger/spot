//! Shared setup: a scratch database with a bridge, the storage catalog the test records use, and
//! the doubles the apply path takes (ownership, audit, quarantine, clock, failpoint).
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_replica_apply::{
    DocPayload, OriginFields, OwnerCheck, Ownership, Reason, ReceivedRecord, RecordPayload,
    RecordingAudit, RecordingQuarantine,
};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, DocumentStore, EntityDef, EntityKind, FieldDef, FieldType,
    Fields, Isolation, Name, RecordId, RowLock, StorageBackend, Transaction, TxOptions, Value,
};
use spot_sync_closure::{Bundle, BundleKind, BundleRecord, RecordKey, Role};
use spot_sync_node_pg::{
    ApplyClock, ApplyEnv, ApplyLimits, Db, Failpoint, Failpoints, PgBundleApply, StaticCatalog,
};
use spot_sync_protocol::SourceId;

pub const POOL: usize = 8;
pub const TENANT: &str = "acme";

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (with a printed message) when no server is configured; with
/// `SPOT_REQUIRE_PG=1` a missing server fails.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

pub fn name(s: &str) -> Name {
    Name::new(s).expect("name")
}

fn field(n: &str, ty: FieldType) -> FieldDef {
    FieldDef {
        name: name(n),
        ty,
        nullable: true,
    }
}

/// An `item` entity with the three origin fields a mirror carries.
pub fn catalog() -> Catalog {
    let item = EntityDef {
        name: name("item"),
        kind: EntityKind::Document,
        fields: vec![
            field("title", FieldType::Text { max_chars: None }),
            field("qty", FieldType::Int),
            field("origin_node", FieldType::Text { max_chars: None }),
            field("origin_id", FieldType::Text { max_chars: None }),
            field("origin_version", FieldType::Int),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    Catalog::new(1, Collation::new("en").expect("collation"), vec![item]).expect("catalog")
}

pub fn hub_source() -> SourceId {
    SourceId::new("hub.example.app").expect("source")
}

/// Refuses a record whose title is `not-mine` (the sender does not own it); mirrors the rest.
pub struct TestOwners;

impl OwnerCheck for TestOwners {
    fn allow_replica(
        &self,
        _sender: &SourceId,
        record: &ReceivedRecord,
        _at: i64,
    ) -> Result<Ownership, Reason> {
        match record.document.fields.get(&name("title")) {
            Some(Value::Text(t)) if t == "not-mine" => Err(Reason::NotOwner {
                owner: "other.node".to_owned(),
            }),
            _ => Ok(Ownership::Mirror),
        }
    }
}

pub struct TestClock(pub AtomicI64);

impl ApplyClock for TestClock {
    fn now_micros(&self) -> i64 {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

/// Crashes at the point it is armed with.
#[derive(Default)]
pub struct Crash(pub Mutex<Option<Failpoint>>);

impl Crash {
    pub fn arm(&self, point: Option<Failpoint>) {
        *self.0.lock().unwrap() = point;
    }
}

impl Failpoints for Crash {
    fn crash_at(&self, point: Failpoint) -> bool {
        *self.0.lock().unwrap() == Some(point)
    }
}

pub struct World {
    pub fx: Fixture,
    pub backend: Arc<PgBackend>,
    pub bridge: Arc<Bridge>,
    pub db: Db,
    pub audit: Arc<RecordingAudit>,
    pub quarantine: Arc<RecordingQuarantine>,
    pub crash: Arc<Crash>,
}

pub fn bridge_for(config: PgConfig) -> (Arc<PgBackend>, Arc<Bridge>) {
    let mut config = config;
    config.pool_max_size = POOL;
    let backend = Arc::new(PgBackend::connect_unchecked(config).expect("backend"));
    let bridge = Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "sync_node_pg_test".to_owned(),
                workers: 2,
                max_in_flight: 3,
                queue_capacity: 16,
                default_deadline: Duration::from_secs(30),
                max_deadline: Duration::from_secs(120),
            },
            &backend,
        )
        .expect("bridge"),
    );
    (backend, bridge)
}

/// A world on a scratch database: backend, bridge, prepared node tables and the `item` catalog.
pub fn world() -> Option<World> {
    let fx = fixture()?;
    let (backend, bridge) = bridge_for(fx.db.config());
    let db = Db::new(Arc::clone(&bridge), POOL).expect("db");
    db.prepare().expect("prepare");
    let prep = Arc::clone(&backend);
    bridge
        .run(Duration::from_secs(60), move |_conn| {
            Box::pin(async move { prep.prepare(&catalog()).await.map_err(|e| e.to_string()) })
        })
        .expect("catalog prepared");
    Some(World {
        fx,
        backend,
        bridge,
        db,
        audit: Arc::new(RecordingAudit::new()),
        quarantine: Arc::new(RecordingQuarantine::new()),
        crash: Arc::new(Crash::default()),
    })
}

impl World {
    pub fn env(&self) -> ApplyEnv {
        ApplyEnv {
            backend: Arc::clone(&self.backend),
            catalog: Arc::new(StaticCatalog(Arc::new(catalog()))),
            owner: Arc::new(TestOwners),
            audit: self.audit.clone(),
            quarantine: self.quarantine.clone(),
            clock: Arc::new(TestClock(AtomicI64::new(1_000))),
            origin: OriginFields::default(),
            max_skew: 0,
            limits: ApplyLimits::default(),
            failpoints: Some(self.crash.clone() as Arc<dyn Failpoints>),
        }
    }

    pub fn apply(&self, tenant: &str) -> PgBundleApply {
        PgBundleApply::new(self.db.clone(), tenant, hub_source(), self.env()).expect("apply")
    }

    /// The stored version and title of an item, read in a plain transaction.
    pub fn stored(&self, id: RecordId) -> Option<(u64, String)> {
        let backend = Arc::clone(&self.backend);
        self.bridge
            .run(Duration::from_secs(30), move |_conn| {
                Box::pin(async move {
                    let options =
                        TxOptions::new(name("test_read"), Isolation::ReadCommitted).read_only();
                    let mut tx = backend
                        .begin(Arc::new(catalog()), options)
                        .await
                        .map_err(|e| e.to_string())?;
                    let doc = tx
                        .load(&name("item"), id, RowLock::None)
                        .await
                        .map_err(|e| e.to_string())?;
                    let _ = tx.rollback().await;
                    Ok::<_, String>(doc.map(|d| {
                        let title = match d.data.fields.get(&name("title")) {
                            Some(Value::Text(t)) => t.clone(),
                            _ => String::new(),
                        };
                        (d.version.0, title)
                    }))
                })
            })
            .expect("read")
    }

    /// One integer from a query (a count).
    pub fn scalar(&self, sql: &'static str) -> i64 {
        self.bridge
            .run(Duration::from_secs(30), move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(sql, &[]).await.map_err(|e| e.to_string())?;
                    Ok::<_, String>(row.get::<_, i64>(0))
                })
            })
            .expect("scalar")
    }

    pub fn exec(&self, sql: &'static str) {
        self.bridge
            .run(Duration::from_secs(30), move |conn| {
                Box::pin(async move { conn.batch_execute(sql).await.map_err(|e| e.to_string()) })
            })
            .expect("exec");
    }
}

pub fn id_text(id: RecordId) -> String {
    id.to_string()
}

/// A mirror `item` record at `position` carrying `title`.
pub fn put(id: RecordId, title: &str, position: u64) -> BundleRecord {
    let mut fields: Fields = Fields::new();
    fields.insert(name("title"), Value::Text(title.to_owned()));
    fields.insert(name("qty"), Value::Int(i64::try_from(position).unwrap()));
    fields.insert(name("origin_node"), Value::Null);
    fields.insert(name("origin_id"), Value::Null);
    fields.insert(name("origin_version"), Value::Null);
    let doc = DocumentData {
        entity: name("item"),
        id,
        fields,
        children: BTreeMap::new(),
    };
    let payload = RecordPayload {
        document: DocPayload::from_document(&doc),
        entries: Vec::new(),
        meta_version: 1,
    };
    BundleRecord {
        role: Role::Root,
        key: RecordKey::new("item", id_text(id)),
        position,
        image: serde_json::to_value(&payload).expect("image"),
        deleted: false,
    }
}

/// A record whose image carries a field the entity does not declare (a shape error).
pub fn put_bad_shape(id: RecordId, position: u64) -> BundleRecord {
    let mut record = put(id, "bad", position);
    if let Some(fields) = record
        .image
        .get_mut("document")
        .and_then(|d| d.get_mut("fields"))
        .and_then(serde_json::Value::as_object_mut)
    {
        fields.insert(
            "undeclared".to_owned(),
            serde_json::json!({"t": "text", "v": "x"}),
        );
    }
    record
}

pub fn tomb(id: RecordId, position: u64) -> BundleRecord {
    BundleRecord {
        role: Role::Root,
        key: RecordKey::new("item", id_text(id)),
        position,
        image: serde_json::Value::Null,
        deleted: true,
    }
}

pub fn bundle(records: Vec<BundleRecord>) -> Bundle {
    let bytes = records
        .iter()
        .map(|r| serde_json::to_string(&r.image).map_or(0, |s| s.len()))
        .sum();
    Bundle {
        kind: BundleKind::Root,
        records,
        bytes,
    }
}
