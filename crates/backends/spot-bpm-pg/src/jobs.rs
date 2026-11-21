//! The jobs adapter: turning a Case's next step into a durable `spot-jobs-pg` job.
//!
//! [`PgCaseJobs::enqueue`] takes the [`CaseJob`] the engine's jobs port describes and stores it
//! through [`PgJobStore`], so a step survives a crash and a second enqueue of the same Case is
//! dropped by the job engine's idempotency key (`bpm:case:<case>`).
//!
//! The engine's own [`CaseJobQueue`](spot_bpm_engine::CaseJobQueue) port is synchronous (the
//! reference queue records in memory), while the durable store is asynchronous, so this adapter
//! is an `async fn` a host awaits after a commit rather than an implementation of that port. A
//! host that wants the port's shape wraps this call; the durable path never blocks.

use std::sync::Arc;

use spot_bpm_engine::{CaseJob, EngineError, EngineResult, to_new_job};
use spot_jobs::JobStore;
use spot_jobs::model::InsertResult;
use spot_jobs_pg::PgJobStore;

/// Enqueues Case steps as durable jobs.
#[derive(Clone)]
pub struct PgCaseJobs {
    store: Arc<PgJobStore>,
}

impl std::fmt::Debug for PgCaseJobs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCaseJobs").finish_non_exhaustive()
    }
}

impl PgCaseJobs {
    /// An adapter over the job store.
    #[must_use]
    pub fn new(store: Arc<PgJobStore>) -> Self {
        Self { store }
    }

    /// Enqueues one Case step.
    ///
    /// The job carries the Case's idempotency key, so enqueuing the same Case twice is a
    /// [`Duplicate`](InsertResult::Duplicate), never a second step.
    ///
    /// # Errors
    /// [`EngineError::StoreFailed`] when the job store cannot insert the job.
    pub async fn enqueue(&self, job: &CaseJob) -> EngineResult<InsertResult> {
        let new_job = to_new_job(job);
        self.store
            .insert_if_new(new_job)
            .await
            .map_err(|error| EngineError::StoreFailed {
                detail: format!("the case step could not be enqueued: {error}"),
            })
    }
}
