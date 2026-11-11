//! Long operations: enqueue a job and return its id, and read a job's status.
//!
//! A tool declared asynchronous (an import, a report, a bulk update, or a whitelisted function
//! with no HTTP verbs) never runs inside the call. It enqueues a job through [`spot_jobs`] and
//! returns `{ "job_id": … }`; the model polls the `job_status` tool. No call holds a
//! connection or a transaction across the enqueue.

use serde_json::{Value as Json, json};
use spot_app_api::prelude::SiteId;
use spot_jobs::model::InsertResult;
use spot_jobs::{JobId, JobState, JobStore, NewJob};
use time::OffsetDateTime;

use crate::error::McpError;

/// The job name every asynchronous MCP tool runs under. The host registers one handler for it,
/// which reads `tool` and `arguments` from the job's args.
pub const JOB_NAME: &str = "mcp:tool";

/// The queue asynchronous tools run on.
pub const JOB_QUEUE: &str = "default";

/// Enqueues a job for an asynchronous tool and returns its id.
///
/// # Errors
///
/// [`McpError::internal`] when the store fails.
pub async fn enqueue(
    store: &dyn JobStore,
    site: &SiteId,
    tool: &str,
    arguments: &Json,
    now: OffsetDateTime,
    idempotency_key: Option<String>,
) -> Result<u64, McpError> {
    let args = json!({ "tool": tool, "arguments": arguments });
    let mut job = NewJob::new(
        JOB_NAME.to_owned(),
        args,
        JOB_QUEUE.to_owned(),
        site.clone(),
        now,
    );
    if let Some(key) = idempotency_key {
        job = job.with_idempotency_key(key);
    }
    let result = store
        .insert_if_new(job)
        .await
        .map_err(|_| McpError::internal("could not enqueue the job"))?;
    Ok(match result {
        InsertResult::Inserted(id) | InsertResult::Duplicate(id) => id.0,
    })
}

/// One job's status, as the `job_status` tool returns it.
#[derive(Clone, Debug, PartialEq)]
pub struct JobView {
    /// The job id.
    pub id: u64,
    /// The state, as its stable text form.
    pub state: String,
    /// The current attempt number.
    pub attempt: u32,
    /// The maximum attempts.
    pub max_attempts: u32,
    /// The last error message, when there is one.
    pub last_error: Option<String>,
}

impl JobView {
    /// The view as JSON.
    #[must_use]
    pub fn to_json(&self) -> Json {
        json!({
            "job_id": self.id,
            "state": self.state,
            "attempt": self.attempt,
            "max_attempts": self.max_attempts,
            "last_error": self.last_error,
        })
    }
}

/// Reads one job's status.
///
/// # Errors
///
/// [`McpError::job_not_found`] when the job does not exist, [`McpError::internal`] on a store
/// failure.
pub async fn status(store: &dyn JobStore, id: u64) -> Result<JobView, McpError> {
    let job = store
        .get(JobId(id))
        .await
        .map_err(|_| McpError::internal("could not read the job"))?
        .ok_or_else(|| McpError::job_not_found(id))?;
    Ok(JobView {
        id: job.id.0,
        state: state_name(&job.state).to_owned(),
        attempt: job.attempt,
        max_attempts: job.max_attempts,
        last_error: job.last_error,
    })
}

fn state_name(state: &JobState) -> &'static str {
    match state {
        JobState::Pending => "pending",
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Retrying { .. } => "retrying",
        JobState::Dead => "dead",
        JobState::Cancelled => "cancelled",
    }
}
