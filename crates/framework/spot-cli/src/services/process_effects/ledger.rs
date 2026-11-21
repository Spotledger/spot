//! The tables of the module and every statement that touches them.
//!
//! All rows carry the site (the tenant), so nothing is shared between sites:
//!
//! - `spot_bpm_eff_log`: the request record file, **append-only** (a trigger refuses `UPDATE` and
//!   `DELETE`). One row per receipt (`kind = 'receipt'`) or visit of a node that wrote nothing
//!   (`kind = 'visit'`), keyed by `(request, kind, node, attempt, op_index)`, so a retried write
//!   is a no-op. A receipt holds ids, counts and digests only (the contract's
//!   `OperationReceipt`): no field value, no payload body, no secret.
//! - `spot_bpm_eff_request`: who started the request, written once.
//! - `spot_bpm_eff_done`: the reversal journal: one row per reversal call that completed, keyed
//!   by the request key `undo:<request>:<entry>:<reversal>`, holding its receipt. It is what makes
//!   a resumed undo exactly-once per receipt after a restart.
//! - `spot_bpm_eff_undo`: the state of the undo of a request (`running`, `interrupted`,
//!   `needs_attention`, `done`, `abandoned`) and a reason token.
//! - `spot_bpm_eff_audit`: one row per undo, decision, refusal and hold transition: who, what,
//!   which rule.
//!
//! The statements are this module's own constants with bound parameters (ADR-45). Times are unix
//! microseconds so no driver feature is needed.

use deadpool_postgres::Pool;

/// The schema, created if absent. Every statement is idempotent.
pub const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS spot_bpm_eff_log (\
    tenant text NOT NULL, seq bigserial NOT NULL, request text NOT NULL, kind text NOT NULL, \
    node text NOT NULL, attempt integer NOT NULL, op_index integer NOT NULL, \
    body text NOT NULL, links text NOT NULL, at_us bigint NOT NULL, \
    PRIMARY KEY (tenant, seq), \
    UNIQUE (tenant, request, kind, node, attempt, op_index));\
CREATE INDEX IF NOT EXISTS spot_bpm_eff_log_request ON spot_bpm_eff_log (tenant, request, seq);\
CREATE OR REPLACE FUNCTION spot_bpm_eff_append_only() RETURNS trigger AS $body$ \
BEGIN RAISE EXCEPTION 'spot_bpm_eff_log is append-only'; END; $body$ LANGUAGE plpgsql;\
DO $body$ BEGIN \
  CREATE TRIGGER spot_bpm_eff_log_append_only BEFORE UPDATE OR DELETE ON spot_bpm_eff_log \
  FOR EACH ROW EXECUTE FUNCTION spot_bpm_eff_append_only(); \
EXCEPTION WHEN duplicate_object THEN NULL; END $body$;\
CREATE TABLE IF NOT EXISTS spot_bpm_eff_request (\
    tenant text NOT NULL, request text NOT NULL, requester text NOT NULL, created_us bigint NOT NULL, \
    PRIMARY KEY (tenant, request));\
CREATE TABLE IF NOT EXISTS spot_bpm_eff_done (\
    tenant text NOT NULL, key text NOT NULL, receipt text NOT NULL, at_us bigint NOT NULL, \
    PRIMARY KEY (tenant, key));\
CREATE TABLE IF NOT EXISTS spot_bpm_eff_undo (\
    tenant text NOT NULL, request text NOT NULL, state text NOT NULL, started_by text NOT NULL, \
    to_node text, reason text NOT NULL, started_us bigint NOT NULL, updated_us bigint NOT NULL, \
    PRIMARY KEY (tenant, request));\
CREATE TABLE IF NOT EXISTS spot_bpm_eff_audit (\
    tenant text NOT NULL, id bigserial NOT NULL, at_us bigint NOT NULL, actor text NOT NULL, \
    request text NOT NULL, action text NOT NULL, outcome text NOT NULL, rule text NOT NULL, \
    PRIMARY KEY (tenant, id));";

const OPEN_REQUEST: &str = "\
INSERT INTO spot_bpm_eff_request (tenant, request, requester, created_us) \
VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING";

const REQUESTER_OF: &str =
    "SELECT requester FROM spot_bpm_eff_request WHERE tenant = $1 AND request = $2";

const APPEND_LOG: &str = "\
INSERT INTO spot_bpm_eff_log \
    (tenant, request, kind, node, attempt, op_index, body, links, at_us) \
VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT DO NOTHING";

const READ_LOG: &str = "\
SELECT kind, node, body, links, at_us FROM spot_bpm_eff_log \
WHERE tenant = $1 AND request = $2 ORDER BY seq";

const DONE_GET: &str = "SELECT receipt FROM spot_bpm_eff_done WHERE tenant = $1 AND key = $2";

const DONE_PUT: &str = "\
INSERT INTO spot_bpm_eff_done (tenant, key, receipt, at_us) VALUES ($1, $2, $3, $4) \
ON CONFLICT DO NOTHING";

const CLAIM_UNDO: &str = "\
INSERT INTO spot_bpm_eff_undo \
    (tenant, request, state, started_by, to_node, reason, started_us, updated_us) \
VALUES ($1, $2, 'running', $3, $4, '', $5, $5) \
ON CONFLICT (tenant, request) DO UPDATE SET \
    state = 'running', started_by = EXCLUDED.started_by, \
    to_node = COALESCE(EXCLUDED.to_node, spot_bpm_eff_undo.to_node), \
    reason = '', updated_us = EXCLUDED.updated_us \
WHERE spot_bpm_eff_undo.state IN ('interrupted', 'needs_attention') \
   OR (spot_bpm_eff_undo.state = 'running' AND spot_bpm_eff_undo.updated_us < $6)";

const SET_UNDO: &str = "\
UPDATE spot_bpm_eff_undo SET state = $3, reason = $4, updated_us = $5 \
WHERE tenant = $1 AND request = $2";

const UNDO_ROW: &str = "\
SELECT state, started_by, to_node, reason FROM spot_bpm_eff_undo WHERE tenant = $1 AND request = $2";

const RESUMABLE: &str = "\
SELECT request, started_by, to_node FROM spot_bpm_eff_undo \
WHERE tenant = $1 AND (state = 'interrupted' OR (state = 'running' AND updated_us < $2)) \
ORDER BY updated_us LIMIT $3";

const AUDIT: &str = "\
INSERT INTO spot_bpm_eff_audit (tenant, at_us, actor, request, action, outcome, rule) \
VALUES ($1, $2, $3, $4, $5, $6, $7)";

/// Why a ledger call failed. Never carries a value, a statement or a driver message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LedgerError(&'static str);

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for LedgerError {}

/// One row of the request record file, as it is replayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogRow {
    /// `receipt` or `visit`.
    pub kind: String,
    /// The node.
    pub node: String,
    /// The receipt as JSON (empty for a visit).
    pub body: String,
    /// The linked records as JSON (empty for a visit).
    pub links: String,
    /// When, unix microseconds.
    pub at_us: i64,
}

/// The stored state of the undo of one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoRow {
    /// `running`, `interrupted`, `needs_attention`, `done` or `abandoned`.
    pub state: String,
    /// Who started it (a typed principal reference).
    pub started_by: String,
    /// The node it goes back to; absent: the start of the request.
    pub to_node: Option<String>,
    /// A stable reason token for `needs_attention` and `interrupted`.
    pub reason: String,
}

/// An undo that is waiting to be resumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resumable {
    /// The request.
    pub request: String,
    /// Who started it.
    pub started_by: String,
    /// The node it goes back to.
    pub to_node: Option<String>,
}

/// The ledger of one site.
#[derive(Clone)]
pub struct EffLedger {
    pool: Pool,
    tenant: String,
}

impl EffLedger {
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

    /// The pool (the hold's own ledger shares it).
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.pool
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, LedgerError> {
        self.pool.get().await.map_err(|_| LedgerError("pool"))
    }

    /// Creates the tables if they are absent.
    ///
    /// # Errors
    /// The database refused.
    pub async fn prepare(&self) -> Result<(), LedgerError> {
        self.client()
            .await?
            .batch_execute(SCHEMA)
            .await
            .map_err(|_| LedgerError("schema"))
    }

    /// Records who started `request`, once.
    ///
    /// # Errors
    /// The database refused.
    pub async fn open_request(
        &self,
        request: &str,
        requester: &str,
        at_us: i64,
    ) -> Result<(), LedgerError> {
        self.client()
            .await?
            .execute(OPEN_REQUEST, &[&self.tenant, &request, &requester, &at_us])
            .await
            .map(|_| ())
            .map_err(|_| LedgerError("open_request"))
    }

    /// Who started `request`, or `None` for an unknown request.
    ///
    /// # Errors
    /// The database refused.
    pub async fn requester_of(&self, request: &str) -> Result<Option<String>, LedgerError> {
        let row = self
            .client()
            .await?
            .query_opt(REQUESTER_OF, &[&self.tenant, &request])
            .await
            .map_err(|_| LedgerError("requester_of"))?;
        row.map(|row| row.try_get(0).map_err(|_| LedgerError("row")))
            .transpose()
    }

    /// Appends one row of the record file. `false`: the row already existed (a retry).
    ///
    /// # Errors
    /// The database refused.
    #[allow(clippy::too_many_arguments)]
    pub async fn append(
        &self,
        request: &str,
        kind: &str,
        node: &str,
        attempt: i32,
        op_index: i32,
        body: &str,
        links: &str,
        at_us: i64,
    ) -> Result<bool, LedgerError> {
        let added = self
            .client()
            .await?
            .execute(
                APPEND_LOG,
                &[
                    &self.tenant,
                    &request,
                    &kind,
                    &node,
                    &attempt,
                    &op_index,
                    &body,
                    &links,
                    &at_us,
                ],
            )
            .await
            .map_err(|_| LedgerError("append"))?;
        Ok(added == 1)
    }

    /// The request's record file, in the order it was written.
    ///
    /// # Errors
    /// The database refused.
    pub async fn log(&self, request: &str) -> Result<Vec<LogRow>, LedgerError> {
        let rows = self
            .client()
            .await?
            .query(READ_LOG, &[&self.tenant, &request])
            .await
            .map_err(|_| LedgerError("log"))?;
        let bad = |_| LedgerError("row");
        rows.iter()
            .map(|row| {
                Ok(LogRow {
                    kind: row.try_get(0).map_err(bad)?,
                    node: row.try_get(1).map_err(bad)?,
                    body: row.try_get(2).map_err(bad)?,
                    links: row.try_get(3).map_err(bad)?,
                    at_us: row.try_get(4).map_err(bad)?,
                })
            })
            .collect()
    }

    /// The receipt of a reversal that already completed under `key`.
    ///
    /// # Errors
    /// The database refused.
    pub async fn done(&self, key: &str) -> Result<Option<String>, LedgerError> {
        let row = self
            .client()
            .await?
            .query_opt(DONE_GET, &[&self.tenant, &key])
            .await
            .map_err(|_| LedgerError("done"))?;
        row.map(|row| row.try_get(0).map_err(|_| LedgerError("row")))
            .transpose()
    }

    /// Remembers that the reversal under `key` completed with `receipt`.
    ///
    /// # Errors
    /// The database refused.
    pub async fn mark_done(&self, key: &str, receipt: &str, at_us: i64) -> Result<(), LedgerError> {
        self.client()
            .await?
            .execute(DONE_PUT, &[&self.tenant, &key, &receipt, &at_us])
            .await
            .map(|_| ())
            .map_err(|_| LedgerError("mark_done"))
    }

    /// Takes the undo of `request` for `by`: a new one, one that was interrupted or waits for a
    /// decision, or a running one nobody touched since `stale_before_us`. `false`: another caller
    /// holds it.
    ///
    /// # Errors
    /// The database refused.
    pub async fn claim_undo(
        &self,
        request: &str,
        by: &str,
        to_node: Option<&str>,
        now_us: i64,
        stale_before_us: i64,
    ) -> Result<bool, LedgerError> {
        let taken = self
            .client()
            .await?
            .execute(
                CLAIM_UNDO,
                &[
                    &self.tenant,
                    &request,
                    &by,
                    &to_node,
                    &now_us,
                    &stale_before_us,
                ],
            )
            .await
            .map_err(|_| LedgerError("claim_undo"))?;
        Ok(taken == 1)
    }

    /// Sets the state of the undo of `request`.
    ///
    /// # Errors
    /// The database refused.
    pub async fn set_undo(
        &self,
        request: &str,
        state: &str,
        reason: &str,
        at_us: i64,
    ) -> Result<(), LedgerError> {
        self.client()
            .await?
            .execute(SET_UNDO, &[&self.tenant, &request, &state, &reason, &at_us])
            .await
            .map(|_| ())
            .map_err(|_| LedgerError("set_undo"))
    }

    /// The stored undo of `request`, if there is one.
    ///
    /// # Errors
    /// The database refused.
    pub async fn undo_row(&self, request: &str) -> Result<Option<UndoRow>, LedgerError> {
        let row = self
            .client()
            .await?
            .query_opt(UNDO_ROW, &[&self.tenant, &request])
            .await
            .map_err(|_| LedgerError("undo_row"))?;
        let bad = |_| LedgerError("row");
        row.map(|row| {
            Ok(UndoRow {
                state: row.try_get(0).map_err(bad)?,
                started_by: row.try_get(1).map_err(bad)?,
                to_node: row.try_get(2).map_err(bad)?,
                reason: row.try_get(3).map_err(bad)?,
            })
        })
        .transpose()
    }

    /// The undos waiting to be resumed: interrupted, or running and untouched since
    /// `stale_before_us`.
    ///
    /// # Errors
    /// The database refused.
    pub async fn resumable(
        &self,
        stale_before_us: i64,
        limit: u32,
    ) -> Result<Vec<Resumable>, LedgerError> {
        let limit = i64::from(limit);
        let rows = self
            .client()
            .await?
            .query(RESUMABLE, &[&self.tenant, &stale_before_us, &limit])
            .await
            .map_err(|_| LedgerError("resumable"))?;
        let bad = |_| LedgerError("row");
        rows.iter()
            .map(|row| {
                Ok(Resumable {
                    request: row.try_get(0).map_err(bad)?,
                    started_by: row.try_get(1).map_err(bad)?,
                    to_node: row.try_get(2).map_err(bad)?,
                })
            })
            .collect()
    }

    /// Writes one audit row. A failure is logged by rule and does not change the outcome of the
    /// call it describes (a refusal is a refusal either way).
    pub async fn audit(
        &self,
        actor: &str,
        request: &str,
        action: &str,
        outcome: &str,
        rule: &str,
        at_us: i64,
    ) {
        let written = match self.client().await {
            Ok(client) => client
                .execute(
                    AUDIT,
                    &[
                        &self.tenant,
                        &at_us,
                        &actor,
                        &request,
                        &action,
                        &outcome,
                        &rule,
                    ],
                )
                .await
                .is_ok(),
            Err(_) => false,
        };
        if !written {
            tracing::error!(action, rule, "an effects audit row could not be written");
        }
    }
}
