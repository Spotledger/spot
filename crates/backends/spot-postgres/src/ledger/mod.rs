//! The command ledger (storage contract 0.3.0, WP-DOC-04.T8): the table `spot_command_ledger`
//! and the three transaction operations that read, write and prune it.
//!
//! A record is written in the caller's own transaction, so it commits and rolls back with the
//! document change it answers for. The table is created by `prepare` with the other `spot_`
//! tables, once per tenant database (the site is implicit: a command id never crosses sites). The
//! backend never reads the clock: `recorded_at`, `retain_until` and the prune cut-off are
//! arguments.

mod sql;

use spot_storage_contract::{
    CommandRecord, ErrorKind, Name, StorageError, StorageResult, Transaction,
};
use time::OffsetDateTime;
use tokio_postgres::Transaction as PgTx;

use crate::PgTransaction;
use crate::error::map_pg_error;
use crate::schema::names::{META_TABLE, quote};

/// The name `install` creates; an entity cannot use it.
const RESERVED: &str = "spot_command_ledger";

fn db(e: tokio_postgres::Error) -> StorageError {
    map_pg_error(&e, None)
}

/// Creates the table and its index when the stored schema version differs. Idempotent, additive.
pub(crate) async fn install(
    tx: &PgTx<'_>,
    catalog: &spot_storage_contract::Catalog,
) -> StorageResult<()> {
    if Name::new(RESERVED).is_ok_and(|n| catalog.entity(&n).is_ok()) {
        return Err(StorageError::new(
            ErrorKind::InvalidSchema,
            format!("`{RESERVED}` is reserved for the command ledger and cannot be an entity"),
        ));
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
    if stored.as_deref() == Some(sql::SCHEMA_VERSION) {
        return Ok(());
    }
    tx.batch_execute(sql::SCHEMA).await.map_err(db)?;
    tx.execute(
        &format!(
            "INSERT INTO {meta} (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
        ),
        &[&sql::META_SCHEMA, &sql::SCHEMA_VERSION],
    )
    .await
    .map_err(db)?;
    Ok(())
}

/// Reads the record of `(principal, command_id)` on the transaction's own connection, so a
/// record this transaction inserted is seen too. A read: it counts no write and, because it
/// takes `&Client`, it records no state change on the transaction (the engine rolls back on any
/// error).
pub(crate) async fn get(
    client: &deadpool_postgres::Object,
    principal: &str,
    command_id: &str,
) -> StorageResult<Option<CommandRecord>> {
    let statement = client.prepare_cached(sql::GET).await.map_err(db)?;
    let row = client
        .query_opt(&statement, &[&principal, &command_id])
        .await
        .map_err(db)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let fingerprint: Vec<u8> = row.get(0);
    let outcome: String = row.get(1);
    let recorded_at: OffsetDateTime = row.get(2);
    let retain_until: OffsetDateTime = row.get(3);
    CommandRecord::new(
        principal,
        command_id,
        fingerprint,
        outcome,
        recorded_at,
        retain_until,
    )
    .map(Some)
}

/// Inserts one record. A key that exists already (committed, or inserted by a concurrent
/// transaction that commits first) fails with `UniqueViolation`.
pub(crate) async fn put(tx: &mut PgTransaction, record: &CommandRecord) -> StorageResult<()> {
    if tx.options().access == spot_storage_contract::Access::ReadOnly {
        return Err(tx.fail_storage(StorageError::new(
            ErrorKind::ReadOnlyTransaction,
            "a command record was written in a read-only transaction",
        )));
    }
    if let Err(e) = record.validate() {
        return Err(tx.fail_storage(e));
    }
    tx.count_writes(1)?;
    let statement = tx.cached(sql::PUT).await?;
    let client = tx.client()?;
    let result = client
        .execute(
            &statement,
            &[
                &record.principal,
                &record.command_id,
                &record.fingerprint,
                &record.outcome,
                &record.recorded_at,
                &record.retain_until,
            ],
        )
        .await;
    result.map(|_| ()).map_err(|e| tx.fail(&e, None))
}

/// Deletes every record whose `retain_until` is at or before `before`.
pub(crate) async fn prune(tx: &mut PgTransaction, before: OffsetDateTime) -> StorageResult<u64> {
    if tx.options().access == spot_storage_contract::Access::ReadOnly {
        return Err(tx.fail_storage(StorageError::new(
            ErrorKind::ReadOnlyTransaction,
            "a command prune was attempted in a read-only transaction",
        )));
    }
    tx.count_writes(1)?;
    let client = tx.client()?;
    let result = client.execute(sql::PRUNE, &[&before]).await;
    result.map_err(|e| tx.fail(&e, None))
}
