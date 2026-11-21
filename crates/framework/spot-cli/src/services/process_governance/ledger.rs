//! The tables of the module and every statement that touches them.
//!
//! Four small tables, all keyed by the site (the tenant) so nothing is shared between sites:
//!
//! - `spot_bpm_gov_request`: one row per `(request, node)` that was handed a grant: the attempt
//!   the step is at, the DocTypes it may write, and whether the request is closed. The grant is
//!   only as current as this row says, so a newer attempt or a closed request kills it.
//! - `spot_bpm_gov_grant_use`: one row per consumed grant, written in the same transaction as
//!   the governed write. Its primary key is `(request, node, attempt)`: a second use of the same
//!   grant finds the row and is refused (a replay).
//! - `spot_bpm_gov_audit`: one row per refusal, override and policy failure. A refusal rolls the
//!   write's own transaction back, so the row of a refusal is written on its own connection.
//! - `spot_bpm_gov_draft`: the draft records of the form step.
//!
//! The statements are this module's own constants with bound parameters (ADR-45); no value ever
//! becomes SQL text. Times are unix microseconds so no driver feature is needed.

use deadpool_postgres::Pool;
use spot_bpm_governance::VerifiedRequest;
use spot_postgres::PgTransaction;

/// The schema, created if absent. Every statement is idempotent.
pub const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS spot_bpm_gov_request (\
    tenant text NOT NULL, request text NOT NULL, node text NOT NULL, process text NOT NULL, \
    attempt integer NOT NULL, doctypes text[] NOT NULL, actor text NOT NULL, \
    opened_us bigint NOT NULL, expires_us bigint NOT NULL, closed_us bigint, \
    PRIMARY KEY (tenant, request, node));\
CREATE TABLE IF NOT EXISTS spot_bpm_gov_grant_use (\
    tenant text NOT NULL, request text NOT NULL, node text NOT NULL, attempt integer NOT NULL, \
    doctype text NOT NULL, document_id text NOT NULL, actor text NOT NULL, used_us bigint NOT NULL, \
    PRIMARY KEY (tenant, request, node, attempt));\
CREATE TABLE IF NOT EXISTS spot_bpm_gov_audit (\
    tenant text NOT NULL, id bigserial NOT NULL, at_us bigint NOT NULL, kind text NOT NULL, \
    rule text NOT NULL, actor text NOT NULL, doctype text NOT NULL, document_id text NOT NULL, \
    company text NOT NULL, detail text NOT NULL, PRIMARY KEY (tenant, id));\
CREATE TABLE IF NOT EXISTS spot_bpm_gov_draft (\
    tenant text NOT NULL, id text NOT NULL, request text NOT NULL, node text NOT NULL, \
    doctype text NOT NULL, owner text NOT NULL, state text NOT NULL, payload text NOT NULL, \
    created_us bigint NOT NULL, updated_us bigint NOT NULL, submitted_us bigint, \
    PRIMARY KEY (tenant, id));\
CREATE TABLE IF NOT EXISTS spot_bpm_gov_reversal (\
    tenant text NOT NULL, request text NOT NULL, key text NOT NULL, state text NOT NULL, \
    code text NOT NULL DEFAULT '', started_us bigint NOT NULL, finished_us bigint, \
    PRIMARY KEY (tenant, request, key));\
CREATE TABLE IF NOT EXISTS spot_bpm_gov_close_wanted (\
    tenant text NOT NULL, request text NOT NULL, wanted_us bigint NOT NULL, \
    PRIMARY KEY (tenant, request));\
CREATE INDEX IF NOT EXISTS spot_bpm_gov_draft_age ON spot_bpm_gov_draft (tenant, updated_us);";

const UPSERT_ATTEMPT: &str = "\
INSERT INTO spot_bpm_gov_request \
    (tenant, request, node, process, attempt, doctypes, actor, opened_us, expires_us) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
ON CONFLICT (tenant, request, node) DO UPDATE SET \
    process = EXCLUDED.process, attempt = EXCLUDED.attempt, doctypes = EXCLUDED.doctypes, \
    actor = EXCLUDED.actor, opened_us = EXCLUDED.opened_us, expires_us = EXCLUDED.expires_us \
WHERE spot_bpm_gov_request.closed_us IS NULL AND spot_bpm_gov_request.attempt <= EXCLUDED.attempt";

const CLOSE_REQUEST: &str = "\
UPDATE spot_bpm_gov_request SET closed_us = $3 \
WHERE tenant = $1 AND request = $2 AND closed_us IS NULL";

const BEGIN_REVERSAL: &str = "\
INSERT INTO spot_bpm_gov_reversal (tenant, request, key, state, started_us) \
VALUES ($1, $2, $3, 'pending', $4) \
ON CONFLICT (tenant, request, key) DO UPDATE SET key = EXCLUDED.key RETURNING state";

const FINISH_REVERSAL: &str = "\
UPDATE spot_bpm_gov_reversal SET state = $4, code = $5, finished_us = $6 \
WHERE tenant = $1 AND request = $2 AND key = $3 AND state = 'pending'";

const PENDING_REVERSALS: &str = "\
SELECT count(*) FROM spot_bpm_gov_reversal WHERE tenant = $1 AND request = $2 AND state = 'pending'";

const WANT_CLOSE: &str = "\
INSERT INTO spot_bpm_gov_close_wanted (tenant, request, wanted_us) VALUES ($1, $2, $3) \
ON CONFLICT DO NOTHING";

const CLOSE_WANTED: &str = "\
SELECT 1 FROM spot_bpm_gov_close_wanted WHERE tenant = $1 AND request = $2";

const CLEAR_WANTED: &str =
    "DELETE FROM spot_bpm_gov_close_wanted WHERE tenant = $1 AND request = $2";

const READY_TO_CLOSE: &str = "\
SELECT w.request FROM spot_bpm_gov_close_wanted w WHERE w.tenant = $1 AND NOT EXISTS (\
    SELECT 1 FROM spot_bpm_gov_reversal r \
    WHERE r.tenant = w.tenant AND r.request = w.request AND r.state = 'pending')";

const REAP_REVERSALS: &str = "\
DELETE FROM spot_bpm_gov_reversal WHERE tenant = $1 AND state <> 'pending' AND finished_us < $2";

const OPEN_SLOTS: &str = "\
SELECT request, node, process, attempt, doctypes FROM spot_bpm_gov_request \
WHERE tenant = $1 AND closed_us IS NULL AND expires_us > $2";

const LOCK_SLOT: &str = "\
SELECT attempt, closed_us, expires_us, ($4 = ANY(doctypes)) FROM spot_bpm_gov_request \
WHERE tenant = $1 AND request = $2 AND node = $3 FOR UPDATE";

const INSERT_USE: &str = "\
INSERT INTO spot_bpm_gov_grant_use \
    (tenant, request, node, attempt, doctype, document_id, actor, used_us) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING";

const INSERT_AUDIT: &str = "\
INSERT INTO spot_bpm_gov_audit \
    (tenant, at_us, kind, rule, actor, doctype, document_id, company, detail) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";

const REAP_REQUESTS: &str = "\
DELETE FROM spot_bpm_gov_request \
WHERE tenant = $1 AND (closed_us IS NOT NULL OR expires_us < $2) AND opened_us < $2";

const REAP_USES: &str = "DELETE FROM spot_bpm_gov_grant_use WHERE tenant = $1 AND used_us < $2";

/// Why a ledger call failed. Never carries a value, a statement or a driver message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerError(&'static str);

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for LedgerError {}

/// The step a grant is issued for.
#[derive(Clone, Debug)]
pub struct Attempt {
    /// The request (Run).
    pub request: String,
    /// The process the request runs.
    pub process: String,
    /// The step.
    pub node: String,
    /// The attempt number.
    pub attempt: u32,
    /// The DocTypes the step may write.
    pub doctypes: Vec<String>,
    /// The typed actor the grant is issued to.
    pub actor: String,
    /// When the grant was issued, unix microseconds.
    pub opened_us: i64,
    /// When the grant expires, unix microseconds.
    pub expires_us: i64,
}

/// One open step, as the cache loads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    /// The request.
    pub request: String,
    /// The step.
    pub node: String,
    /// The process.
    pub process: String,
    /// The attempt the step is at.
    pub attempt: u32,
    /// The DocTypes the step may write.
    pub doctypes: Vec<String>,
}

/// One audit row.
#[derive(Clone, Debug)]
pub struct AuditRow {
    /// When, unix microseconds.
    pub at_us: i64,
    /// The audit kind token.
    pub kind: String,
    /// The rule that decided (a stable token, never a value).
    pub rule: String,
    /// Who acted (a typed principal reference).
    pub actor: String,
    /// The record type.
    pub doctype: String,
    /// The record.
    pub document_id: String,
    /// The company or scope.
    pub company: String,
    /// A value-free description.
    pub detail: String,
}

/// How a grant's use was refused.
#[derive(Debug)]
pub enum ConsumeError {
    /// The step is closed, expired, at another attempt, or may not write the type.
    Stale,
    /// The grant was already used.
    Replay,
    /// The database failed.
    Storage(spot_storage_contract::StorageError),
}

/// The ledger of one site.
#[derive(Clone)]
pub struct Ledger {
    pool: Pool,
    tenant: String,
}

impl Ledger {
    /// The ledger of `tenant` on `pool`.
    #[must_use]
    pub fn new(pool: Pool, tenant: &str) -> Self {
        Self {
            pool,
            tenant: tenant.to_owned(),
        }
    }

    /// The tenant key every row carries.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The pool, for the draft statements.
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    /// Creates the tables if they are absent.
    ///
    /// # Errors
    /// The database refused.
    pub async fn prepare(&self) -> Result<(), LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        client
            .batch_execute(SCHEMA)
            .await
            .map_err(|_| LedgerError("schema"))
    }

    /// Records that `attempt`'s step was handed a grant. `false`: the request is closed or the
    /// step is already at a newer attempt, so no grant may be issued.
    ///
    /// # Errors
    /// The database refused.
    pub async fn open_attempt(&self, attempt: &Attempt) -> Result<bool, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        let number = i32::try_from(attempt.attempt).map_err(|_| LedgerError("attempt"))?;
        let changed = client
            .execute(
                UPSERT_ATTEMPT,
                &[
                    &self.tenant,
                    &attempt.request,
                    &attempt.node,
                    &attempt.process,
                    &number,
                    &attempt.doctypes,
                    &attempt.actor,
                    &attempt.opened_us,
                    &attempt.expires_us,
                ],
            )
            .await
            .map_err(|_| LedgerError("open_attempt"))?;
        Ok(changed == 1)
    }

    /// Closes `request`: every grant issued for it stops working.
    ///
    /// # Errors
    /// The database refused.
    pub async fn close_request(&self, request: &str, at_us: i64) -> Result<u64, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        client
            .execute(CLOSE_REQUEST, &[&self.tenant, &request, &at_us])
            .await
            .map_err(|_| LedgerError("close_request"))
    }

    /// The steps that may still use a grant at `now_us`.
    ///
    /// # Errors
    /// The database refused.
    pub async fn open_slots(&self, now_us: i64) -> Result<Vec<Slot>, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        let rows = client
            .query(OPEN_SLOTS, &[&self.tenant, &now_us])
            .await
            .map_err(|_| LedgerError("open_slots"))?;
        let mut slots = Vec::with_capacity(rows.len());
        for row in rows {
            let attempt: i32 = row.try_get(3).map_err(|_| LedgerError("row"))?;
            slots.push(Slot {
                request: row.try_get(0).map_err(|_| LedgerError("row"))?,
                node: row.try_get(1).map_err(|_| LedgerError("row"))?,
                process: row.try_get(2).map_err(|_| LedgerError("row"))?,
                attempt: u32::try_from(attempt).map_err(|_| LedgerError("row"))?,
                doctypes: row.try_get(4).map_err(|_| LedgerError("row"))?,
            });
        }
        Ok(slots)
    }

    /// Writes one audit row on its own connection (the write's transaction is rolling back).
    ///
    /// # Errors
    /// The database refused.
    pub async fn record_audit(&self, row: &AuditRow) -> Result<(), LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        client
            .execute(
                INSERT_AUDIT,
                &[
                    &self.tenant,
                    &row.at_us,
                    &row.kind,
                    &row.rule,
                    &row.actor,
                    &row.doctype,
                    &row.document_id,
                    &row.company,
                    &row.detail,
                ],
            )
            .await
            .map(|_| ())
            .map_err(|_| LedgerError("record_audit"))
    }

    /// Writes one audit row inside the transaction of the write it belongs to.
    ///
    /// # Errors
    /// The storage error, unchanged.
    pub async fn audit_in_tx(
        &self,
        tx: &mut PgTransaction,
        row: &AuditRow,
    ) -> Result<(), spot_storage_contract::StorageError> {
        tx.raw_execute(
            INSERT_AUDIT,
            &[
                &self.tenant,
                &row.at_us,
                &row.kind,
                &row.rule,
                &row.actor,
                &row.doctype,
                &row.document_id,
                &row.company,
                &row.detail,
            ],
        )
        .await
        .map(|_| ())
    }

    /// Consumes the grant of `step` for one write of `doctype`/`document` inside `tx`.
    ///
    /// The step's row is locked, so two writes racing on one grant serialise: the second finds
    /// the use row the first wrote and is refused as a replay.
    ///
    /// # Errors
    /// [`ConsumeError::Stale`], [`ConsumeError::Replay`] or the storage error.
    pub async fn consume(
        &self,
        tx: &mut PgTransaction,
        step: &VerifiedRequest,
        doctype: &str,
        document: &str,
        actor: &str,
        now_us: i64,
    ) -> Result<(), ConsumeError> {
        let attempt = i32::try_from(step.attempt).map_err(|_| ConsumeError::Stale)?;
        let row = tx
            .raw_query_opt(
                LOCK_SLOT,
                &[&self.tenant, &step.request, &step.node, &doctype],
                None,
            )
            .await
            .map_err(ConsumeError::Storage)?;
        let Some(row) = row else {
            return Err(ConsumeError::Stale);
        };
        let current: i32 = row.try_get(0).map_err(|_| ConsumeError::Stale)?;
        let closed: Option<i64> = row.try_get(1).map_err(|_| ConsumeError::Stale)?;
        let expires: i64 = row.try_get(2).map_err(|_| ConsumeError::Stale)?;
        let may_write: bool = row.try_get(3).map_err(|_| ConsumeError::Stale)?;
        if current != attempt || closed.is_some() || expires <= now_us || !may_write {
            return Err(ConsumeError::Stale);
        }
        let inserted = tx
            .raw_execute(
                INSERT_USE,
                &[
                    &self.tenant,
                    &step.request,
                    &step.node,
                    &attempt,
                    &doctype,
                    &document,
                    &actor,
                    &now_us,
                ],
            )
            .await
            .map_err(ConsumeError::Storage)?;
        if inserted == 1 {
            Ok(())
        } else {
            Err(ConsumeError::Replay)
        }
    }

    /// Notes that the undo identified by `key` of `request` has started and returns its state
    /// (`pending`, `done` or `failed`). Idempotent: a second call with the same key (a retry
    /// after a crash) changes nothing and returns the stored state.
    ///
    /// # Errors
    /// The database refused.
    pub async fn begin_reversal(
        &self,
        request: &str,
        key: &str,
        at_us: i64,
    ) -> Result<String, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        let row = client
            .query_one(BEGIN_REVERSAL, &[&self.tenant, &request, &key, &at_us])
            .await
            .map_err(|_| LedgerError("begin_reversal"))?;
        row.try_get(0).map_err(|_| LedgerError("row"))
    }

    /// Records the end of the undo `key` (`done` or `failed` with a catalogue `code`). Only a
    /// pending undo changes; returns whether this call changed it.
    ///
    /// # Errors
    /// The database refused.
    pub async fn finish_reversal(
        &self,
        request: &str,
        key: &str,
        state: &str,
        code: &str,
        at_us: i64,
    ) -> Result<bool, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        let changed = client
            .execute(
                FINISH_REVERSAL,
                &[&self.tenant, &request, &key, &state, &code, &at_us],
            )
            .await
            .map_err(|_| LedgerError("finish_reversal"))?;
        Ok(changed == 1)
    }

    /// How many undos of `request` have not finished.
    ///
    /// # Errors
    /// The database refused.
    pub async fn pending_reversals(&self, request: &str) -> Result<i64, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        let row = client
            .query_one(PENDING_REVERSALS, &[&self.tenant, &request])
            .await
            .map_err(|_| LedgerError("pending_reversals"))?;
        row.try_get(0).map_err(|_| LedgerError("row"))
    }

    /// Remembers that closing `request` was asked for while an undo was pending.
    ///
    /// # Errors
    /// The database refused.
    pub async fn want_close(&self, request: &str, at_us: i64) -> Result<(), LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        client
            .execute(WANT_CLOSE, &[&self.tenant, &request, &at_us])
            .await
            .map(|_| ())
            .map_err(|_| LedgerError("want_close"))
    }

    /// Whether a close of `request` is waiting for its undos.
    ///
    /// # Errors
    /// The database refused.
    pub async fn close_wanted(&self, request: &str) -> Result<bool, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        client
            .query_opt(CLOSE_WANTED, &[&self.tenant, &request])
            .await
            .map(|row| row.is_some())
            .map_err(|_| LedgerError("close_wanted"))
    }

    /// Forgets a waiting close (it was carried out).
    ///
    /// # Errors
    /// The database refused.
    pub async fn clear_wanted(&self, request: &str) -> Result<(), LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        client
            .execute(CLEAR_WANTED, &[&self.tenant, &request])
            .await
            .map(|_| ())
            .map_err(|_| LedgerError("clear_wanted"))
    }

    /// The requests whose close was asked for and whose undos have all finished.
    ///
    /// # Errors
    /// The database refused.
    pub async fn ready_to_close(&self) -> Result<Vec<String>, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        let rows = client
            .query(READY_TO_CLOSE, &[&self.tenant])
            .await
            .map_err(|_| LedgerError("ready_to_close"))?;
        rows.iter()
            .map(|row| row.try_get(0).map_err(|_| LedgerError("row")))
            .collect()
    }

    /// Removes closed or expired steps and old use rows older than `before_us`.
    ///
    /// # Errors
    /// The database refused.
    pub async fn reap(&self, before_us: i64) -> Result<u64, LedgerError> {
        let client = self.pool.get().await.map_err(|_| LedgerError("pool"))?;
        let steps = client
            .execute(REAP_REQUESTS, &[&self.tenant, &before_us])
            .await
            .map_err(|_| LedgerError("reap"))?;
        let uses = client
            .execute(REAP_USES, &[&self.tenant, &before_us])
            .await
            .map_err(|_| LedgerError("reap"))?;
        let reversals = client
            .execute(REAP_REVERSALS, &[&self.tenant, &before_us])
            .await
            .map_err(|_| LedgerError("reap"))?;
        Ok(steps + uses + reversals)
    }
}
