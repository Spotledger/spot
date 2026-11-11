//! `QueryExecutor` and `StorageBackend::read` over the query compiler (WP-STO-03.T2, ADR-053).
//!
//! Both paths compile with [`spot_pg_query::compile`] and the default schema map (the same
//! identifiers `prepare` creates), bind every parameter with the exact type the compiler named
//! (never text and a cast), run it and decode each column by the compiler's [`ColumnType`]
//! (`decode.rs`). A numeric that does not fit `rust_decimal` is `InvalidValue`, never rounded;
//! `timestamptz` comes back as UTC.
//!
//! # Inside a transaction
//!
//! [`query`] sends the statement with `query_typed`: parse, bind and execute travel in one
//! round trip and no server-side statement is left behind. Errors go through the transaction's
//! `fail` (a lock refused with `NOWAIT` is `LockNotAvailable`, a wait past the transaction's
//! `lock_timeout` is `LockTimeout`; both come from the STO-01 table), and a query the compiler
//! refuses is recorded with `fail_storage`.
//!
//! # Outside one
//!
//! [`read`] serves `Primary` only (`Replica` and `AnalyticsMirror` are `Unsupported` until they
//! exist) and refuses any row lock with `InvalidQuery`. It takes a connection from the pool and
//! runs the query in its own `READ COMMITTED READ ONLY` transaction with the default timeouts
//! of [`TxOptions`], then rolls back. If the caller drops the future halfway, the connection is
//! discarded instead of going back to the pool inside an open transaction.
//!
//! # Prepared statements
//!
//! `read` prepares through the connection's statement cache, keyed by SQL text and parameter
//! types, so a repeated query is parsed once per connection (the compiler's output is
//! deterministic and does not depend on any value). The cache holds at most
//! [`STATEMENT_CACHE_MAX`] statements per connection; when a new statement would exceed it the
//! whole cache is cleared first (the driver closes the evicted statements on the server).
//! The transactional path cannot reach the connection's cache from here (`PgTransaction` only
//! hands out the plain client), so it does not cache; see the task's open questions.

mod bind;
mod decode;
mod numeric;

use std::sync::Arc;

use deadpool_postgres::Object;
use spot_pg_query::{CompiledQuery, DefaultSchemaMap, compile};
use spot_storage_contract::{
    Access, Catalog, ErrorKind, Isolation, Name, Query, ReadSource, RowLock, Rows, StorageError,
    StorageResult, TxOptions, Wait,
};
use tokio_postgres::types::{ToSql, Type};

use crate::error::map_pg_error;
use crate::{PgBackend, PgTransaction};

use self::bind::{Bound, bind};
use self::decode::decode_row;

/// Most prepared statements cached per connection by [`read`].
pub(crate) const STATEMENT_CACHE_MAX: usize = 256;

/// Added to the statement timeout for the server's idle-in-transaction timeout (as in `tx.rs`).
const MARGIN_MS: u128 = 10_000;

/// Runs a query inside a transaction.
pub(crate) async fn query(tx: &mut PgTransaction, query: &Query) -> StorageResult<Rows> {
    let catalog = Arc::clone(tx.catalog_arc());
    let compiled = compile(query, &catalog, &DefaultSchemaMap).map_err(|e| tx.fail_storage(e))?;
    refuse_search(query).map_err(|e| tx.fail_storage(e))?;
    let bound = bind(&compiled.params);
    let args: Vec<(&(dyn ToSql + Sync), Type)> = bound
        .iter()
        .map(|b: &Bound| (&*b.value as &(dyn ToSql + Sync), b.ty.clone()))
        .collect();
    let client = tx.client()?;
    let result = client.query_typed(&compiled.sql, &args).await;
    let rows = result.map_err(|e| tx.fail(&e, lock_wait(query.lock)))?;
    decode_all(&compiled, &rows).map_err(|e| tx.fail_storage(e))
}

/// Runs a read-only query outside a transaction.
pub(crate) async fn read(
    backend: &PgBackend,
    catalog: Arc<Catalog>,
    source: ReadSource,
    query: &Query,
) -> StorageResult<Rows> {
    match source {
        ReadSource::Primary => {}
        ReadSource::Replica | ReadSource::AnalyticsMirror => {
            return Err(StorageError::new(
                ErrorKind::Unsupported,
                format!("reads from {source:?} are not available in spot-postgres yet"),
            ));
        }
    }
    if query.lock != RowLock::None {
        return Err(StorageError::new(
            ErrorKind::InvalidQuery,
            "a read outside a transaction cannot lock rows",
        ));
    }
    let compiled = compile(query, &catalog, &DefaultSchemaMap)?;
    refuse_search(query)?;
    let bound = bind(&compiled.params);
    let mut conn = Pooled::new(backend.checkout().await?);
    let outcome = run_read(backend, &mut conn, &compiled, &bound).await;
    conn.finish().await;
    outcome
}

/// Search and search rank read the projection table, which WP-STO-13.T1 builds; until then they
/// answer `Unsupported`, as `Capabilities::search` says. Validation (`InvalidQuery`) has already
/// run in the compiler, so a malformed term is still `InvalidQuery` (ADR-087 section 7.9).
fn refuse_search(query: &Query) -> StorageResult<()> {
    if query.constructs().search {
        return Err(StorageError::new(
            ErrorKind::Unsupported,
            "search is not supported by this backend yet: the search projection arrives with \
             WP-STO-13.T1 (ADR-087)",
        ));
    }
    Ok(())
}

/// A pooled connection that goes back to the pool only after its transaction ended cleanly.
struct Pooled {
    conn: Option<Object>,
}

impl Pooled {
    fn new(conn: Object) -> Self {
        Self { conn: Some(conn) }
    }

    fn object(&self) -> StorageResult<&Object> {
        self.conn
            .as_ref()
            .ok_or_else(|| StorageError::new(ErrorKind::Internal, "the connection is gone"))
    }

    /// Rolls the read-only transaction back. Only a confirmed rollback releases the connection
    /// to the pool; otherwise `Drop` discards it.
    async fn finish(&mut self) {
        if let Some(conn) = self.conn.as_ref()
            && conn.batch_execute("ROLLBACK").await.is_ok()
        {
            self.conn.take();
        }
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        // Reached with a connection still held only when the future was dropped before
        // `finish`: the transaction may be open, so the connection must not be recycled.
        if let Some(conn) = self.conn.take() {
            drop(Object::take(conn));
        }
    }
}

async fn run_read(
    backend: &PgBackend,
    pooled: &mut Pooled,
    compiled: &CompiledQuery,
    bound: &[Bound],
) -> StorageResult<Rows> {
    let defaults = TxOptions {
        isolation: Isolation::ReadCommitted,
        access: Access::ReadOnly,
        ..TxOptions::new(label()?, Isolation::ReadCommitted)
    };
    let conn = pooled.object()?;
    conn.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY")
        .await
        .map_err(|e| map_pg_error(&e, None))?;
    let statement_ms = defaults.statement_timeout.as_millis();
    let app = {
        let mut name = format!("{}:read", backend.config().application_name);
        let mut end = name.len().min(63);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        name.truncate(end);
        name
    };
    conn.execute(
        "SELECT set_config('lock_timeout', $1, true), \
                set_config('statement_timeout', $2, true), \
                set_config('idle_in_transaction_session_timeout', $3, true), \
                set_config('application_name', $4, true)",
        &[
            &format!("{}ms", defaults.lock_timeout.as_millis()),
            &format!("{statement_ms}ms"),
            &format!("{}ms", statement_ms + MARGIN_MS),
            &app,
        ],
    )
    .await
    .map_err(|e| map_pg_error(&e, None))?;

    if conn.statement_cache.size() >= STATEMENT_CACHE_MAX {
        conn.statement_cache.clear();
    }
    let types: Vec<Type> = bound.iter().map(|b| b.ty.clone()).collect();
    let statement = conn
        .prepare_typed_cached(&compiled.sql, &types)
        .await
        .map_err(|e| map_pg_error(&e, None))?;
    let args: Vec<&(dyn ToSql + Sync)> = bound.iter().map(|b| &*b.value as _).collect();
    let rows = conn
        .query(&statement, &args)
        .await
        .map_err(|e| map_pg_error(&e, None))?;
    decode_all(compiled, &rows)
}

fn label() -> StorageResult<Name> {
    Name::new("read")
}

fn decode_all(compiled: &CompiledQuery, rows: &[tokio_postgres::Row]) -> StorageResult<Rows> {
    let rows = rows
        .iter()
        .map(|r| decode_row(r, &compiled.decode))
        .collect::<StorageResult<Vec<_>>>()?;
    Ok(Rows {
        columns: compiled.columns.clone(),
        rows,
    })
}

/// The wait mode of a row lock, which tells `NOWAIT` from a lock timeout in the error table.
fn lock_wait(lock: RowLock) -> Option<Wait> {
    match lock {
        RowLock::None => None,
        RowLock::Share(w) | RowLock::Update(w) => Some(w),
    }
}
