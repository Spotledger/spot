//! The jobs part of the world (WP-AI-07.T6): the real [`JobOps`] over the durable job store.
//!
//! # Where the store comes from
//!
//! The part works through the [`JobAdmin`] port the jobs module provides (`ctx.port::<dyn
//! JobAdmin>()`), built from the site's own store on **both** job profiles (PostgreSQL and the
//! in-memory one), so the part no longer opens a store of its own. [`from_parts`] and
//! [`StoreJobsPart::new`] build the same port over a store, for tests.
//!
//! # What each operation does
//!
//! - `failed` lists the site's **dead** jobs (permanently failed), `running` the running ones.
//!   A job waiting for its next attempt (`retrying`) or pending is *queued*; a succeeded or
//!   cancelled one is *done*.
//! - `job` reads one job of this site (a job of another site answers `None`).
//! - `unstick` moves a running job whose lease is **expired by the injected clock** back to the
//!   queue through the port's `reap_expired`. That operation reaps every job whose lease
//!   lapsed, not only the named one: they are exactly the jobs the jobs module's reaper takes every
//!   two seconds, so the effect is the same, only sooner. The change returned is the named job's.
//! - `retry` moves a **dead** job back to the queue through the port's `requeue` (pending, due
//!   now, attempts reset, last error kept) and returns the change from `failed` to `queued`. A job
//!   that is not dead, or whose idempotency key another live job holds, is refused.
//! - `restore` is the undo of a retry: `restore(job, failed)`. The store cannot put a requeued
//!   job back to `dead` (that state is only reached by a worker running out of attempts), so the
//!   undo is **not exact**: it cancels the requeued job, but only while it is still pending and
//!   unclaimed (the port's `cancel_unclaimed`, one atomic `JobStore::cancel_if_pending`: a worker
//!   that claims at the same moment is never cancelled), which returns it to a terminal state without
//!   running again. The job ends `cancelled` (a *done* job), not `failed`, and its error history is
//!   kept. A job a worker already claimed or finished is refused ("the job already ran again"); a
//!   job that is already cancelled is the undone state (a no-op); a restore to the state the job
//!   already has is a no-op. A restore to any other state (the undo of `unstick`) is not supported
//!   by the jobs port.
//! - `rehearsal` reads the dead and running jobs of the site (at most [`SNAPSHOT_LIMIT`] of each)
//!   into the in-memory fixture, so a rehearsal can retry or unstick them and be thrown away.
//!
//! The ops are synchronous and run on a blocking thread: they block on the runtime handle the part
//! captured when the site was built (`Handle::block_on`, never `block_in_place`). No argument,
//! context or error text of a job is read into an error or a log line: ids and state names only.

use std::sync::Arc;

use spot_core::SiteId;
use spot_jobs::JobStore;
use spot_jobs::model::{JobId, JobState};
use spot_remedies::fixture::FakeJobs;
use spot_remedies::{JobChange, JobLife, JobOps, JobRef, JobView, OpError};
use time::OffsetDateTime;
use tokio::runtime::Handle;

use super::parts::{JobsPart, capture_runtime};
use crate::services::SiteCtx;
use crate::services::ports::{JobAdmin, JobSummary, PortError, StopGate, StoreJobAdmin};
use crate::services::remedies::world::WorldError;

/// The most jobs of one state a listing or a snapshot reads.
pub const SNAPSHOT_LIMIT: u32 = 500;

/// The detail of an operation the jobs port has no operation for.
const NOT_SUPPORTED: &str = "not supported by the jobs port";

/// The state of a job as the remedy ports name it.
#[must_use]
pub fn life_of(state: &JobState) -> JobLife {
    match state {
        JobState::Pending | JobState::Retrying { .. } => JobLife::Queued,
        JobState::Running => JobLife::Running,
        JobState::Succeeded | JobState::Cancelled => JobLife::Done,
        JobState::Dead => JobLife::Failed,
    }
}

/// What the part says when a restore finds the job past the requeue it would undo.
const RAN_AGAIN: &str = "the job already ran again";

/// The jobs part over a [`JobAdmin`] port.
pub struct StoreJobsPart {
    admin: Arc<dyn JobAdmin>,
    handle: Handle,
}

impl StoreJobsPart {
    /// A part over `store` for `site`: the same admin port the jobs module provides, built over
    /// `store` with an open stop gate (a test or an embedding without a site). `handle` is the
    /// runtime the ops block on. The part reads no clock: the instant of an operation is the `now`
    /// the caller passes (the injected clock's).
    #[must_use]
    pub fn new(store: Arc<dyn JobStore>, site: SiteId, handle: Handle) -> Self {
        Self::over_admin(
            Arc::new(StoreJobAdmin::new(site, store, StopGate::new())),
            handle,
        )
    }

    /// A part over the site's job admin port.
    #[must_use]
    pub fn over_admin(admin: Arc<dyn JobAdmin>, handle: Handle) -> Self {
        Self { admin, handle }
    }

    fn ops(&self) -> StoreJobOps<'_> {
        StoreJobOps { part: self }
    }
}

/// The part for a site, or `None` when there is no job store, the site name is invalid or no
/// runtime is available to block on.
#[must_use]
pub fn from_parts(
    store: Option<Arc<dyn JobStore>>,
    site: &str,
    handle: Option<Handle>,
) -> Option<Arc<dyn JobsPart>> {
    let store = store?;
    let site = SiteId::new(site).ok()?;
    let handle = handle?;
    Some(Arc::new(StoreJobsPart::new(store, site, handle)))
}

/// The part over a job admin port, or `None` when there is no port or no runtime to block on.
#[must_use]
pub fn from_admin(
    admin: Option<Arc<dyn JobAdmin>>,
    handle: Option<Handle>,
) -> Option<Arc<dyn JobsPart>> {
    Some(Arc::new(StoreJobsPart::over_admin(admin?, handle?)))
}

/// The jobs part for this site, or `None` when the jobs module did not provide its admin port.
pub(super) fn from_site(ctx: &SiteCtx<'_>) -> Option<Arc<dyn JobsPart>> {
    from_admin(ctx.port::<dyn JobAdmin>(), capture_runtime())
}

impl JobsPart for StoreJobsPart {
    fn live(&self) -> Box<dyn JobOps + '_> {
        Box::new(self.ops())
    }

    fn rehearsal(&self) -> Result<Box<dyn JobOps>, WorldError> {
        let mut fake = FakeJobs::new();
        for (state, life) in [
            (JobState::Dead, JobLife::Failed),
            (JobState::Running, JobLife::Running),
        ] {
            let jobs = self
                .list(state)
                .map_err(|_| super::parts::PartName::Jobs.rehearsal_unavailable())?;
            for job in jobs {
                fake.seed(
                    &job.id.to_string(),
                    life,
                    job.attempt,
                    job.lease_expires_at.map(OffsetDateTime::unix_timestamp),
                );
            }
        }
        Ok(Box::new(fake))
    }
}

impl StoreJobsPart {
    /// This site's jobs in `state`, at most [`SNAPSHOT_LIMIT`].
    fn list(&self, state: JobState) -> Result<Vec<JobSummary>, ()> {
        self.handle
            .block_on(self.admin.list(Some(state), SNAPSHOT_LIMIT))
            .map_err(|_| ())
    }

    /// One job of this site.
    fn get(&self, id: &JobRef) -> Result<Option<JobSummary>, ()> {
        let Ok(number) = id.as_str().parse::<u64>() else {
            return Ok(None);
        };
        self.handle
            .block_on(self.admin.get(JobId(number)))
            .map_err(|_| ())
    }
}

/// The live ops: every call goes to the job store.
struct StoreJobOps<'a> {
    part: &'a StoreJobsPart,
}

fn view_of(job: &JobSummary) -> JobView {
    JobView {
        id: JobRef::new(&job.id.to_string()),
        life: life_of(&job.state),
        attempts: job.attempt,
        lease_expires_at: job.lease_expires_at.map(OffsetDateTime::unix_timestamp),
    }
}

/// The text an operator sees for a refused or failed port call. A [`PortError`] never carries a
/// payload, a key or an argument, so its text is safe to show.
fn port_detail(error: PortError) -> String {
    error.to_string()
}

impl JobOps for StoreJobOps<'_> {
    fn failed(&self) -> Vec<JobRef> {
        self.part
            .list(JobState::Dead)
            .map(|jobs| {
                jobs.iter()
                    .map(|job| JobRef::new(&job.id.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn running(&self) -> Vec<JobRef> {
        self.part
            .list(JobState::Running)
            .map(|jobs| {
                jobs.iter()
                    .map(|job| JobRef::new(&job.id.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn job(&self, id: &JobRef) -> Option<JobView> {
        self.part.get(id).ok().flatten().as_ref().map(view_of)
    }

    fn retry(&mut self, id: &JobRef, now: i64) -> Result<JobChange, OpError> {
        let op = "job_retry";
        let job = self
            .part
            .get(id)
            .map_err(|()| OpError::new(op, "the job store did not answer"))?
            .ok_or_else(|| OpError::new(op, &format!("no job `{}`", id.as_str())))?;
        if life_of(&job.state) != JobLife::Failed {
            return Err(OpError::new(
                op,
                &format!(
                    "job `{}` is {}, not failed",
                    id.as_str(),
                    life_of(&job.state).as_str()
                ),
            ));
        }
        let at = OffsetDateTime::from_unix_timestamp(now)
            .map_err(|_| OpError::new(op, "the instant is out of range"))?;
        let after = self
            .part
            .handle
            .block_on(self.part.admin.requeue(job.id, at))
            .map_err(|error| OpError::new(op, &port_detail(error)))?;
        Ok(JobChange {
            id: id.clone(),
            before: JobLife::Failed,
            after: life_of(&after.state),
        })
    }

    fn unstick(&mut self, id: &JobRef, now: i64) -> Result<JobChange, OpError> {
        let op = "job_unstick";
        let job = self
            .part
            .get(id)
            .map_err(|()| OpError::new(op, "the job store did not answer"))?
            .ok_or_else(|| OpError::new(op, &format!("no job `{}`", id.as_str())))?;
        if life_of(&job.state) != JobLife::Running {
            return Err(OpError::new(
                op,
                &format!(
                    "job `{}` is {}, not running",
                    id.as_str(),
                    life_of(&job.state).as_str()
                ),
            ));
        }
        // Expiry is judged by the instant the caller passes (the injected clock's), the same
        // instant the reaper is asked about.
        match job.lease_expires_at.map(OffsetDateTime::unix_timestamp) {
            Some(expires) if expires <= now => {}
            _ => {
                return Err(OpError::new(
                    op,
                    &format!("the lease on job `{}` has not expired", id.as_str()),
                ));
            }
        }
        let at = OffsetDateTime::from_unix_timestamp(now)
            .map_err(|_| OpError::new(op, "the instant is out of range"))?;
        let reaped = self
            .part
            .handle
            .block_on(self.part.admin.reap_expired(at))
            .map_err(|_| OpError::new(op, "the job store did not answer"))?;
        if !reaped.contains(&job.id) {
            return Err(OpError::new(
                op,
                &format!("the job store did not release job `{}`", id.as_str()),
            ));
        }
        Ok(JobChange {
            id: id.clone(),
            before: JobLife::Running,
            after: JobLife::Queued,
        })
    }

    fn restore(&mut self, id: &JobRef, life: JobLife) -> Result<(), OpError> {
        let op = "job_restore";
        let job = self
            .part
            .get(id)
            .map_err(|()| OpError::new(op, "the job store did not answer"))?
            .ok_or_else(|| OpError::new(op, &format!("no job `{}`", id.as_str())))?;
        if life_of(&job.state) == life {
            return Ok(());
        }
        if life != JobLife::Failed {
            return Err(OpError::new(op, NOT_SUPPORTED));
        }
        // The undo of a retry. The job is only taken back while no worker has touched it: a job
        // already cancelled is the undone state; a pending job nobody holds is cancelled; anything
        // else (running, retrying after a run, succeeded) has already run again. The read above is
        // only a shortcut: the port's cancel decides atomically, so a worker that claimed the job
        // since is refused there (and reported as having run again) instead of being cancelled.
        match job.state {
            JobState::Cancelled => Ok(()),
            JobState::Pending if job.lease_expires_at.is_none() => self
                .part
                .handle
                .block_on(self.part.admin.cancel_unclaimed(job.id))
                .map(|_| ())
                .map_err(|error| match error {
                    PortError::Invalid(_) => OpError::new(op, RAN_AGAIN),
                    other => OpError::new(op, &port_detail(other)),
                }),
            _ => Err(OpError::new(op, RAN_AGAIN)),
        }
    }
}
