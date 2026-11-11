//! [`PgAuditStore`]: the [`AuditStore`] over PostgreSQL.
//!
//! Append is a compare-and-set on the head in one statement ([`crate::sql::APPEND_SQL`]), so two
//! writers can never fork the chain: the loser's `INSERT … SELECT … WHERE` inserts no row and
//! the call reports [`AppendOutcome::HeadMoved`]. An entry that does not extend a head that
//! *has* not moved is a store error, never a silent skip.
//!
//! A record and a checkpoint are stored as JSON. The entry hash is recomputed from the decoded
//! record (`canonical_bytes()`), so editing the stored JSON — or any stored hash — changes the
//! recomputed hash and the verifier reports it; storing the canonical bytes as well would add
//! nothing, because the canonical encoding has no decoder.

use deadpool_postgres::{Pool, PoolError};
use spot_audit_chain::{
    AppendOutcome, AuditError, AuditRecord, AuditStore, ChainEntry, Checkpoint, StoreFuture,
    genesis_hash,
};
use tokio_postgres::Row;

use crate::error::{Error, Result, corrupt, db, pool};
use crate::sql;

/// The audit chain in PostgreSQL.
pub struct PgAuditStore {
    pool: Pool,
}

impl PgAuditStore {
    /// A store over `pool`.
    #[must_use]
    pub fn new(pool: Pool) -> PgAuditStore {
        PgAuditStore { pool }
    }

    /// The pool the store was built on.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Creates the tables, the `spot_audit_app` role and the guard trigger. Idempotent.
    ///
    /// # Errors
    ///
    /// [`crate::UNAVAILABLE`] when the database or its pool fails.
    pub async fn prepare(&self) -> Result<()> {
        prepare(&self.pool).await
    }

    async fn client(&self, what: &str) -> Result<deadpool_postgres::Object> {
        self.pool.get().await.map_err(|e| pool(what, &e))
    }
}

/// Creates the store's schema, role and trigger on `pool`. A free function so a host can prepare
/// before building the store.
///
/// # Errors
///
/// [`crate::UNAVAILABLE`] when the database or its pool fails.
pub async fn prepare(pool: &Pool) -> Result<()> {
    sql::prepare(pool).await
}

impl AuditStore for PgAuditStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async move {
            let client = self.client("head").await.map_err(to_audit)?;
            let row = client
                .query_opt(sql::HEAD_SQL, &[&site])
                .await
                .map_err(|e| to_audit(db("head", &e)))?;
            row.as_ref().map(entry_of).transpose()
        })
    }

    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        Box::pin(async move {
            let client = self.client("append").await.map_err(to_audit)?;
            let seq = i64::try_from(entry.seq).map_err(|_| {
                to_audit(Error::unavailable(
                    "the entry's sequence number does not fit bigint",
                ))
            })?;
            let expected = i64::try_from(expected_seq).map_err(|_| {
                to_audit(Error::unavailable("the expected head does not fit bigint"))
            })?;
            let record = serde_json::to_string(&entry.record)
                .map_err(|_| to_audit(Error::unavailable("the record could not be encoded")))?;
            let row = client
                .query_opt(
                    sql::APPEND_SQL,
                    &[
                        &site,
                        &seq,
                        &entry.prev_hash.to_vec(),
                        &entry.hash.to_vec(),
                        &record,
                        &entry.record.at,
                        &expected,
                        &sql::genesis(site),
                    ],
                )
                .await
                .map_err(|e| to_audit(db("append", &e)))?;
            if row.is_some() {
                return Ok(AppendOutcome::Appended);
            }
            // No row: either the head moved under us, or the entry never linked to it.
            let head = self.head(site).await?;
            let head_seq = head.as_ref().map_or(0, |h| h.seq);
            if head_seq != expected_seq {
                return Ok(AppendOutcome::HeadMoved);
            }
            let head_hash = head.as_ref().map_or_else(|| genesis_hash(site), |h| h.hash);
            if entry.seq != expected_seq.saturating_add(1) || entry.prev_hash != head_hash {
                return Err(AuditError::Store(
                    "the entry does not extend the head".to_owned(),
                ));
            }
            Ok(AppendOutcome::HeadMoved)
        })
    }

    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async move {
            let client = self.client("range").await.map_err(to_audit)?;
            let from = i64::try_from(from).unwrap_or(i64::MIN);
            let to = i64::try_from(to).unwrap_or(i64::MAX);
            let rows = client
                .query(sql::RANGE_SQL, &[&site, &from, &to])
                .await
                .map_err(|e| to_audit(db("range", &e)))?;
            rows.iter().map(entry_of).collect()
        })
    }

    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let client = self.client("put_checkpoint").await.map_err(to_audit)?;
            let seq = i64::try_from(checkpoint.seq).map_err(|_| {
                to_audit(Error::unavailable(
                    "the checkpoint's seq does not fit bigint",
                ))
            })?;
            client
                .execute(
                    sql::CHECKPOINT_PUT_SQL,
                    &[
                        &checkpoint.site,
                        &seq,
                        &checkpoint.head_hash.to_vec(),
                        &checkpoint.at,
                        &checkpoint.key_id,
                        &checkpoint.signature,
                    ],
                )
                .await
                .map_err(|e| to_audit(db("put_checkpoint", &e)))?;
            Ok(())
        })
    }

    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        Box::pin(async move {
            let client = self.client("latest_checkpoint").await.map_err(to_audit)?;
            let row = client
                .query_opt(sql::CHECKPOINT_GET_SQL, &[&site])
                .await
                .map_err(|e| to_audit(db("latest_checkpoint", &e)))?;
            row.as_ref().map(checkpoint_of).transpose()
        })
    }
}

/// One stored chain row as an entry.
fn entry_of(row: &Row) -> std::result::Result<ChainEntry, AuditError> {
    let seq = u64::try_from(row.get::<_, i64>(0))
        .map_err(|_| AuditError::Store(corrupt("a seq is negative").message))?;
    let prev_hash = hash(row.get(1))?;
    let hash = hash(row.get(2))?;
    let record: AuditRecord = serde_json::from_str(&row.get::<_, String>(3))
        .map_err(|_| AuditError::Store(corrupt("a record does not decode").message))?;
    Ok(ChainEntry {
        seq,
        prev_hash,
        record,
        hash,
    })
}

/// One stored checkpoint row as a checkpoint.
fn checkpoint_of(row: &Row) -> std::result::Result<Checkpoint, AuditError> {
    Ok(Checkpoint {
        site: row.get(0),
        seq: u64::try_from(row.get::<_, i64>(1))
            .map_err(|_| AuditError::Store(corrupt("a checkpoint seq is negative").message))?,
        head_hash: hash(row.get(2))?,
        at: row.get(3),
        key_id: row.get(4),
        signature: row.get(5),
    })
}

fn hash(bytes: Vec<u8>) -> std::result::Result<[u8; 32], AuditError> {
    bytes
        .try_into()
        .map_err(|_| AuditError::Store(corrupt("a hash is not 32 bytes").message))
}

/// A store error, as the chain error the trait returns.
fn to_audit(error: Error) -> AuditError {
    AuditError::Store(error.message)
}

/// A pool error, for callers that want the crate's error type.
impl From<PoolError> for Error {
    fn from(error: PoolError) -> Error {
        Error::unavailable(error.to_string())
    }
}
