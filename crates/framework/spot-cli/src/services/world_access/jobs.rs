//! The jobs part of the world (WP-AI-07.T6): the real [`JobOps`] over the durable job store.
//!
//! # Where the store comes from
//!
//! The jobs module does not provide its store as a port (it provides `JobSubmit`, which cannot
//! list or read). So the part opens the **same durable store** the way the MCP service does
//! (`PgJobStore` over the site's own pool): through the store's own trait, never a table. On the
//! in-memory job profile (`services.jobs.store = "memory"`) the store is private to the jobs
//! module and the site has no jobs part (`from_site` answers `None`; readiness says so).
//!
//! # What each operation does
//!
//! - `failed` lists the site's **dead** jobs (permanently failed), `running` the running ones.
//!   A job waiting for its next attempt (`retrying`) or pending is *queued*; a succeeded or
//!   cancelled one is *done*.
//! - `job` reads one job of this site (a job of another site answers `None`).
//! - `unstick` moves a running job whose lease is **expired by the injected clock** back to the
//!   queue through the store's own `reap_expired`. That operation reaps every job whose lease
//!   lapsed, not only the named one: they are exactly the jobs the jobs module's reaper takes every
//!   two seconds, so the effect is the same, only sooner. The change returned is the named job's.
//! - `retry` and `restore` are **not supported by the job port**: the store offers no operation
//!   that moves a dead job back to the queue or a queued job back to dead or running. They answer
//!   `OpError` "not supported by the jobs port" (a `restore` to the state the job already has is
//!   accepted: it is a no-op). See the report's open questions for the operations to add.
//! - `rehearsal` reads the dead and running jobs of the site (at most [`SNAPSHOT_LIMIT`] of each)
//!   into the in-memory fixture, so a rehearsal can retry or unstick them and be thrown away.
//!
//! The ops are synchronous and run on a blocking thread: they block on the runtime handle the part
//! captured when the site was built (`Handle::block_on`, never `block_in_place`). No argument,
//! context or error text of a job is read into an error or a log line: ids and state names only.

use std::sync::Arc;

use spot_core::SiteId;
use spot_jobs::JobStore;
use spot_jobs::model::{Job, JobId, JobState};
use spot_jobs::store::JobFilter;
use spot_remedies::fixture::FakeJobs;
use spot_remedies::{JobChange, JobLife, JobOps, JobRef, JobView, OpError};
use time::OffsetDateTime;
use tokio::runtime::Handle;

use super::parts::{JobsPart, capture_runtime};
use crate::services::SiteCtx;
use crate::services::remedies::world::WorldError;

/// The most jobs of one state a listing or a snapshot reads.
pub const SNAPSHOT_LIMIT: u32 = 500;

/// The detail of an operation the job store has no operation for.
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

/// The jobs part over a job store.
pub struct StoreJobsPart {
    store: Arc<dyn JobStore>,
    site: SiteId,
    handle: Handle,
}

impl StoreJobsPart {
    /// A part over `store` for `site`. `handle` is the runtime the ops block on. The part reads no
    /// clock: the instant of an operation is the `now` the caller passes (the injected clock's).
    #[must_use]
    pub fn new(store: Arc<dyn JobStore>, site: SiteId, handle: Handle) -> Self {
        Self {
            store,
            site,
            handle,
        }
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

/// The jobs part for this site, or `None` when the site has no durable job store.
pub(super) fn from_site(ctx: &SiteCtx<'_>) -> Option<Arc<dyn JobsPart>> {
    let env = *ctx.env().ok()?;
    let store: Option<Arc<dyn JobStore>> = if env.loaded.common.services.jobs.store == "postgres" {
        Some(Arc::new(spot_jobs_pg::PgJobStore::new(env.backend.pool())))
    } else {
        None
    };
    from_parts(store, &env.site.site, capture_runtime())
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
                    job.claim
                        .as_ref()
                        .map(|claim| claim.expires_at.unix_timestamp()),
                );
            }
        }
        Ok(Box::new(fake))
    }
}

impl StoreJobsPart {
    /// This site's jobs in `state`, at most [`SNAPSHOT_LIMIT`].
    fn list(&self, state: JobState) -> Result<Vec<Job>, ()> {
        let filter = JobFilter {
            site: Some(self.site.clone()),
            state: Some(state),
            queue: None,
            limit: Some(SNAPSHOT_LIMIT),
        };
        self.handle
            .block_on(self.store.list(filter))
            .map_err(|_| ())
    }

    /// One job of this site.
    fn get(&self, id: &JobRef) -> Result<Option<Job>, ()> {
        let Ok(number) = id.as_str().parse::<u64>() else {
            return Ok(None);
        };
        let job = self
            .handle
            .block_on(self.store.get(JobId(number)))
            .map_err(|_| ())?;
        Ok(job.filter(|job| job.site == self.site))
    }
}

/// The live ops: every call goes to the job store.
struct StoreJobOps<'a> {
    part: &'a StoreJobsPart,
}

fn view_of(job: &Job) -> JobView {
    JobView {
        id: JobRef::new(&job.id.to_string()),
        life: life_of(&job.state),
        attempts: job.attempt,
        lease_expires_at: job
            .claim
            .as_ref()
            .map(|claim| claim.expires_at.unix_timestamp()),
    }
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

    fn retry(&mut self, _id: &JobRef, _now: i64) -> Result<JobChange, OpError> {
        Err(OpError::new("job_retry", NOT_SUPPORTED))
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
        match job
            .claim
            .as_ref()
            .map(|claim| claim.expires_at.unix_timestamp())
        {
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
            .block_on(self.part.store.reap_expired(at))
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
        Err(OpError::new(op, NOT_SUPPORTED))
    }
}
