//! [`PgThroughput`]: completed Cases per process version per ISO week, straight from
//! `spot_bpm_case` (WP-BPM-11.T10).
//!
//! The `bpm_throughput_by_week` report needs "how many Runs completed in each ISO week, per
//! process version". The answer already sits in `spot_bpm_case` (`status`, `ended_at`,
//! `dag_key`, `dag_version`), so this is one grouped query: no new table and no event fold.
//!
//! # The rules
//!
//! - Only Cases whose `status` is `completed` count (a failed or cancelled Case is not
//!   throughput; see the task's open question).
//! - The window is `[since, until)`: `since` inclusive, `until` exclusive, both arguments. The
//!   SQL never reads `now()`.
//! - The week is the **UTC ISO week** of `ended_at`, rendered `YYYY-Www` with the ISO year (so
//!   2026-12-31 and 2027-01-01 are both `2026-W53`, and 2027-01-04 is `2027-W01`).
//! - Rows come ordered by week, then process, then version, and at most `limit` of them
//!   (clamped to `1..=5000`).
//!
//! # The index
//!
//! [`PgThroughput::prepare`] creates `spot_bpm_case_ended_idx (tenant, ended_at) WHERE ended_at
//! IS NOT NULL`, which serves the window scan. It is opt-in like the migration tables: a process
//! that never asks for throughput never pays for the index.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::EngineResult;

use crate::error;
use crate::store::DEFAULT_TENANT;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x6270_6d70_6702;

/// The most rows one call returns.
pub const MAX_LIMIT: u32 = 5000;

/// The idempotent DDL: the one index the window scan uses.
pub const DDL: &str = "CREATE INDEX IF NOT EXISTS spot_bpm_case_ended_idx \
    ON spot_bpm_case (tenant, ended_at) WHERE ended_at IS NOT NULL";

/// The weekly statement. `$1` tenant, `$2` since (inclusive), `$3` until (exclusive), `$4` limit.
pub const WEEKLY: &str = "\
SELECT dag_key AS process, dag_version AS version, \
       to_char(ended_at AT TIME ZONE 'UTC', 'IYYY-\"W\"IW') AS week, \
       count(*)::bigint AS completed \
FROM spot_bpm_case \
WHERE tenant = $1 AND status = 'completed' AND ended_at >= $2 AND ended_at < $3 \
GROUP BY dag_key, dag_version, to_char(ended_at AT TIME ZONE 'UTC', 'IYYY-\"W\"IW') \
ORDER BY week, process, version \
LIMIT $4";

/// One row: the Cases of one process version that completed in one ISO week.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WeeklyRow {
    /// The process name (`dag_key`).
    pub process: String,
    /// The process version.
    pub version: u32,
    /// The UTC ISO week of the Case's end, as `YYYY-Www`.
    pub week: String,
    /// How many Cases completed.
    pub completed: u64,
}

/// Weekly completion counts for one tenant.
#[derive(Clone)]
pub struct PgThroughput {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgThroughput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgThroughput")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgThroughput {
    /// A reader on `pool` for the default tenant. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            tenant: DEFAULT_TENANT.to_owned(),
        }
    }

    /// The same reader for another tenant.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// The tenant every query filters by.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the index if missing. Idempotent; safe from several nodes at once. The
    /// `spot_bpm_case` table must exist ([`PgCaseStore::prepare`](crate::PgCaseStore::prepare)).
    ///
    /// # Errors
    /// `bpm_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        let mut conn = self.pool.get().await.map_err(error::migration_pool)?;
        let tx = conn.transaction().await.map_err(error::migration)?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
            .await
            .map_err(error::migration)?;
        tx.batch_execute(DDL).await.map_err(error::migration)?;
        tx.commit().await.map_err(error::migration)
    }

    /// The Cases that completed in `[since, until)`, counted per process version per UTC ISO
    /// week, ordered by week then process then version, at most `limit` rows (clamped to
    /// `1..=5000`). Neither argument is read from a clock.
    ///
    /// # Errors
    /// A store failure.
    pub async fn weekly(
        &self,
        since: OffsetDateTime,
        until: OffsetDateTime,
        limit: u32,
    ) -> EngineResult<Vec<WeeklyRow>> {
        let limit = i64::from(limit.clamp(1, MAX_LIMIT));
        let conn = self.pool.get().await.map_err(error::pool)?;
        let rows = conn
            .query(WEEKLY, &[&self.tenant, &since, &until, &limit])
            .await
            .map_err(error::pg)?;
        rows.iter().map(row).collect()
    }
}

fn row(row: &tokio_postgres::Row) -> EngineResult<WeeklyRow> {
    let process: String = row
        .try_get("process")
        .map_err(|e| error::corrupt(format!("throughput process: {e}")))?;
    let version: i32 = row
        .try_get("version")
        .map_err(|e| error::corrupt(format!("throughput version: {e}")))?;
    let week: String = row
        .try_get("week")
        .map_err(|e| error::corrupt(format!("throughput week: {e}")))?;
    let completed: i64 = row
        .try_get("completed")
        .map_err(|e| error::corrupt(format!("throughput count: {e}")))?;
    Ok(WeeklyRow {
        process,
        version: u32::try_from(version)
            .map_err(|_| error::corrupt("a stored process version is negative"))?,
        week,
        completed: u64::try_from(completed)
            .map_err(|_| error::corrupt("a throughput count is negative"))?,
    })
}
