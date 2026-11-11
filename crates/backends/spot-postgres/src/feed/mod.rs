//! `Outbox` and `ChangeFeed` (ADR-11, ADR-053 "Outbox and change feed", ADR-056).
//!
//! # Capture
//!
//! Business transactions only *capture*; they take no shared counter and no lock for the logs.
//!
//! - **Outbox.** [`enqueue`] inserts into the insert-only `spot_outbox` inside the transaction,
//!   with the transaction's id (a UUID the first capture of the transaction makes and keeps in a
//!   transaction-local setting), the next ordinal of that transaction, the capture time and the
//!   backend's transaction id (`xid8`). Each event counts against `max_writes`.
//! - **Change feed.** [`install`], called by `prepare`, puts a trigger on every document table.
//!   It keeps one *net* row per document per transaction in `spot_change`: insert then update is
//!   one insert at the final version; insert then delete leaves nothing; update then delete is a
//!   delete; delete then insert of the same id is an update. Child rows never need a trigger:
//!   every document write updates the parent row and its version.
//!
//! Rows written in a rolled-back transaction or savepoint vanish with it (guarantee 1).
//!
//! # Publication
//!
//! `read_outbox` and `read_changes` first *publish*, then read; see [`publish`]. One publisher at
//! a time per log (a transaction-level advisory lock held only by the publisher) appends the
//! pending committed entries to a side table of positions `(generation, seq)`. Pending means
//! "visible and not yet in that table", never "id greater than the last seen id", which loses
//! entries that commit late.
//!
//! # Recovery and retention
//!
//! The stream generation is `spot_meta.feed_generation`. [`start_new_generation`] is the admin
//! function a restore calls: old bookmarks then fail with `ResyncRequired`. Retention is a
//! setting ([`set_retention`], default 7 days) and [`cleanup`] is the function the job engine
//! calls (WP-PLT-06); it raises each log's floor, and a bookmark below the floor, from another
//! generation or beyond the head fails with `ResyncRequired`.

mod admin;
mod envelope;
mod publish;
mod registry_gate;
mod sql;

use spot_storage_contract::{
    Catalog, Change, ChangeOp, EntityKind, ErrorKind, FeedPosition, Name, NodeRef, OutboxEvent,
    OutboxRecord, RecordId, StorageError, StorageResult, Version,
};
use tokio_postgres::Row;
use tokio_postgres::Transaction;
use uuid::Uuid;

use crate::schema::names::{META_TABLE, quote};
use crate::{PgBackend, PgTransaction};

pub use self::admin::{Cleanup, cleanup, retention, set_retention, start_new_generation};
pub use self::envelope::envelopes;

/// Names `install` creates in the database; an entity cannot use them.
const RESERVED: &[&str] = &[
    "spot_outbox",
    "spot_outbox_pub",
    "spot_change",
    "spot_change_pub",
    "spot_feed_state",
];

fn db(e: tokio_postgres::Error) -> StorageError {
    crate::error::map_pg_error(&e, None)
}

fn internal(what: impl Into<String>) -> StorageError {
    StorageError::new(ErrorKind::Internal, what)
}

/// Creates the feed's tables and functions (once per schema version) and the capture trigger on
/// every document table that lacks one. Called by `prepare` in its transaction, after the entity
/// tables exist; running it again with nothing to do sends no DDL.
pub(crate) async fn install(tx: &Transaction<'_>, catalog: &Catalog) -> StorageResult<()> {
    for name in RESERVED {
        if Name::new(name).is_ok_and(|n| catalog.entity(&n).is_ok()) {
            return Err(StorageError::new(
                ErrorKind::InvalidSchema,
                format!(
                    "`{name}` is reserved for the outbox and change feed and cannot be an entity"
                ),
            ));
        }
    }
    let meta = quote(META_TABLE);
    let stored: Option<String> = tx
        .query_opt(
            &format!("SELECT value FROM {meta} WHERE key = $1"),
            &[&sql::META_SCHEMA],
        )
        .await
        .map_err(db)?
        .map(|r| r.get(0));
    if stored.as_deref() != Some(sql::SCHEMA_VERSION) {
        tx.batch_execute(sql::SCHEMA).await.map_err(db)?;
        tx.batch_execute(sql::INITIAL_STATE).await.map_err(db)?;
        let upsert = format!(
            "INSERT INTO {meta} (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
        );
        // The generation starts at 1: `FeedPosition::START` (generation 0) means "the current one".
        tx.execute(&upsert, &[&sql::META_SCHEMA, &sql::SCHEMA_VERSION])
            .await
            .map_err(db)?;
        tx.execute(
            &format!(
                "INSERT INTO {meta} (key, value) VALUES ($1, '1') ON CONFLICT (key) DO NOTHING"
            ),
            &[&sql::META_GENERATION],
        )
        .await
        .map_err(db)?;
    }
    let have: std::collections::BTreeSet<String> = tx
        .query(sql::TRIGGERED_TABLES, &[])
        .await
        .map_err(db)?
        .iter()
        .map(|r| r.get(0))
        .collect();
    for entity in catalog.entities() {
        if matches!(entity.kind, EntityKind::Document) && !have.contains(entity.name.as_str()) {
            tx.batch_execute(&sql::capture_trigger(&quote(entity.name.as_str())))
                .await
                .map_err(db)?;
        }
    }
    Ok(())
}

/// Captures an outbox event in the transaction.
pub(crate) async fn enqueue(tx: &mut PgTransaction, event: &OutboxEvent) -> StorageResult<()> {
    // Before any SQL: a refused event writes nothing and fails the caller's transaction.
    if let Err(refusal) = registry_gate::gate(event.topic.as_str()) {
        return Err(tx.fail_storage(StorageError::new(
            ErrorKind::InvalidValue,
            refusal.message(),
        )));
    }
    tx.count_writes(1)?;
    let subject_version = match event.subject_version {
        None => None,
        Some(v) => Some(i64::try_from(v.0).map_err(|_| {
            tx.fail_storage(StorageError::new(
                ErrorKind::InvalidValue,
                format!("subject version {} does not fit the database", v.0),
            ))
        })?),
    };
    let schema_version = i32::try_from(event.schema_version).map_err(|_| {
        tx.fail_storage(StorageError::new(
            ErrorKind::InvalidValue,
            format!(
                "schema version {} does not fit the database",
                event.schema_version
            ),
        ))
    })?;
    let id = Uuid::from_bytes(*event.id.as_bytes());
    let subject_id = Uuid::from_bytes(*event.subject.id.as_bytes());
    let topic = event.topic.as_str();
    let entity = event.subject.entity.as_str();
    let client = tx.client()?;
    let result = client
        .execute(
            sql::ENQUEUE,
            &[
                &id,
                &topic,
                &entity,
                &subject_id,
                &subject_version,
                &schema_version,
                &event.payload,
            ],
        )
        .await;
    match result {
        Ok(0) => Err(tx.fail_storage(StorageError::new(
            ErrorKind::AlreadyExists,
            format!("an outbox event with id {} already exists", event.id),
        ))),
        Ok(_) => Ok(()),
        Err(e) => Err(tx.fail(&e, None)),
    }
}

/// Reads published outbox entries after `after`.
pub(crate) async fn read_outbox(
    backend: &PgBackend,
    after: FeedPosition,
    limit: u32,
) -> StorageResult<Vec<OutboxRecord>> {
    let (generation, rows) =
        publish::publish_and_read(backend, publish::Log::Outbox, after, limit).await?;
    rows.into_iter()
        .map(|row| outbox_record(generation, row))
        .collect()
}

/// Reads published change entries after `after`.
pub(crate) async fn read_changes(
    backend: &PgBackend,
    after: FeedPosition,
    limit: u32,
) -> StorageResult<Vec<Change>> {
    let (generation, rows) =
        publish::publish_and_read(backend, publish::Log::Changes, after, limit).await?;
    rows.into_iter()
        .map(|row| change(generation, row))
        .collect()
}

/// A position from the columns the read statements return.
fn position(generation: i64, seq: i64) -> StorageResult<FeedPosition> {
    match (u64::try_from(generation), u64::try_from(seq)) {
        (Ok(g), Ok(s)) => Ok(FeedPosition::new(g, s)),
        _ => Err(internal("the feed holds a negative position")),
    }
}

fn record_id(u: Uuid) -> StorageResult<RecordId> {
    RecordId::from_bytes(*u.as_bytes())
}

fn version_of(v: i64) -> StorageResult<Version> {
    u64::try_from(v)
        .map(Version)
        .map_err(|_| internal("the feed holds a negative version"))
}

fn outbox_record(generation: i64, row: Row) -> StorageResult<OutboxRecord> {
    let ordinal: i32 = row.get(3);
    let schema_version: i32 = row.get(8);
    let subject_version: Option<i64> = row.get(7);
    Ok(OutboxRecord {
        position: position(generation, row.get(0))?,
        transaction: record_id(row.get(2))?,
        ordinal: u32::try_from(ordinal).map_err(|_| internal("negative outbox ordinal"))?,
        event: OutboxEvent {
            id: record_id(row.get(1))?,
            topic: Name::new(row.get::<_, &str>(4))?,
            subject: NodeRef {
                entity: Name::new(row.get::<_, &str>(5))?,
                id: record_id(row.get(6))?,
            },
            subject_version: subject_version.map(version_of).transpose()?,
            schema_version: u32::try_from(schema_version)
                .map_err(|_| internal("negative schema version"))?,
            payload: row.get(9),
        },
    })
}

fn change(generation: i64, row: Row) -> StorageResult<Change> {
    let op = match row.get::<_, &str>(4) {
        "I" => ChangeOp::Insert,
        "U" => ChangeOp::Update,
        "D" => ChangeOp::Delete,
        other => return Err(internal(format!("unknown change operation `{other}`"))),
    };
    Ok(Change {
        position: position(generation, row.get(0))?,
        transaction: record_id(row.get(1))?,
        document: NodeRef {
            entity: Name::new(row.get::<_, &str>(2))?,
            id: record_id(row.get(3))?,
        },
        op,
        version: version_of(row.get(5))?,
    })
}
