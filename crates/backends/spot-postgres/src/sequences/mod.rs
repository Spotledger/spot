//! `Sequences` for [`PgTransaction`]: gapless numbering series (WP-STO-04.T1, ADR-46).
//!
//! # Storage
//!
//! One row per series in `spot_sequence (series text primary key, next_value bigint)`, holding
//! the next number to hand out. PostgreSQL `SEQUENCE` objects are not used: they are not
//! transactional and leave gaps on rollback.
//!
//! The table belongs to `prepare` in principle; until `prepare` creates it, the first
//! allocation in a database does (see [`ensure_table`]). Numbers are `u64` in the contract and
//! `bigint` here, so a series cannot go past `i64::MAX` (`InvalidValue`, as for any
//! out-of-range value).
//!
//! # Allocation
//!
//! `INSERT … ON CONFLICT (series) DO UPDATE SET next_value = next_value + count RETURNING …`, in
//! the caller's own transaction. The update takes the series row's lock, so allocations
//! **serialize per series**: a second transaction waits until the first commits or rolls back
//! (bounded by its `lock_timeout`), different series are different rows and never wait for one
//! another. A rollback undoes the update, so the numbers return to the series: committed numbers
//! are exactly `1..=n` with no gaps and no duplicates.
//!
//! # Isolation levels
//!
//! - `READ COMMITTED`: a transaction that waited for the row lock re-reads the newest committed
//!   row version before updating, so it hands out the next number after the previous holder's
//!   (or the same number again if the previous holder rolled back). Correct and the usual case.
//! - `REPEATABLE READ` and `SERIALIZABLE`: if the row was changed by a transaction that committed
//!   after this one's snapshot, PostgreSQL does not overwrite it but fails with `40001`
//!   (`SerializationFailure`, retryable), whether the row existed or was inserted concurrently.
//!   Nothing is handed out in that case, and the failed transaction rolls back, so uniqueness
//!   and gaplessness hold; the caller's retry policy (ADR-46) runs the transaction again and then
//!   sees the new row. A transaction that waited and then won sees the same error only if the
//!   other committed, which is exactly the case in which its snapshot is stale.
//!
//! In every level two transactions never receive the same number: the second update either
//! sees the first's committed value or fails.
//!
//! # Setting
//!
//! `set_next` moves the next number forward (or keeps it), never back: a number already handed
//! out, whether committed or by the caller's own earlier allocation, cannot be reused
//! (`InvalidValue`). Like allocation, it takes the series row's lock until the transaction ends.

use spot_storage_contract::{ErrorKind, SeriesKey, StorageError, StorageResult};

use crate::PgTransaction;

/// The table's name.
const TABLE: &str = "spot_sequence";

/// The advisory lock (two-integer key space, `SPOT`/`SEQC`) that serializes creating the table.
const CREATE_LOCK: (i32, i32) = (0x5350_4f54, 0x5345_5143);

/// Allocates `count` consecutive numbers and returns the first.
pub(crate) async fn allocate(
    tx: &mut PgTransaction,
    series: &SeriesKey,
    count: u32,
) -> StorageResult<u64> {
    if count == 0 {
        return Err(tx.fail_storage(StorageError::new(
            ErrorKind::InvalidValue,
            "a series cannot allocate zero numbers",
        )));
    }
    ensure_table(tx).await?;
    let count = i64::from(count);
    // First use inserts `1 + count`; later uses add `count`. `RETURNING` is the first number.
    let sql = format!(
        "INSERT INTO \"{TABLE}\" (series, next_value) VALUES ($1, 1 + $2::bigint) \
         ON CONFLICT (series) DO UPDATE SET next_value = \"{TABLE}\".next_value + $2::bigint \
         RETURNING next_value - $2::bigint"
    );
    let client = tx.client()?;
    let result = client.query_one(&sql, &[&series.as_str(), &count]).await;
    let row = result.map_err(|e| tx.fail(&e, None))?;
    first_number(tx, row.get::<_, i64>(0))
}

/// Sets the next number a series hands out.
pub(crate) async fn set_next(
    tx: &mut PgTransaction,
    series: &SeriesKey,
    next: u64,
) -> StorageResult<()> {
    let Some(next) = i64::try_from(next).ok().filter(|n| *n >= 1) else {
        return Err(tx.fail_storage(StorageError::new(
            ErrorKind::InvalidValue,
            "the next number of a series must be between 1 and 9223372036854775807",
        )));
    };
    ensure_table(tx).await?;
    // The conflict branch only applies when it moves forward; no row back means it would not.
    let sql = format!(
        "INSERT INTO \"{TABLE}\" (series, next_value) VALUES ($1, $2) \
         ON CONFLICT (series) DO UPDATE SET next_value = EXCLUDED.next_value \
         WHERE \"{TABLE}\".next_value <= EXCLUDED.next_value \
         RETURNING next_value"
    );
    let client = tx.client()?;
    let result = client.query_opt(&sql, &[&series.as_str(), &next]).await;
    let row = result.map_err(|e| tx.fail(&e, None))?;
    match row {
        Some(_) => Ok(()),
        None => Err(tx.fail_storage(StorageError::new(
            ErrorKind::InvalidValue,
            format!(
                "series `{series}` has already handed out numbers up to or past {next}; \
                 a number cannot be reused",
                series = series.as_str()
            ),
        ))),
    }
}

fn first_number(tx: &mut PgTransaction, value: i64) -> StorageResult<u64> {
    u64::try_from(value).map_err(|_| {
        tx.fail_storage(StorageError::new(
            ErrorKind::Internal,
            "the sequence table holds a number below 1",
        ))
    })
}

/// Creates `spot_sequence` if the database has none yet, inside the caller's transaction.
///
/// An existing table costs one catalog lookup (`to_regclass`). Without it, the transaction takes
/// a transaction-level advisory lock, looks again and then creates the table: a concurrent first
/// use waits on that lock (bounded by its `lock_timeout`) until this transaction ends, and then
/// finds the table committed, so the two `CREATE`s never race. If this transaction rolls back the
/// table goes with it and the next first use creates it again. A read-only transaction cannot
/// create it and fails with `ReadOnlyTransaction`, which is also what its allocation would do.
async fn ensure_table(tx: &mut PgTransaction) -> StorageResult<()> {
    if table_exists(tx).await? {
        return Ok(());
    }
    let client = tx.client()?;
    let result = client
        .execute(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&CREATE_LOCK.0, &CREATE_LOCK.1],
        )
        .await;
    result.map_err(|e| tx.fail(&e, None))?;
    if table_exists(tx).await? {
        return Ok(());
    }
    let sql = format!(
        "CREATE TABLE IF NOT EXISTS \"{TABLE}\" (\
         series text COLLATE \"C\" PRIMARY KEY, \
         next_value bigint NOT NULL CHECK (next_value >= 1))"
    );
    let client = tx.client()?;
    let result = client.batch_execute(&sql).await;
    result.map_err(|e| tx.fail(&e, None))
}

async fn table_exists(tx: &mut PgTransaction) -> StorageResult<bool> {
    let client = tx.client()?;
    let result = client
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&TABLE])
        .await;
    let row = result.map_err(|e| tx.fail(&e, None))?;
    Ok(row.get(0))
}
