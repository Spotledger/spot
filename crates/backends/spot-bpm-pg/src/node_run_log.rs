//! The PostgreSQL node-run transition log (WP-BPM-11.T6): the [`NodeRunLog`] port of
//! `spot-bpm-engine` on [`PgCaseStore`].
//!
//! # How a record gets its `seq`
//!
//! [`append`] runs inside the transaction of [`CaseStore::commit`](spot_bpm_engine::CaseStore),
//! after every other write and immediately before the commit. It
//!
//! 1. drops the drafts whose `(case, node, attempt, state)` is already logged (a replay), so a
//!    replay burns no number;
//! 2. reserves the sequence numbers by upserting the tenant's row of `spot_bpm_node_run_seq`.
//!    That row lock is held until the commit, so two commits of one tenant serialise only on
//!    this tail, and the numbers follow **commit order**. A `bigserial` would not: a later-numbered
//!    row could commit first, a reader that polls by cursor would move past the gap, and the
//!    earlier row would be skipped for ever;
//! 3. inserts the rows.
//!
//! The Case row lock the commit already holds orders one Case's commits. A failure in any step
//! rolls the whole commit back, so the log and the node rows are always in step.
//!
//! # Retention
//!
//! Nothing here prunes. A row holds codes, keys, ids and instants (never an attempt's `output`,
//! the variables, the plain-language failure text or a person's name) and a commit adds at most a
//! few rows per node attempt. Per-tenant retention is an operator action through
//! [`NodeRunLog::prune_before`], which never deletes a record a registered consumer has not folded
//! yet (above the slowest checkpoint). A consumer that rebuilds from zero after a prune sees only
//! the retained window.

use spot_bpm_engine::node_run_log::MAX_PAGE;
use spot_bpm_engine::{
    CaseFuture, CaseId, Checkpoint, EngineError, EngineResult, LoggedState, LoggedWait,
    NodeRunDraft, NodeRunLog, NodeRunRecord, NodeRunSeq, validate_consumer,
};
use spot_dag::NodeId;
use spot_dag_exec::Run;
use tokio_postgres::{Row, Transaction};

use crate::convert;
use crate::error;
use crate::sql;
use crate::store::PgCaseStore;

/// A `u32` that fits an `integer` column.
fn small(value: u32) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// Whether `draft` is already in the log of `tenant` for the Case `case`.
async fn already_logged(
    tx: &Transaction<'_>,
    tenant: &str,
    case: &str,
    draft: &NodeRunDraft,
) -> EngineResult<bool> {
    let attempt = small(draft.attempt);
    let state = draft.state.token();
    let node = draft.node.as_str();
    let row = tx
        .query_opt(
            sql::NODE_RUN_EXISTS,
            &[&tenant, &case, &node, &attempt, &state],
        )
        .await
        .map_err(error::pg)?;
    Ok(row.is_some())
}

/// Writes the drafts of one commit, in the transaction `tx`, as the **last** step before it
/// commits. `run` is the Case after the commit (its version is the `case_version` of every
/// record).
///
/// # Errors
/// A store failure; the caller's transaction then rolls back.
pub(crate) async fn append(
    tx: &Transaction<'_>,
    tenant: &str,
    run: &Run,
    drafts: Vec<NodeRunDraft>,
) -> EngineResult<()> {
    let case = run.id.as_str();
    let mut fresh = Vec::with_capacity(drafts.len());
    for draft in drafts {
        if !already_logged(tx, tenant, case, &draft).await? {
            fresh.push(draft);
        }
    }
    if fresh.is_empty() {
        return Ok(());
    }
    let count = i64::try_from(fresh.len())
        .map_err(|_| error::corrupt("too many node-run records in one commit"))?;
    // The counter row lock is taken here, after every other write, and held to the commit.
    let last: i64 = tx
        .query_one(sql::NODE_RUN_RESERVE, &[&tenant, &count])
        .await
        .map_err(error::pg)?
        .try_get(0)
        .map_err(|error| error::corrupt(error.to_string()))?;
    let dag_version = small(run.dag_version);
    let case_version =
        i64::try_from(run.version).map_err(|_| error::corrupt("case version is out of range"))?;
    let mut seq = last - count;
    for draft in fresh {
        seq += 1;
        let attempt = small(draft.attempt);
        let state = draft.state.token();
        let node = draft.node.as_str();
        let at = convert::at(draft.at)?;
        let wait = draft.wait.map(LoggedWait::token);
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 14] = [
            &tenant,
            &seq,
            &case,
            &run.dag_key,
            &dag_version,
            &case_version,
            &node,
            &attempt,
            &state,
            &at,
            &wait,
            &draft.failure_code,
            &draft.is_retry,
            &draft.human,
        ];
        let written = tx
            .execute(sql::INSERT_NODE_RUN, &params)
            .await
            .map_err(error::pg)?;
        if written != 1 {
            // The replay check ran under the Case row lock, so a clash here means the check and
            // the index disagree; fail closed (the commit rolls back) rather than leave a gap.
            return Err(error::store_failed(
                "the node-run log refused a record its replay check had cleared",
            ));
        }
    }
    Ok(())
}

/// A column of a stored record, or a corrupt-row error naming the column.
fn col<'a, T: tokio_postgres::types::FromSql<'a>>(row: &'a Row, name: &str) -> EngineResult<T> {
    row.try_get(name)
        .map_err(|error| error::corrupt(format!("node-run log column `{name}`: {error}")))
}

/// A stored record, read back.
fn record_from_row(row: &Row) -> EngineResult<NodeRunRecord> {
    let case: String = col(row, "case_id")?;
    let node: String = col(row, "node")?;
    let state: String = col(row, "state")?;
    let wait: Option<String> = col(row, "wait")?;
    let dag_version: i32 = col(row, "dag_version")?;
    let case_version: i64 = col(row, "case_version")?;
    let attempt: i32 = col(row, "attempt")?;
    let at: spot_app_api::prelude::OffsetDateTime = col(row, "at")?;
    Ok(NodeRunRecord {
        seq: NodeRunSeq(col(row, "seq")?),
        case: CaseId::new(&case)
            .map_err(|error| error::corrupt(format!("stored case id: {error}")))?,
        dag_key: col(row, "dag_key")?,
        dag_version: u32::try_from(dag_version)
            .map_err(|_| error::corrupt("stored process version is negative"))?,
        case_version: u64::try_from(case_version)
            .map_err(|_| error::corrupt("stored case version is negative"))?,
        node: NodeId::new(&node)
            .map_err(|error| error::corrupt(format!("stored node id: {error}")))?,
        attempt: u32::try_from(attempt)
            .map_err(|_| error::corrupt("stored attempt is negative"))?,
        state: LoggedState::from_token(&state)
            .ok_or_else(|| error::corrupt("stored node-run state is unknown"))?,
        at: convert::instant(at),
        wait: match wait {
            Some(text) => Some(
                LoggedWait::from_token(&text)
                    .ok_or_else(|| error::corrupt("stored node-run wait is unknown"))?,
            ),
            None => None,
        },
        failure_code: col(row, "failure_code")?,
        is_retry: col(row, "is_retry")?,
        human: col(row, "human")?,
    })
}

impl NodeRunLog for PgCaseStore {
    fn list_after<'a>(
        &'a self,
        after: NodeRunSeq,
        limit: u32,
    ) -> CaseFuture<'a, Vec<NodeRunRecord>> {
        Box::pin(async move {
            let take = limit.min(MAX_PAGE);
            if take == 0 {
                return Ok(Vec::new());
            }
            let conn = self.conn().await?;
            let rows = conn
                .query(
                    sql::LIST_NODE_RUNS,
                    &[&self.tenant(), &after.0, &i64::from(take)],
                )
                .await
                .map_err(error::pg)?;
            rows.iter().map(record_from_row).collect()
        })
    }

    fn load_checkpoint<'a>(&'a self, consumer: &'a str) -> CaseFuture<'a, Option<Checkpoint>> {
        Box::pin(async move {
            validate_consumer(consumer)?;
            let conn = self.conn().await?;
            let row = conn
                .query_opt(
                    sql::SELECT_NODE_RUN_CHECKPOINT,
                    &[&self.tenant(), &consumer],
                )
                .await
                .map_err(error::pg)?;
            match row {
                Some(row) => Ok(Some(Checkpoint {
                    seq: NodeRunSeq(col(&row, "seq")?),
                    state: col(&row, "state")?,
                })),
                None => Ok(None),
            }
        })
    }

    fn save_checkpoint<'a>(
        &'a self,
        consumer: &'a str,
        expected: NodeRunSeq,
        next: Checkpoint,
    ) -> CaseFuture<'a, bool> {
        Box::pin(async move {
            validate_consumer(consumer)?;
            if expected.0 < 0 || next.seq < expected {
                return Err(EngineError::StoreFailed {
                    detail: "a checkpoint cannot move backwards".to_owned(),
                });
            }
            let conn = self.conn().await?;
            if expected.0 == 0 {
                // No row yet: the first save wins by the primary key.
                let inserted = conn
                    .execute(
                        sql::INSERT_NODE_RUN_CHECKPOINT,
                        &[&self.tenant(), &consumer, &next.seq.0, &next.state],
                    )
                    .await
                    .map_err(error::pg)?;
                if inserted == 1 {
                    return Ok(true);
                }
            }
            let updated = conn
                .execute(
                    sql::UPDATE_NODE_RUN_CHECKPOINT,
                    &[
                        &self.tenant(),
                        &consumer,
                        &expected.0,
                        &next.seq.0,
                        &next.state,
                    ],
                )
                .await
                .map_err(error::pg)?;
            Ok(updated == 1)
        })
    }

    fn prune_before<'a>(&'a self, before: NodeRunSeq) -> CaseFuture<'a, u64> {
        Box::pin(async move {
            let conn = self.conn().await?;
            conn.execute(sql::PRUNE_NODE_RUNS, &[&self.tenant(), &before.0])
                .await
                .map_err(error::pg)
        })
    }
}
