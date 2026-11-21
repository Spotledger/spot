//! The jobs adapter: running [`Engine::step`](crate::Engine::step) as a `spot-jobs` job per Case.
//!
//! The engine never depends on the job engine: a host turns a Case into a job through
//! [`CaseJobQueue`] and [`CaseJob`], and [`to_new_job`] is the pure adapter to `spot-jobs`'
//! [`NewJob`](spot_jobs::NewJob). The queue is a trait, so the tests run the engine without any
//! job store at all.

use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::prelude::{OffsetDateTime, SiteId};
use spot_jobs::NewJob;

use crate::error::EngineResult;
use crate::handler::CaseId;

/// The job name a Case step runs under.
pub const CASE_JOB_NAME: &str = "bpm:case";

/// One Case that is due a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseJob {
    /// The Case to step.
    pub case: CaseId,
    /// The site the Case belongs to.
    pub site: SiteId,
    /// The queue the step runs on.
    pub queue: String,
    /// The earliest instant the step may run.
    pub not_before: OffsetDateTime,
}

impl CaseJob {
    /// A job for a Case on the default queue.
    #[must_use]
    pub fn new(case: CaseId, site: SiteId, not_before: OffsetDateTime) -> Self {
        Self {
            case,
            site,
            queue: "bpm".to_owned(),
            not_before,
        }
    }

    /// The idempotency key that makes a double enqueue harmless.
    #[must_use]
    pub fn idempotency_key(&self) -> String {
        format!("{CASE_JOB_NAME}:{}", self.case)
    }
}

/// Where a host enqueues a Case step.
pub trait CaseJobQueue: Send + Sync {
    /// Enqueues one Case step.
    ///
    /// # Errors
    /// A store failure from the host's job engine.
    fn enqueue(&self, job: CaseJob) -> EngineResult<()>;
}

/// A [`CaseJobQueue`] that records the jobs it is given, for tests without a job store.
#[derive(Debug, Clone, Default)]
pub struct RecordingQueue {
    jobs: Arc<Mutex<Vec<CaseJob>>>,
}

impl RecordingQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<CaseJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The jobs enqueued so far.
    #[must_use]
    pub fn jobs(&self) -> Vec<CaseJob> {
        self.lock().clone()
    }

    /// How many jobs were enqueued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether nothing was enqueued.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

impl CaseJobQueue for RecordingQueue {
    fn enqueue(&self, job: CaseJob) -> EngineResult<()> {
        self.lock().push(job);
        Ok(())
    }
}

/// The pure adapter: a [`CaseJob`] as the `spot-jobs` job a worker claims.
#[must_use]
pub fn to_new_job(job: &CaseJob) -> NewJob {
    NewJob::new(
        CASE_JOB_NAME.to_owned(),
        serde_json::json!({ "case": job.case.as_str() }),
        job.queue.clone(),
        job.site.clone(),
        job.not_before,
    )
    .with_idempotency_key(job.idempotency_key())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_case_job_maps_to_a_deduplicated_new_job() {
        let case = CaseId::new("case1").unwrap();
        let site = SiteId::new("site-1").unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let job = CaseJob::new(case, site, now);
        let new_job = to_new_job(&job);
        assert_eq!(new_job.name, CASE_JOB_NAME);
        assert_eq!(new_job.idempotency_key.as_deref(), Some("bpm:case:case1"));
        assert_eq!(new_job.not_before, now);
    }

    #[test]
    fn the_recording_queue_keeps_jobs() {
        let queue = RecordingQueue::new();
        let case = CaseId::new("case1").unwrap();
        let site = SiteId::new("site-1").unwrap();
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        queue.enqueue(CaseJob::new(case, site, now)).unwrap();
        assert_eq!(queue.len(), 1);
    }
}
