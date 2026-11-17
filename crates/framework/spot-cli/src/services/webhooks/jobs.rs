//! A job store that only creates jobs, for the delivery handler's retry-after successor
//! (WP-INT-01.T2).

use super::*;

/// A [`JobStore`] that only creates jobs, through the site's [`JobSubmit`] port. The handler
/// needs it to schedule a retry-after successor; it must not read or claim anything.
pub struct SubmitOnlyJobs(pub Arc<dyn JobSubmit>);

fn refused<T>() -> std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<T>> + Send>>
{
    Box::pin(async { Err(spot_jobs::Error::other("job.operation_refused")) })
}

impl JobStore for SubmitOnlyJobs {
    fn insert_if_new(
        &self,
        job: NewJob,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = spot_jobs::Result<InsertResult>> + Send + '_>,
    > {
        Box::pin(async move {
            let spec = JobSpec {
                name: job.name,
                queue: job.queue,
                args: job.args,
                idempotency_key: job.idempotency_key.unwrap_or_default(),
                not_before: Some(job.not_before),
            };
            match self.0.submit(spec).await {
                Ok(true) => Ok(InsertResult::Inserted(JobId(0))),
                Ok(false) => Ok(InsertResult::Duplicate(JobId(0))),
                Err(_) => Err(spot_jobs::Error::other("job.submit_failed")),
            }
        })
    }
    fn claim(
        &self,
        _: &[String],
        _: &str,
        _: Duration,
        _: OffsetDateTime,
        _: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = spot_jobs::Result<Vec<ClaimedJob>>> + Send + '_>,
    > {
        refused()
    }
    fn heartbeat(
        &self,
        _: &spot_jobs::Claim,
        _: Duration,
        _: OffsetDateTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<()>> + Send + '_>>
    {
        refused()
    }
    fn complete(
        &self,
        _: &spot_jobs::Claim,
        _: spot_jobs::JobState,
        _: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<()>> + Send + '_>>
    {
        refused()
    }
    fn reap_expired(
        &self,
        _: OffsetDateTime,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = spot_jobs::Result<Vec<JobId>>> + Send + '_>,
    > {
        refused()
    }
    fn cancel(
        &self,
        _: JobId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<()>> + Send + '_>>
    {
        refused()
    }
    fn merge_context(
        &self,
        _: JobId,
        _: &spot_jobs::Claim,
        _: serde_json::Map<String, Json>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<Json>> + Send + '_>>
    {
        refused()
    }
    fn get(
        &self,
        _: JobId,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = spot_jobs::Result<Option<Job>>> + Send + '_>,
    > {
        refused()
    }
    fn list(
        &self,
        _: JobFilter,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<Vec<Job>>> + Send + '_>>
    {
        refused()
    }
    fn intake_position(
        &self,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = spot_jobs::Result<FeedPosition>> + Send + '_>,
    > {
        refused()
    }
    fn set_intake_position(
        &self,
        _: FeedPosition,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<()>> + Send + '_>>
    {
        refused()
    }
}
