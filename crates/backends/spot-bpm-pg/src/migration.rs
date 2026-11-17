//! [`PgCaseStore::apply_migration`]: moving one running Case to another process version
//! (WP-BPM-09.T7, ADR-045).
//!
//! The mapping (which node becomes which, what a variable defaults to) stays in
//! `spot-bpm-versioning`; the caller hands this method the finished new state. This method owns
//! only the writes to the Case tables, in **one** transaction: it re-reads the Case `FOR UPDATE`,
//! refuses (a typed [`MigrationOutcome::Skipped`], never a forced move) if the Case is not where
//! the plan saw it, and otherwise rewrites the Case and its variables, counts the move on the
//! migration record and writes the per-Case audit row. Either all four land or none does.
//!
//! The tables `spot_bpm_migration` and `spot_bpm_migration_audit` are created by the host
//! (`spot-cli`, `process_versions.rs`), not by [`PgCaseStore::prepare`]; this method writes to
//! them only.

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, EngineResult};

use crate::error;
use crate::sql;
use crate::store::PgCaseStore;

/// Everything one move needs: which Case, what the plan saw, and the finished new state.
#[derive(Clone, Copy, Debug)]
pub struct MigrationMove<'a> {
    /// The Case to move.
    pub case: &'a CaseId,
    /// The process name the Case must still be pinned to.
    pub from_name: &'a str,
    /// The version the Case must still be on.
    pub from_version: u32,
    /// The Case's stored revision (`spot_bpm_case.version`) the plan was made at; a Case that was
    /// written since is skipped.
    pub expected_revision: u64,
    /// The version the Case moves to.
    pub to_version: u32,
    /// The compiled content hash of the target version.
    pub to_hash: &'a str,
    /// The migrated node map, as the `nodes` column holds it.
    pub nodes: &'a Json,
    /// The migrated taken edges, as the `taken` column holds them.
    pub taken: &'a Json,
    /// Whether the migrated Case has a ready node (the `has_ready` flag `list_runnable` reads).
    pub has_ready: bool,
    /// The migrated variables.
    pub variables: &'a Json,
    /// The migration record whose moved count is bumped.
    pub migration_id: &'a str,
    /// The plan digest: with the Case it keys the audit row, so a replay cannot write it twice.
    pub plan_digest: &'a str,
    /// The audit payload (the outbox JSON of the migration audit; codes and ids only).
    pub audit_payload: &'a str,
    /// The instant of the move.
    pub at: OffsetDateTime,
}

/// Why a Case was not moved. The reasons are stable codes; none carries a value of the Case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// No Case of that id exists for this tenant.
    NotFound,
    /// The Case is no longer running (it completed, failed or was cancelled).
    NotRunning,
    /// The Case is no longer on the expected process version.
    NotOnVersion,
    /// The Case was written after the plan was made.
    ChangedSincePlan,
    /// A node is running or an attempt is in flight or retrying.
    MidNode,
    /// The Case waits on a timer, an event, a person or a child.
    Waiting,
}

impl SkipReason {
    /// The stable code of the reason.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "bpm_versioning.case_not_found",
            Self::NotRunning => "bpm_versioning.case_not_running",
            Self::NotOnVersion => "bpm_versioning.case_not_on_version",
            Self::ChangedSincePlan => "bpm_versioning.case_changed",
            Self::MidNode => "bpm_versioning.case_mid_node",
            Self::Waiting => "bpm_versioning.case_waiting",
        }
    }
}

/// What [`PgCaseStore::apply_migration`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// The Case moved, the migration counted it and the audit row was written.
    Moved,
    /// Nothing was written; the reason says why.
    Skipped(SkipReason),
}

impl PgCaseStore {
    /// How many running Cases of `process` sit on each version, as `(version, count)` in version
    /// order. The Cases of other tenants are not counted.
    ///
    /// # Errors
    /// A store failure.
    pub async fn running_by_version(&self, process: &str) -> EngineResult<Vec<(u32, i64)>> {
        let conn = self.conn().await?;
        let rows = conn
            .query(sql::COUNT_RUNNING_BY_VERSION, &[&self.tenant(), &process])
            .await
            .map_err(error::pg)?;
        let mut counts = Vec::with_capacity(rows.len());
        for row in &rows {
            let version: i32 = row
                .try_get("dag_version")
                .map_err(|e| error::corrupt(e.to_string()))?;
            let running: i64 = row
                .try_get("running")
                .map_err(|e| error::corrupt(e.to_string()))?;
            let version = u32::try_from(version)
                .map_err(|_| error::corrupt("a stored process version is negative"))?;
            counts.push((version, running));
        }
        counts.sort_unstable();
        Ok(counts)
    }

    /// Up to `limit` ids of the running Cases pinned to `process` at `version`, after the id
    /// `after` (empty for the first page), in id order.
    ///
    /// # Errors
    /// A store failure.
    pub async fn running_case_ids(
        &self,
        process: &str,
        version: u32,
        after: &str,
        limit: i64,
    ) -> EngineResult<Vec<String>> {
        let version = version_column(version)?;
        let conn = self.conn().await?;
        let rows = conn
            .query(
                sql::LIST_RUNNING_ON_VERSION,
                &[&self.tenant(), &process, &version, &after, &limit],
            )
            .await
            .map_err(error::pg)?;
        rows.iter()
            .map(|row| {
                row.try_get::<_, String>("case_id")
                    .map_err(|e| error::corrupt(e.to_string()))
            })
            .collect()
    }

    /// Moves one Case to another process version, counts the move and writes its audit row, in
    /// one transaction. The Case must belong to this store's tenant.
    ///
    /// The Case is re-read `FOR UPDATE`, so two concurrent moves of one Case serialise and the
    /// second finds the Case already on the target version. Skips, in this order: no such Case,
    /// not running, not on the expected version, written since the plan, a node mid-run, waiting.
    ///
    /// # Errors
    /// A database failure, including a failed audit insert or a missing migration record: the
    /// transaction rolls back and the Case is left unmoved and uncounted.
    pub async fn apply_migration(&self, mv: &MigrationMove<'_>) -> EngineResult<MigrationOutcome> {
        let case = mv.case.as_str();
        let (from, to) = (
            version_column(mv.from_version)?,
            version_column(mv.to_version)?,
        );
        let expected = i64::try_from(mv.expected_revision)
            .map_err(|_| error::corrupt("the planned revision is out of range"))?;
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await.map_err(error::pg)?;
        let row = tx
            .query_opt(sql::SELECT_CASE_FOR_MIGRATION, &[&case, &self.tenant()])
            .await
            .map_err(error::pg)?;
        let Some(row) = row else {
            return Ok(MigrationOutcome::Skipped(SkipReason::NotFound));
        };
        let get = |error: tokio_postgres::Error| error::corrupt(error.to_string());
        let key: String = row.try_get("dag_key").map_err(get)?;
        let pinned: i32 = row.try_get("dag_version").map_err(get)?;
        let status: String = row.try_get("status").map_err(get)?;
        let revision: i64 = row.try_get("version").map_err(get)?;
        let nodes: Json = row.try_get("nodes").map_err(get)?;
        let has_children: bool = row.try_get("has_children").map_err(get)?;
        if status != "running" {
            return Ok(MigrationOutcome::Skipped(SkipReason::NotRunning));
        }
        if key != mv.from_name || pinned != from {
            return Ok(MigrationOutcome::Skipped(SkipReason::NotOnVersion));
        }
        if revision != expected {
            return Ok(MigrationOutcome::Skipped(SkipReason::ChangedSincePlan));
        }
        let running = nodes
            .as_object()
            .is_some_and(|map| map.values().any(|n| n["status"] == "running"));
        let attempts = tx
            .query(sql::SELECT_ATTEMPTS_FOR_MIGRATION, &[&case, &self.tenant()])
            .await
            .map_err(error::pg)?;
        let (mut in_flight, mut waiting) = (false, has_children);
        for attempt in &attempts {
            let state: String = attempt.try_get("state").map_err(get)?;
            let timed: bool = attempt.try_get("timed").map_err(get)?;
            in_flight |= state == "started" || state == "retrying";
            waiting |= state == "waiting" || timed;
        }
        if running || in_flight {
            return Ok(MigrationOutcome::Skipped(SkipReason::MidNode));
        }
        if waiting {
            return Ok(MigrationOutcome::Skipped(SkipReason::Waiting));
        }
        let next = revision
            .checked_add(1)
            .ok_or_else(|| error::corrupt("the Case revision overflowed"))?;
        tx.execute(
            sql::UPDATE_CASE_MIGRATED,
            &[
                &case,
                &self.tenant(),
                &to,
                &mv.to_hash,
                mv.nodes,
                mv.taken,
                &next,
                &mv.has_ready,
            ],
        )
        .await
        .map_err(error::pg)?;
        tx.execute(sql::UPSERT_VAR, &[&case, &self.tenant(), mv.variables])
            .await
            .map_err(error::pg)?;
        tx.execute(
            sql::INSERT_MIGRATION_AUDIT,
            &[
                &self.tenant(),
                &mv.migration_id,
                &case,
                &mv.plan_digest,
                &mv.audit_payload,
                &mv.at,
            ],
        )
        .await
        .map_err(error::pg)?;
        let counted = tx
            .execute(
                sql::COUNT_MIGRATION_MOVED,
                &[&self.tenant(), &mv.migration_id],
            )
            .await
            .map_err(error::pg)?;
        if counted != 1 {
            // The transaction drops here and rolls back: a move that cannot be counted is not made.
            return Err(error::corrupt("the migration record does not exist"));
        }
        tx.commit().await.map_err(error::pg)?;
        Ok(MigrationOutcome::Moved)
    }
}

/// A process version as the `integer` column holds it.
fn version_column(version: u32) -> EngineResult<i32> {
    i32::try_from(version).map_err(|_| error::corrupt("the process version is out of range"))
}
