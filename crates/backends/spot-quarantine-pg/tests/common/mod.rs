//! Shared setup for the tests: a scratch database, pools, and record fixtures.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_postgres::testing::TestDb;
use spot_quarantine::{NewItem, QuarantineItem, RecordRef};
use spot_quarantine_pg::PgQuarantineStore;
use spot_replica_apply::ReceivedRecord;
use spot_storage_contract::{
    Catalog, Collation, DocumentData, EntityDef, EntityKind, FieldDef, FieldType, Fields,
    Isolation, Name, RecordId, TxOptions, Value,
};
use spot_sync_protocol::{RecordName, SourceId};
use tokio_postgres::NoTls;

/// A pool of `size` connections to the scratch database.
pub fn pool(db: &TestDb, size: usize, app: &str) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some(app.to_owned());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

/// A prepared store on a new pool (a new pool is how a "restart" is simulated).
pub async fn store(db: &TestDb, app: &str) -> PgQuarantineStore {
    let store = PgQuarantineStore::new(pool(db, 4, app));
    store.prepare().await.unwrap();
    store
}

/// A validated storage name.
pub fn name(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

/// A sender id.
pub fn sender(s: &str) -> SourceId {
    SourceId::new(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

/// The sender most tests replicate from.
pub fn source() -> SourceId {
    sender("head.office.app#hub")
}

fn text() -> FieldType {
    FieldType::Text { max_chars: None }
}

fn field(field: &str, ty: FieldType) -> FieldDef {
    FieldDef {
        name: name(field),
        ty,
        nullable: true,
    }
}

fn plain_def(entity: &str) -> EntityDef {
    EntityDef {
        name: name(entity),
        kind: EntityKind::Document,
        fields: vec![
            field("title", text()),
            field("origin_node", text()),
            field("origin_id", text()),
            field("origin_version", FieldType::Int),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    }
}

/// The catalog the intake scenarios apply against; `ghost` exists only after the "upgrade".
pub fn catalog(with_ghost: bool) -> Catalog {
    let mut entities = vec![plain_def("note")];
    if with_ghost {
        entities.push(plain_def("ghost"));
    }
    Catalog::new(
        1,
        Collation::new("en").unwrap_or_else(|e| panic!("{e}")),
        entities,
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

/// The apply transaction options.
pub fn options() -> TxOptions {
    TxOptions::new(name("quarantine_apply"), Isolation::ReadCommitted)
}

/// A field map from parts.
pub fn fields(parts: &[(&str, Value)]) -> Fields {
    parts.iter().map(|(k, v)| (name(k), v.clone())).collect()
}

/// A document of `entity` at `version`.
pub fn document(entity: &str, id: RecordId, title: &str, version: i64) -> DocumentData {
    DocumentData {
        entity: name(entity),
        id,
        fields: fields(&[
            ("title", Value::Text(title.to_owned())),
            ("origin_node", Value::Null),
            ("origin_id", Value::Null),
            ("origin_version", Value::Int(version)),
        ]),
        children: BTreeMap::new(),
    }
}

/// A `note` document.
pub fn note(id: RecordId, title: &str, version: i64) -> DocumentData {
    document("note", id, title, version)
}

/// A `ghost` document: valid in shape, refused until the catalog declares the entity.
pub fn ghost(id: RecordId, title: &str, version: i64) -> DocumentData {
    document("ghost", id, title, version)
}

/// A record for `document` from `from` at `version`/`seq`, with `entries`.
pub fn record_from(
    from: &SourceId,
    document: DocumentData,
    version: u64,
    seq: u64,
    entries: Vec<DocumentData>,
) -> ReceivedRecord {
    ReceivedRecord {
        name: RecordName::new(&format!("{}:{}", document.entity.as_str(), document.id))
            .unwrap_or_else(|e| panic!("{e}")),
        version,
        seq,
        source: from.clone(),
        topic: None,
        conversation: None,
        meta_version: 1,
        document,
        entries,
    }
}

/// A record from the default sender.
pub fn record(
    document: DocumentData,
    version: u64,
    seq: u64,
    entries: Vec<DocumentData>,
) -> ReceivedRecord {
    record_from(&source(), document, version, seq, entries)
}

/// The held item the intake would build for `record`.
pub fn new_item(record: &ReceivedRecord, reason: &str, first_seen: i64) -> NewItem {
    NewItem::held(
        record.source.clone(),
        record.seq,
        RecordRef::of(record),
        reason,
        "refused for a test",
        first_seen,
    )
}

/// Inserts `record` as a held item and returns the stored item.
pub async fn put(
    store: &PgQuarantineStore,
    record: &ReceivedRecord,
    reason: &str,
    first_seen: i64,
) -> QuarantineItem {
    use spot_quarantine::QuarantineStore;
    store
        .insert(new_item(record, reason, first_seen), record.clone())
        .await
        .unwrap()
}

/// A shared handle, for the intake scenarios.
pub type Shared = Arc<PgQuarantineStore>;

/// Closes `item` the way the admin layer does: a token from the permission guard, then `close`.
pub async fn close(
    store: &PgQuarantineStore,
    item: &QuarantineItem,
    outcome: spot_quarantine::ClosureOutcome,
    by: &str,
    at: i64,
) -> Result<QuarantineItem, spot_quarantine::QuarantineError> {
    use spot_quarantine::{Access, ClosureOutcome, QuarantineStore};
    let access = match outcome {
        ClosureOutcome::Released => Access::Release,
        ClosureOutcome::Discarded => Access::Discard,
    };
    let authority = spot_quarantine::authorize_close(
        &spot_quarantine::AllowAll,
        &spot_permissions::Principal::system(),
        access,
        item,
        outcome,
    )?;
    store
        .close(
            item.id,
            spot_quarantine::Closure {
                outcome,
                by: by.to_owned(),
                at,
            },
            authority,
        )
        .await
}
