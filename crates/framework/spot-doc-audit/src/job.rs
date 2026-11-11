//! Running a lane appender as a fenced job: one active appender per lane.
//!
//! [`LaneAppender::drain`](crate::LaneAppender::drain) is a plain async method so a host can drive
//! it however it likes. On the platform it runs as a [`spot_jobs`] handler: the job engine claims
//! the job under a **fenced lease**, heartbeats it while it runs, and lets the lease lapse when the
//! worker crashes (ADR-056, the job engine). A lane is therefore chained by exactly one appender
//! at a time, and a crashed appender's lane is picked up again after the lease lapses — the append
//! itself is idempotent, so a redelivered event is chained once.
//!
//! One job name per lane ([`append_job_name`]) means the lease is per lane and no lane's lease
//! blocks another's. The job's arguments name the lane as `{"lane": <n>}` for the host that
//! enqueues it, and [`job_lane`] reads it back.

use std::sync::Arc;

use spot_audit_chain::AuditStore;
use spot_jobs::Job;
use spot_jobs::worker::{HandlerResult, JobContext, JobHandlerRegistry};

use crate::appender::{LaneAppender, OutboxFeed};

/// The job-name prefix of a lane's appender: `platform:doc_audit_append_<lane>`.
pub const AUDIT_APPEND_JOB_PREFIX: &str = "platform:doc_audit_append";

/// The job name of lane `lane`'s appender.
#[must_use]
pub fn append_job_name(lane: u32) -> String {
    format!("{AUDIT_APPEND_JOB_PREFIX}_{lane}")
}

/// The lane a job's arguments name as `{"lane": <n>}`, when they name one.
#[must_use]
pub fn job_lane(job: &Job) -> Option<u32> {
    job.args
        .get("lane")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Registers `appender` under its lane's job name: the handler drains the lane from `feed` in
/// batches of `limit`, completing the job on success and retrying it on a drain failure. The
/// handler holds only shared handles, so it is safe to register once and run on any worker.
///
/// The caller enqueues one recurring job per lane (see [`append_job_name`] and [`job_lane`]); the
/// engine's fenced lease keeps one appender active per lane.
pub fn register_appender<S>(
    registry: &mut JobHandlerRegistry,
    appender: Arc<LaneAppender<S>>,
    feed: Arc<dyn OutboxFeed>,
    limit: u32,
) where
    S: AuditStore + 'static,
{
    let name = append_job_name(appender.lane());
    registry.register(name, move |_ctx: JobContext| {
        let appender = Arc::clone(&appender);
        let feed = Arc::clone(&feed);
        Box::pin(async move {
            match appender.drain(&*feed, limit).await {
                Ok(_report) => Ok(HandlerResult::Ok),
                Err(error) => Ok(HandlerResult::Retry(error.code().to_owned())),
            }
        })
    });
}
