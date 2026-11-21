//! [`PgRunDirectory`]: the visibility-filtered, keyset-paged listing of a tenant's Cases
//! (WP-BPM-02.T9), the storage half of the process API's `run_list`.
//!
//! The viewer's visibility is applied **inside the SQL**, before ordering, the page cut and the
//! next-page decision, so a page never leaks a row and never comes back short because rows were
//! dropped afterwards. One statement, bound parameters only ([`LISTING_SQL`]); every statement
//! binds the tenant, cursor included.
//!
//! Two indexes serve it, created by [`PgRunDirectory::prepare`] (idempotent, named `spot_*`):
//!
//! * `spot_bpm_case_listing_idx (tenant, started_at DESC, case_id DESC)`: the unfiltered and the
//!   `process`/`status` shapes read the newest rows first and stop at the limit;
//! * `spot_bpm_case_initiator_idx (tenant, initiator, started_at DESC, case_id DESC)` (partial,
//!   `initiator IS NOT NULL`): the `mine` shape and an ordinary viewer's own rows.
//!
//! # How a long `also_cases` list scales
//!
//! `case_id = ANY($array)` is a primary-key probe per element, so an ordinary viewer's cost grows
//! with the list; the caller caps it at 1,000 and this type refuses more than
//! [`MAX_ALSO_CASES`]. A join to the task table (WP-BPM-05.T6) would replace the array with an
//! index range on the assignee and scale with the viewer's open tasks, not a list sent per call.

use deadpool_postgres::Pool;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::EngineResult;

use crate::error;
use crate::store::DEFAULT_TENANT;

/// The most `also_cases` entries a query may carry; more is a store error.
pub const MAX_ALSO_CASES: usize = 5_000;
/// The largest page.
pub const MAX_LIMIT: u32 = 200;

/// The listing statement. `$1` tenant, `$2` sees_all, `$3` viewer, `$4` also_cases (text[]),
/// `$5` mine, `$6` process (nullable), `$7` status (nullable), `$8`/`$9` the cursor's
/// `started_at` and `case_id` (nullable), `$10` the row limit.
pub const LISTING_SQL: &str = "\
SELECT case_id, started_at FROM spot_bpm_case \
WHERE tenant = $1 \
  AND ($2::boolean OR initiator = $3::text OR case_id = ANY($4::text[])) \
  AND (NOT $5::boolean OR initiator = $3::text) \
  AND ($6::text IS NULL OR dag_key = $6::text) \
  AND ($7::text IS NULL OR status = $7::text) \
  AND ($8::timestamptz IS NULL OR (started_at, case_id) < ($8::timestamptz, $9::text)) \
ORDER BY started_at DESC, case_id DESC \
LIMIT $10::bigint";

const LISTING_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS spot_bpm_case_listing_idx \
    ON spot_bpm_case (tenant, started_at DESC, case_id DESC)";

const INITIATOR_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS spot_bpm_case_initiator_idx \
    ON spot_bpm_case (tenant, initiator, started_at DESC, case_id DESC) \
    WHERE initiator IS NOT NULL";

/// The table's `status` values, as a filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseStatusFilter {
    /// `running`.
    Running,
    /// `completed`.
    Completed,
    /// `failed`.
    Failed,
    /// `cancelled`.
    Cancelled,
}

impl CaseStatusFilter {
    /// The value stored in `spot_bpm_case.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A position in the listing order: the last row of a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingCursor {
    /// The row's start instant.
    pub started_at: OffsetDateTime,
    /// The row's Case id (the tie-breaker).
    pub case_id: String,
}

/// What to list and for whom.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingQuery {
    /// The viewer's user (matched against the Case's initiator).
    pub viewer: String,
    /// Cases the viewer sees for another reason (an assigned task), at most [`MAX_ALSO_CASES`].
    pub also_cases: Vec<String>,
    /// The viewer sees every Case of the tenant (a supervisor).
    pub sees_all: bool,
    /// Only Cases of this process (`dag_key`).
    pub process: Option<String>,
    /// Only Cases in this status.
    pub status: Option<CaseStatusFilter>,
    /// Narrow to the viewer's own Cases even when `sees_all`.
    pub mine: bool,
    /// Start strictly after this position, in newest-first order.
    pub after: Option<ListingCursor>,
    /// The page size; clamped to `1..=200`.
    pub limit: u32,
}

/// One page of Case ids, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingPage {
    /// The Case ids of the page.
    pub case_ids: Vec<String>,
    /// The position to continue from; `None` on the last page (no empty page follows).
    pub next: Option<ListingCursor>,
}

/// The listing over one tenant's `spot_bpm_case` rows.
#[derive(Clone)]
pub struct PgRunDirectory {
    pool: Pool,
    tenant: String,
}

impl PgRunDirectory {
    /// A directory on `pool` for the default tenant.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            tenant: DEFAULT_TENANT.to_owned(),
        }
    }

    /// The same directory for another tenant.
    #[must_use]
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// The tenant every statement is bound to.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the two listing indexes if missing. Idempotent. The Case table itself is
    /// [`PgCaseStore::prepare`](crate::PgCaseStore::prepare)'s and must exist first.
    ///
    /// # Errors
    /// A store failure.
    pub async fn prepare(&self) -> EngineResult<()> {
        let conn = self.pool.get().await.map_err(error::pool)?;
        conn.batch_execute(&format!("{LISTING_INDEX}; {INITIATOR_INDEX};"))
            .await
            .map_err(error::pg)
    }

    /// One page of the viewer's visible Cases, newest first.
    ///
    /// # Errors
    /// A store failure, or `also_cases` longer than [`MAX_ALSO_CASES`].
    pub async fn page(&self, query: &ListingQuery) -> EngineResult<ListingPage> {
        if query.also_cases.len() > MAX_ALSO_CASES {
            return Err(error::store_failed(
                "bpm store: too many also_cases entries in a listing query",
            ));
        }
        let limit = query.limit.clamp(1, MAX_LIMIT);
        let fetch = i64::from(limit) + 1;
        let process = query.process.as_deref();
        let status = query.status.map(CaseStatusFilter::as_str);
        let after_at = query.after.as_ref().map(|c| c.started_at);
        let after_id = query.after.as_ref().map(|c| c.case_id.as_str());
        let conn = self.pool.get().await.map_err(error::pool)?;
        let rows = conn
            .query(
                LISTING_SQL,
                &[
                    &self.tenant,
                    &query.sees_all,
                    &query.viewer,
                    &query.also_cases,
                    &query.mine,
                    &process,
                    &status,
                    &after_at,
                    &after_id,
                    &fetch,
                ],
            )
            .await
            .map_err(error::pg)?;
        let mut items: Vec<(String, OffsetDateTime)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let case_id: String = row
                .try_get("case_id")
                .map_err(|e| error::corrupt(e.to_string()))?;
            let started_at: OffsetDateTime = row
                .try_get("started_at")
                .map_err(|e| error::corrupt(e.to_string()))?;
            items.push((case_id, started_at));
        }
        let page_size = limit as usize;
        let more = items.len() > page_size;
        items.truncate(page_size);
        let next = if more {
            items.last().map(|(case_id, started_at)| ListingCursor {
                started_at: *started_at,
                case_id: case_id.clone(),
            })
        } else {
            None
        };
        Ok(ListingPage {
            case_ids: items.into_iter().map(|(id, _)| id).collect(),
            next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_filter_uses_the_table_values() {
        for (filter, text) in [
            (CaseStatusFilter::Running, "running"),
            (CaseStatusFilter::Completed, "completed"),
            (CaseStatusFilter::Failed, "failed"),
            (CaseStatusFilter::Cancelled, "cancelled"),
        ] {
            assert_eq!(filter.as_str(), text);
        }
    }

    #[test]
    fn the_statement_binds_every_input_and_interpolates_nothing() {
        for n in 1..=10 {
            assert!(LISTING_SQL.contains(&format!("${n}")), "${n}");
        }
        assert!(!LISTING_SQL.contains("{}"));
        assert!(LISTING_SQL.contains("tenant = $1"));
    }
}
