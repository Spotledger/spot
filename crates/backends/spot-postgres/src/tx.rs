//! [`PgTransaction`]: one open PostgreSQL transaction on a pooled connection.
//!
//! # Lifecycle
//!
//! `begin` takes a connection and sends `BEGIN ISOLATION LEVEL <declared> <READ WRITE|READ ONLY>`
//! followed by one `set_config(..., true)` statement (the transaction-local form of
//! `SET LOCAL`, which takes bound parameters) for `lock_timeout`, `statement_timeout`,
//! `idle_in_transaction_session_timeout` and `application_name`.
//!
//! `idle_in_transaction_session_timeout` is the statement timeout plus a 10 s margin: an
//! operation may run for the statement timeout, and the application gets a further margin to
//! issue the next one. A transaction idle for longer is killed by the server instead of holding
//! locks for ever; its next operation then fails with `Unavailable`. A statement timeout of zero
//! (none) means no idle timeout either.
//!
//! # Errors and poisoning
//!
//! Every database error is recorded through [`PgTransaction::fail`]. The contract allows a
//! transaction to continue after an error only for some kinds and only when the failed operation
//! ran inside a savepoint that was rolled back, so:
//!
//! - an error of a *recoverable* kind (`UniqueViolation`, `InvalidValue`, `LockNotAvailable`, ...)
//!   puts the transaction in the **aborted** state, which only `rollback_to` an earlier savepoint
//!   leaves;
//! - any other kind **poisons** it for good (even a savepoint cannot revive it).
//!
//! In both states every operation, including `savepoint`, `release` and `commit`, fails at once
//! with the original kind and sends **no SQL**, because PostgreSQL would answer "current
//! transaction is aborted" (and `COMMIT` of an aborted transaction silently rolls back, which
//! must never look like success).
//!
//! # Commit outcome
//!
//! `COMMIT` succeeding is `Ok`. A server answer of severity `ERROR` is a definite failure
//! (the transaction was rolled back) and maps through the SQLSTATE table, so a deferred
//! constraint or a serialization failure at commit keeps its kind. Once `COMMIT` is attempted on
//! a connection that was still open, **any** connection-level failure (reset, `FATAL` response,
//! no answer within the statement timeout plus margin) is `CommitOutcomeUnknown`, never
//! `Unavailable`: the server may have committed before the connection died. Only a connection
//! already known to be closed *before* `COMMIT` is sent is `Unavailable`, because nothing was
//! sent.
//!
//! # Drop
//!
//! `Drop` cannot await, so a transaction that is still open **discards its connection**
//! (`Object::take`, then the client is dropped and the server rolls the transaction back)
//! instead of returning it to the pool. The alternative, a cleanup task that sends `ROLLBACK`
//! before recycling, needs a runtime at drop time and leaves a window in which the pool could
//! hand out the connection; discarding costs one reconnect but cannot leak an open transaction.
//! The connection returns to the pool only after `COMMIT` or `ROLLBACK` was confirmed.
//!
//! # The raw-SQL seam
//!
//! Sibling `*-pg` crates own tables of their own (created by `spot-pg-migrate` or by the
//! crate's idempotent `prepare`) and must write them in the caller's transaction, so that their
//! rows and the document engine's rows commit or roll back together. [`PgTransaction`] offers
//! [`raw_query`](PgTransaction::raw_query), [`raw_query_opt`](PgTransaction::raw_query_opt),
//! [`raw_execute`](PgTransaction::raw_execute) and
//! [`raw_execute_with`](PgTransaction::raw_execute_with) for that.
//!
//! They run on the transaction's **own** connection, through the same `client()` path as the
//! area modules, so they never check out a second connection; they map failures through the
//! same SQLSTATE table, so poisoning and savepoint revival work exactly as for the area modules;
//! and they count writes: the caller states what its statement does with a [`WriteKind`]
//! (`Read`, or `Write(n)`), because the crate never parses SQL. A `Write` in a read-only
//! transaction is refused before any SQL is sent.
//!
//! Values reach the seam as bound parameters, and SQL text is the calling crate's own constant
//! (ADR-45); the seam does not add tenancy: the caller stamps and filters its own tables with
//! its own tenant key (see the crate README).

use std::sync::Arc;
use std::time::{Duration, Instant};

use deadpool_postgres::Object;
use spot_storage_contract::{
    Access, Catalog, Document, DocumentData, DocumentStore, ErrorKind, GraphTraversal, Isolation,
    LockKey, LockMode, Locks, Name, Outbox, OutboxEvent, Query, QueryExecutor, RecordId, RowLock,
    Rows, Savepoint, Sequences, SeriesKey, StorageError, StorageResult, Transaction, Traversal,
    TraversalResult, TxOptions, Version, Wait,
};
use tokio_postgres::Client;
use tracing::Instrument;

use crate::backend::PgBackend;
use crate::error::{commit_outcome_unknown, is_recoverable, map_commit_error, map_pg_error};
use crate::{documents, feed, graph, locks, query, sequences};

/// Added to the statement timeout for the idle-in-transaction timeout and the client-side commit
/// deadline.
const MARGIN: Duration = Duration::from_secs(10);

/// Runs one storage operation as a `storage.query` child of the transaction's span and records
/// its database cost. `$rows` maps a successful result to `(rows read, rows written)`; a failure
/// records the time and no rows, because neither is known then.
///
/// The span carries the statement kind and the transaction's label only — never SQL text, a
/// parameter value or row data.
macro_rules! traced {
    ($this:ident, $kind:literal, $rows:expr, $run:expr) => {{
        let span = tracing::info_span!(
            parent: &$this.span,
            "storage.query",
            kind = $kind,
            label = %$this.options.label,
        );
        let start = Instant::now();
        let result = ($run).instrument(span).await;
        let (read, written) = match &result {
            Ok(value) => ($rows)(value),
            Err(_) => (0, 0),
        };
        spot_telemetry::cost::record_db(start.elapsed(), read, written);
        result
    }};
}

/// What the transaction may still do.
enum State {
    /// Normal.
    Active,
    /// The server aborted the transaction after a recoverable error; only `rollback_to` an
    /// earlier savepoint revives it.
    Aborted(StorageError),
    /// Unusable after an error of a non-recoverable kind.
    Poisoned(StorageError),
}

/// What one statement sent through the raw-SQL seam does to the database.
///
/// The caller states it because the crate never parses SQL (ADR-45). Build it with
/// [`PgTransaction::READ`] or [`PgTransaction::write`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteKind {
    /// Reads only: counts no write.
    Read,
    /// Inserts, updates or deletes `n` records: counts `n` against `TxOptions::max_writes`.
    Write(u32),
}

/// An open transaction. See the module documentation for its error, commit and drop rules.
pub struct PgTransaction {
    conn: Option<Object>,
    catalog: Arc<Catalog>,
    options: TxOptions,
    /// The database this transaction runs against; one tenant is one database (ADR-34).
    tenant: String,
    state: State,
    /// Live savepoints in creation order: `(id, writes counted when it was taken)`.
    savepoints: Vec<(u32, u32)>,
    next_savepoint: u32,
    writes: u32,
    statements: u64,
    /// The `storage.tx` span of this transaction: the parent of every `storage.query` span.
    span: tracing::Span,
    /// Set once the server confirmed the transaction ended; only then may the connection be reused.
    ended: bool,
}

/// Opens a transaction (see the module documentation).
pub(crate) async fn begin(
    backend: &PgBackend,
    catalog: Arc<Catalog>,
    options: TxOptions,
    max_writes_per_tx: u32,
) -> StorageResult<PgTransaction> {
    if options.max_writes > max_writes_per_tx {
        return Err(StorageError::new(
            ErrorKind::InvalidValue,
            format!(
                "max_writes {} is above this backend's limit of {max_writes_per_tx}; chunk the work",
                options.max_writes
            ),
        ));
    }
    let app_name = application_name(&backend.config().application_name, options.label.as_str());
    let tenant = backend.config().dbname.clone();
    // The transaction span lives as long as the transaction: every statement span is its child,
    // and the caller's span (the HTTP request or the job attempt) is its parent.
    let span = tracing::info_span!("storage.tx", label = %options.label);
    let start = Instant::now();
    // A pooled connection that died while idle is only noticed by the first statement. Nothing
    // was done on it, so one retry on a fresh connection is safe.
    let mut attempt = 0;
    let result = async {
        loop {
            let conn = backend.checkout().await?;
            let mut tx = PgTransaction::new(
                conn,
                catalog.clone(),
                options.clone(),
                tenant.clone(),
                span.clone(),
            );
            match tx.start(&app_name).await {
                Ok(()) => return Ok(tx),
                Err(e) if e.kind == ErrorKind::Unavailable && attempt == 0 => attempt += 1,
                Err(e) => return Err(e),
            }
        }
    }
    .instrument(span.clone())
    .await;
    // `BEGIN` and its one `set_config` statement are the transaction's opening cost; rows are
    // not known here, and a failed checkout is not a statement.
    if result.is_ok() {
        spot_telemetry::cost::record_db(start.elapsed(), 0, 0);
    }
    result
}

impl PgTransaction {
    fn new(
        conn: Object,
        catalog: Arc<Catalog>,
        options: TxOptions,
        tenant: String,
        span: tracing::Span,
    ) -> Self {
        Self {
            conn: Some(conn),
            catalog,
            options,
            tenant,
            state: State::Active,
            savepoints: Vec::new(),
            next_savepoint: 1,
            writes: 0,
            statements: 0,
            span,
            ended: false,
        }
    }

    async fn start(&mut self, app_name: &str) -> StorageResult<()> {
        let begin = match (self.options.isolation, self.options.access) {
            (Isolation::ReadCommitted, Access::ReadWrite) => {
                "BEGIN ISOLATION LEVEL READ COMMITTED READ WRITE"
            }
            (Isolation::ReadCommitted, Access::ReadOnly) => {
                "BEGIN ISOLATION LEVEL READ COMMITTED READ ONLY"
            }
            (Isolation::RepeatableRead, Access::ReadWrite) => {
                "BEGIN ISOLATION LEVEL REPEATABLE READ READ WRITE"
            }
            (Isolation::RepeatableRead, Access::ReadOnly) => {
                "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY"
            }
            (Isolation::Serializable, Access::ReadWrite) => {
                "BEGIN ISOLATION LEVEL SERIALIZABLE READ WRITE"
            }
            (Isolation::Serializable, Access::ReadOnly) => {
                "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY"
            }
        };
        let client = self.client()?;
        let result = client.batch_execute(begin).await;
        result.map_err(|e| self.fail(&e, None))?;

        let statement = self.options.statement_timeout;
        let idle = if statement.is_zero() {
            statement
        } else {
            statement + MARGIN
        };
        let lock_ms = millis(self.options.lock_timeout);
        let statement_ms = millis(statement);
        let idle_ms = millis(idle);
        let client = self.client()?;
        let result = client
            .execute(
                "SELECT set_config('lock_timeout', $1, true), \
                        set_config('statement_timeout', $2, true), \
                        set_config('idle_in_transaction_session_timeout', $3, true), \
                        set_config('application_name', $4, true)",
                &[&lock_ms, &statement_ms, &idle_ms, &app_name],
            )
            .await;
        result.map_err(|e| self.fail(&e, None))?;
        Ok(())
    }

    // --- accessors for the area modules ------------------------------------------------------

    /// The connection, for one statement. Fails at once, sending nothing, if the transaction is
    /// aborted or poisoned. Every statement of every area module goes through this.
    pub(crate) fn client(&mut self) -> StorageResult<&Client> {
        self.ensure_usable()?;
        self.statements += 1;
        let conn = self.conn.as_ref().ok_or_else(|| {
            StorageError::new(ErrorKind::Internal, "the transaction has no connection")
        })?;
        Ok(conn)
    }

    /// Records a driver error and returns its mapped form. `lock_wait` is the wait mode of the
    /// lock the failed statement asked for, if any (it tells `NOWAIT` from a lock timeout).
    pub(crate) fn fail(
        &mut self,
        err: &tokio_postgres::Error,
        lock_wait: Option<Wait>,
    ) -> StorageError {
        let mapped = map_pg_error(err, lock_wait);
        self.record(mapped, true)
    }

    /// Records an error found without touching the database (validation, write limit). Errors of
    /// a recoverable kind leave the transaction as it was; others poison it.
    pub(crate) fn fail_storage(&mut self, err: StorageError) -> StorageError {
        self.record(err, false)
    }

    /// The catalog snapshot, shared.
    // Used by the area modules that later tasks fill in; allowed (not expected) so they need not edit this file.
    #[allow(dead_code)]
    pub(crate) fn catalog_arc(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    // See `catalog_arc`.
    #[allow(dead_code)]
    /// Counts `n` inserted, updated or deleted records (child rows and outbox events included)
    /// against `max_writes`. Over the limit fails with `TransactionTooLarge` and poisons the
    /// transaction. Writes undone by `rollback_to` are uncounted again.
    pub(crate) fn count_writes(&mut self, n: u32) -> StorageResult<()> {
        self.ensure_usable()?;
        let total = self.writes.saturating_add(n);
        if total > self.options.max_writes {
            let err = StorageError::new(
                ErrorKind::TransactionTooLarge,
                format!(
                    "transaction `{}` would write {total} records, above its limit of {}; chunk the work",
                    self.options.label, self.options.max_writes
                ),
            );
            return Err(self.fail_storage(err));
        }
        self.writes = total;
        Ok(())
    }

    fn ensure_usable(&self) -> StorageResult<()> {
        match &self.state {
            State::Active => Ok(()),
            State::Aborted(e) | State::Poisoned(e) => Err(unusable(e)),
        }
    }

    fn record(&mut self, err: StorageError, from_database: bool) -> StorageError {
        let next = if !is_recoverable(err.kind) {
            Some(State::Poisoned(err.clone()))
        } else if from_database {
            Some(State::Aborted(err.clone()))
        } else {
            None
        };
        if let Some(next) = next
            && !matches!(self.state, State::Poisoned(_))
        {
            self.state = next;
        }
        err
    }

    fn savepoint_index(&mut self, savepoint: Savepoint) -> StorageResult<usize> {
        match self.savepoints.iter().position(|(id, _)| *id == savepoint.0) {
            Some(i) => Ok(i),
            None => Err(self.fail_storage(StorageError::new(
                ErrorKind::InvalidQuery,
                format!(
                    "savepoint {} does not exist: it was never taken, or it was released or discarded",
                    savepoint.0
                ),
            ))),
        }
    }

    /// `ROLLBACK`, valid in every state; marks the connection reusable on success.
    async fn rollback_inner(&mut self) -> StorageResult<()> {
        let Some(conn) = self.conn.as_ref() else {
            return Ok(());
        };
        let client: &Client = conn;
        self.statements += 1;
        match client.batch_execute("ROLLBACK").await {
            Ok(()) => {
                self.ended = true;
                Ok(())
            }
            // A lost connection rolls the transaction back on the server; the connection itself
            // is discarded on drop.
            Err(e) if e.as_db_error().is_none() => Ok(()),
            Err(e) => Err(map_pg_error(&e, None)),
        }
    }

    // --- test support ------------------------------------------------------------------------

    /// How many statements this transaction has sent (or tried to send) so far. For tests of
    /// the "no SQL after poisoning" rule.
    #[cfg(feature = "testing")]
    pub fn statements_sent(&self) -> u64 {
        self.statements
    }

    /// Counts writes as an area module would; see `count_writes`.
    #[cfg(feature = "testing")]
    pub fn raw_count_writes(&mut self, n: u32) -> StorageResult<()> {
        self.count_writes(n)
    }

    // --- the raw-SQL seam ---------------------------------------------------------------------

    /// A statement that only reads, for [`raw_execute_with`](PgTransaction::raw_execute_with).
    pub const READ: WriteKind = WriteKind::Read;

    /// A statement that inserts, updates or deletes `records` records, for
    /// [`raw_execute_with`](PgTransaction::raw_execute_with). A sibling crate calls this with the
    /// number of rows it wrote; the crate never parses SQL to guess it (ADR-45).
    pub const fn write(records: u32) -> WriteKind {
        WriteKind::Write(records)
    }

    /// The tenant this transaction runs against: the database name, because one tenant is one
    /// database (ADR-34). A sibling crate that stamps or filters its own tables by tenant reads
    /// it here rather than opening a second connection to ask.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Runs one reading statement with bound parameters on the transaction's own connection and
    /// returns its rows. Use [`raw_query_opt`](PgTransaction::raw_query_opt) when at most one row
    /// is expected.
    ///
    /// # Rules
    ///
    /// - The statement runs in this transaction, on this transaction's connection: it is never a
    ///   second checkout, so it sees uncommitted work of the same transaction and commits or
    ///   rolls back with it.
    /// - It counts no write. Reading a sibling table is not a write.
    /// - A failure maps through the same SQLSTATE table as the area modules: a recoverable kind
    ///   aborts the transaction until a savepoint is rolled back, any other kind poisons it, and
    ///   in both states the next call sends no SQL.
    /// - `lock_wait` says how the statement asked for a lock (`Some(Wait::NoWait)` turns a
    ///   `55P03` refusal into `LockNotAvailable` instead of `LockTimeout`); pass `None` when the
    ///   statement takes no lock or blocks normally.
    pub async fn raw_query(
        &mut self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
        lock_wait: Option<Wait>,
    ) -> StorageResult<Vec<tokio_postgres::Row>> {
        traced!(
            self,
            "raw_query",
            |rows: &Vec<tokio_postgres::Row>| (u64::try_from(rows.len()).unwrap_or(u64::MAX), 0),
            async {
                let client = self.client()?;
                let result = client.query(sql, params).await;
                result.map_err(|e| self.fail(&e, lock_wait))
            }
        )
    }

    /// Like [`raw_query`](PgTransaction::raw_query) but for a statement that must return zero or
    /// one row: `None` when it matched nothing, the row when it matched one, and an
    /// `InvalidQuery` error when it matched more than one. The transaction stays usable after
    /// that error because nothing failed on the server.
    pub async fn raw_query_opt(
        &mut self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
        lock_wait: Option<Wait>,
    ) -> StorageResult<Option<tokio_postgres::Row>> {
        let mut rows = self.raw_query(sql, params, lock_wait).await?;
        match rows.len() {
            0 => Ok(None),
            1 => Ok(rows.pop()),
            n => Err(self.fail_storage(StorageError::new(
                ErrorKind::InvalidQuery,
                format!("the statement returned {n} rows; it must return at most one"),
            ))),
        }
    }

    /// Runs one statement that writes at most one record with bound parameters on the
    /// transaction's own connection, and returns the rows affected.
    ///
    /// This is the per-row form: it counts **one** write against `TxOptions::max_writes` per
    /// call, before any SQL is sent. For a statement that writes several records, or for a
    /// statement that only reads, state it with
    /// [`raw_execute_with`](PgTransaction::raw_execute_with).
    ///
    /// # Rules
    ///
    /// - A read-only transaction refuses the call with `ReadOnlyTransaction` and sends no SQL; as
    ///   with the area modules that error poisons the transaction.
    /// - Over `max_writes` the call fails with `TransactionTooLarge` and sends no SQL.
    /// - A failure from the server maps through the same table as the area modules: recoverable
    ///   kinds abort until a savepoint is rolled back, others poison, and the next call sends no
    ///   SQL.
    pub async fn raw_execute(
        &mut self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> StorageResult<u64> {
        self.raw_execute_with(WriteKind::Write(1), sql, params)
            .await
    }

    /// Runs one statement with bound parameters on the transaction's own connection, counting
    /// what it writes as `kind` states, and returns the rows affected.
    ///
    /// `kind` is the caller's declaration, not a guess: [`WriteKind::Read`] counts nothing (and
    /// is allowed in a read-only transaction), [`WriteKind::Write(n)`] counts `n` records. The
    /// write count is taken before the statement is sent, so a refused write reaches no server,
    /// and a `rollback_to` an earlier savepoint gives back the writes it undid.
    ///
    /// Everything else — the connection, error mapping, poisoning and savepoint revival — is
    /// exactly as for [`raw_query`](PgTransaction::raw_query) and [`raw_execute`](PgTransaction::raw_execute).
    pub async fn raw_execute_with(
        &mut self,
        kind: WriteKind,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> StorageResult<u64> {
        // Aborted or poisoned first: the original error, and no SQL, exactly as `client()` would.
        self.ensure_usable()?;
        if let WriteKind::Write(records) = kind {
            if self.options.access == Access::ReadOnly {
                let err = StorageError::new(
                    ErrorKind::ReadOnlyTransaction,
                    "a write was attempted in a read-only transaction",
                );
                return Err(self.fail_storage(err));
            }
            self.count_writes(records)?;
        }
        traced!(
            self,
            "raw_execute",
            |affected: &u64| match kind {
                WriteKind::Read => (*affected, 0),
                WriteKind::Write(_) => (0, *affected),
            },
            async {
                let client = self.client()?;
                let result = client.execute(sql, params).await;
                result.map_err(|e| self.fail(&e, None))
            }
        )
    }
}

impl Drop for PgTransaction {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if self.ended {
                drop(conn);
            } else {
                // Still (possibly) in a transaction: never recycle. Dropping the client ends the
                // session and the server rolls the transaction back.
                drop(Object::take(conn));
            }
        }
    }
}

impl Transaction for PgTransaction {
    fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn options(&self) -> &TxOptions {
        &self.options
    }

    async fn savepoint(&mut self) -> StorageResult<Savepoint> {
        let id = self.next_savepoint;
        let next = id.checked_add(1).ok_or_else(|| {
            StorageError::new(
                ErrorKind::Internal,
                "too many savepoints in one transaction",
            )
        })?;
        let sql = format!("SAVEPOINT sp_{id}");
        let result = traced!(self, "savepoint", |_: &()| (0, 0), async {
            let client = self.client()?;
            client
                .batch_execute(&sql)
                .await
                .map_err(|e| self.fail(&e, None))
        });
        result?;
        self.next_savepoint = next;
        self.savepoints.push((id, self.writes));
        Ok(Savepoint(id))
    }

    async fn rollback_to(&mut self, savepoint: Savepoint) -> StorageResult<()> {
        if let State::Poisoned(e) = &self.state {
            return Err(unusable(e));
        }
        let index = self.savepoint_index(savepoint)?;
        let sql = format!("ROLLBACK TO SAVEPOINT sp_{}", savepoint.0);
        let result = traced!(self, "rollback_to", |_: &()| (0, 0), async {
            // Allowed on an aborted transaction: it is the one way back.
            self.statements += 1;
            let Some(conn) = self.conn.as_ref() else {
                return Err(StorageError::new(
                    ErrorKind::Internal,
                    "the transaction has no connection",
                ));
            };
            let client: &Client = conn;
            client
                .batch_execute(&sql)
                .await
                .map_err(|e| self.fail(&e, None))
        });
        result?;
        let (_, writes) = self.savepoints[index];
        self.savepoints.truncate(index + 1);
        self.writes = writes;
        if matches!(self.state, State::Aborted(_)) {
            self.state = State::Active;
        }
        Ok(())
    }

    async fn release(&mut self, savepoint: Savepoint) -> StorageResult<()> {
        self.ensure_usable()?;
        let index = self.savepoint_index(savepoint)?;
        let sql = format!("RELEASE SAVEPOINT sp_{}", savepoint.0);
        traced!(self, "release", |_: &()| (0, 0), async {
            let client = self.client()?;
            client
                .batch_execute(&sql)
                .await
                .map_err(|e| self.fail(&e, None))
        })?;
        self.savepoints.truncate(index);
        Ok(())
    }

    async fn commit(mut self) -> StorageResult<()> {
        if let Err(e) = self.ensure_usable() {
            // Never send COMMIT: on an aborted transaction PostgreSQL would roll back silently.
            let _ = self.rollback_inner().await;
            return Err(e);
        }
        traced!(self, "commit", |_: &()| (0, 0), async {
            let Some(conn) = self.conn.as_ref() else {
                return Err(StorageError::new(
                    ErrorKind::Internal,
                    "the transaction has no connection",
                ));
            };
            let client: &Client = conn;
            if client.is_closed() {
                return Err(StorageError::new(
                    ErrorKind::Unavailable,
                    "the connection was lost before commit; the transaction was not committed",
                ));
            }
            self.statements += 1;
            let send = client.batch_execute("COMMIT");
            let outcome = if self.options.statement_timeout.is_zero() {
                Ok(send.await)
            } else {
                tokio::time::timeout(self.options.statement_timeout + MARGIN, send).await
            };
            match outcome {
                Ok(Ok(())) => {
                    self.ended = true;
                    Ok(())
                }
                Ok(Err(e)) => {
                    let mapped = map_commit_error(&e, &self.catalog);
                    // A definite server error ended the transaction (rolled back): reusable.
                    self.ended = mapped.kind != ErrorKind::CommitOutcomeUnknown;
                    Err(mapped)
                }
                Err(_elapsed) => Err(commit_outcome_unknown()),
            }
        })
    }

    async fn rollback(mut self) -> StorageResult<()> {
        traced!(self, "rollback", |_: &()| (0, 0), self.rollback_inner())
    }
}

/// The error for an operation on an aborted or poisoned transaction: the original kind.
fn unusable(original: &StorageError) -> StorageError {
    let mut e = original.clone();
    e.explanation = format!(
        "the transaction was aborted by an earlier error ({}) and can only be rolled back: {}",
        original.kind.code(),
        original.explanation
    );
    e
}

/// Milliseconds as text for `set_config`, clamped to PostgreSQL's `int` range.
fn millis(d: Duration) -> String {
    format!("{}ms", d.as_millis().min(i32::MAX as u128))
}

/// `<application>:<label>`, cut to PostgreSQL's 63-byte limit on a character boundary.
fn application_name(base: &str, label: &str) -> String {
    let mut name = format!("{base}:{label}");
    let mut end = name.len().min(63);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.truncate(end);
    name
}

// --- trait impls: thin delegations to the area modules ---------------------------------------

impl DocumentStore for PgTransaction {
    async fn insert(&mut self, doc: &DocumentData) -> StorageResult<Version> {
        traced!(
            self,
            "insert",
            |_: &Version| (0, 1),
            documents::insert(self, doc)
        )
    }

    async fn load(
        &mut self,
        entity: &Name,
        id: RecordId,
        lock: RowLock,
    ) -> StorageResult<Option<Document>> {
        traced!(
            self,
            "load",
            |doc: &Option<Document>| (u64::from(doc.is_some()), 0),
            documents::load(self, entity, id, lock)
        )
    }

    async fn update(&mut self, doc: &DocumentData, expected: Version) -> StorageResult<Version> {
        traced!(
            self,
            "update",
            |_: &Version| (0, 1),
            documents::update(self, doc, expected)
        )
    }

    async fn delete(
        &mut self,
        entity: &Name,
        id: RecordId,
        expected: Version,
    ) -> StorageResult<()> {
        traced!(
            self,
            "delete",
            |_: &()| (0, 1),
            documents::delete(self, entity, id, expected)
        )
    }
}

impl QueryExecutor for PgTransaction {
    async fn query(&mut self, q: &Query) -> StorageResult<Rows> {
        traced!(
            self,
            "query",
            |rows: &Rows| (u64::try_from(rows.rows.len()).unwrap_or(u64::MAX), 0),
            query::query(self, q)
        )
    }
}

impl GraphTraversal for PgTransaction {
    async fn traverse(&mut self, traversal: &Traversal) -> StorageResult<TraversalResult> {
        traced!(
            self,
            "traverse",
            |result: &TraversalResult| {
                (u64::try_from(result.nodes.len()).unwrap_or(u64::MAX), 0)
            },
            graph::traverse(self, traversal)
        )
    }
}

impl Locks for PgTransaction {
    async fn lock_key(&mut self, key: &LockKey, mode: LockMode, wait: Wait) -> StorageResult<()> {
        traced!(
            self,
            "lock",
            |_: &()| (0, 0),
            locks::lock_key(self, key, mode, wait)
        )
    }
}

impl Sequences for PgTransaction {
    async fn allocate(&mut self, series: &SeriesKey, count: u32) -> StorageResult<u64> {
        traced!(
            self,
            "allocate",
            |_: &u64| (0, 0),
            sequences::allocate(self, series, count)
        )
    }

    async fn set_next(&mut self, series: &SeriesKey, next: u64) -> StorageResult<()> {
        traced!(
            self,
            "set_next",
            |_: &()| (0, 1),
            sequences::set_next(self, series, next)
        )
    }
}

impl Outbox for PgTransaction {
    async fn enqueue(&mut self, event: &OutboxEvent) -> StorageResult<()> {
        let result = traced!(self, "enqueue", |_: &()| (0, 1), feed::enqueue(self, event));
        if result.is_ok() {
            spot_telemetry::cost::record_outbox_events(1);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeouts_are_formatted_in_milliseconds() {
        assert_eq!(millis(Duration::from_secs(5)), "5000ms");
        assert_eq!(millis(Duration::ZERO), "0ms");
        assert_eq!(
            millis(Duration::from_secs(u64::MAX / 4)),
            format!("{}ms", i32::MAX)
        );
    }

    #[test]
    fn application_name_is_cut_at_63_bytes() {
        assert_eq!(
            application_name("spot", "post_sales_invoice"),
            "spot:post_sales_invoice"
        );
        let long = application_name(&"é".repeat(40), "label");
        assert!(long.len() <= 63);
        assert!(long.is_char_boundary(long.len()));
    }
}
