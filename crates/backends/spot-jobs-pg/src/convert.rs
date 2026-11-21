//! Rows to jobs, and driver errors to job-engine errors.

use spot_core::SiteId;
use spot_jobs::{Claim, Error, Job, JobId, JobState, Result};
use spot_scheduler::lease::FencingToken;
use tokio_postgres::Row;

/// A driver error as a job-engine error; a database error keeps its SQLSTATE and message.
pub(crate) fn pg_err(e: tokio_postgres::Error) -> Error {
    match e.as_db_error() {
        Some(db) => Error::other(format!(
            "job store: {} ({})",
            db.message(),
            db.code().code()
        )),
        None => Error::other(format!("job store: {e}")),
    }
}

/// A pool error as a job-engine error.
pub(crate) fn pool_err(e: deadpool_postgres::PoolError) -> Error {
    Error::other(format!("job store: no connection: {e}"))
}

/// A job id from its column.
pub(crate) fn id_from_db(v: i64) -> Result<JobId> {
    u64::try_from(v)
        .map(JobId)
        .map_err(|_| Error::other("job store: negative job id"))
}

/// A job id for a parameter.
pub(crate) fn id_to_db(id: JobId) -> Result<i64> {
    i64::try_from(id.0).map_err(|_| Error::other("job store: job id does not fit the database"))
}

/// A duration in whole milliseconds for a parameter (saturating).
pub(crate) fn millis(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// The `state` column text of a state that can be stored by `complete`.
pub(crate) fn state_text(state: &JobState) -> &'static str {
    match state {
        JobState::Pending => "pending",
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Retrying { .. } => "retrying",
        JobState::Dead => "dead",
        JobState::Cancelled => "cancelled",
    }
}

/// Reads a job in [`JOB_COLUMNS`] order.
pub(crate) fn job_from_row(row: &Row) -> Result<Job> {
    let g = |e: tokio_postgres::Error| pg_err(e);
    let state_s: String = row.try_get("state").map_err(g)?;
    let not_before: time::OffsetDateTime = row.try_get("not_before").map_err(g)?;
    let state = match state_s.as_str() {
        "pending" => JobState::Pending,
        "running" => JobState::Running,
        "succeeded" => JobState::Succeeded,
        "retrying" => JobState::Retrying { at: not_before },
        "dead" => JobState::Dead,
        "cancelled" => JobState::Cancelled,
        other => return Err(Error::other(format!("job store: unknown state {other}"))),
    };
    let site: String = row.try_get("site").map_err(g)?;
    let token: Option<i64> = row.try_get("claim_token").map_err(g)?;
    let worker: Option<String> = row.try_get("claim_worker").map_err(g)?;
    let expires: Option<time::OffsetDateTime> = row.try_get("lease_expires_at").map_err(g)?;
    let claim = match (token, worker, expires) {
        (Some(t), Some(w), Some(e)) => Some(Claim::new(
            FencingToken(
                u64::try_from(t).map_err(|_| Error::other("job store: negative claim token"))?,
            ),
            w,
            e,
        )),
        _ => None,
    };
    let attempt: i32 = row.try_get("attempt").map_err(g)?;
    let max_attempts: i32 = row.try_get("max_attempts").map_err(g)?;
    Ok(Job {
        id: id_from_db(row.try_get("id").map_err(g)?)?,
        site: SiteId::new(site)
            .map_err(|e| Error::other(format!("job store: stored site: {e}")))?,
        name: row.try_get("name").map_err(g)?,
        args: row.try_get("args").map_err(g)?,
        queue: row.try_get("queue").map_err(g)?,
        state,
        attempt: u32::try_from(attempt).unwrap_or(0),
        max_attempts: u32::try_from(max_attempts).unwrap_or(0),
        not_before,
        idempotency_key: row.try_get("idempotency_key").map_err(g)?,
        claim,
        last_error: row.try_get("last_error").map_err(g)?,
        created_at: row.try_get("created_at").map_err(g)?,
        context: row.try_get("context").map_err(g)?,
    })
}
