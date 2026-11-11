//! The job intake: reading jobs from the outbox and enqueuing scheduled jobs.

use crate::model::NewJob;
use crate::store::JobStore;
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_core::SiteId;
use spot_scheduler::slot::RunSlot;
use spot_storage_contract::feed::{ChangeFeed, FeedPosition};
use time::OffsetDateTime;

/// The payload of a `sys_job_enqueued` outbox event (schema version 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEnqueued {
    /// The site ID that owns the job.
    pub site: String,
    /// The job name, app-qualified: `app:job`.
    pub job: String,
    /// Job arguments.
    pub args: Json,
    /// Idempotency key (optional).
    pub idempotency_key: Option<String>,
    /// Earliest start time (optional, ISO 8601 string).
    pub not_before: Option<String>,
    /// Queue name (optional; defaults to `default`).
    pub queue: Option<String>,
    /// Opaque context for extensions (optional).
    pub context: Option<Json>,
}

impl JobEnqueued {
    /// Validate the payload structure.
    pub fn validate(&self) -> crate::Result<()> {
        // Job name must be valid
        if !NewJob::validate_job_name(&self.job) {
            return Err(crate::error::Error::InvalidName);
        }
        Ok(())
    }
}

impl NewJob {
    /// Check if a job name is valid (app:job format).
    pub fn validate_job_name(name: &str) -> bool {
        spot_app_api::context::valid_job_name(name)
    }
}

/// What one intake batch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntakeReport {
    /// The position now stored.
    pub position: FeedPosition,
    /// Jobs created (a replayed event creates none).
    pub inserted: u32,
    /// Events that could not become jobs, with the reason; the operator sees them.
    pub rejected: Vec<RejectedEvent>,
}

/// An outbox event the intake could not turn into a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedEvent {
    /// The outbox event id.
    pub event_id: String,
    /// Why: `job.invalid_name`, `undecodable_payload`, `invalid_site` or
    /// `unsupported_schema_version`.
    pub reason: &'static str,
}

/// Process a batch of outbox events, reading sys_job_enqueued events and inserting them as jobs.
///
/// This function:
/// - Reads up to `batch_size` events from the feed starting at the stored position
/// - Filters for `sys_job_enqueued` events
/// - Decodes the payload as `JobEnqueued` v1
/// - Inserts jobs using the outbox event ID as an additional dedupe key
/// - Advances the position only after all jobs are successfully inserted
/// - Returns `job.intake_resync` if `ResyncRequired` is encountered
pub async fn process_batch<F: ChangeFeed + ?Sized>(
    store: &dyn JobStore,
    feed: &F,
    batch_size: u32,
    now: OffsetDateTime,
) -> Result<IntakeReport> {
    // Get the current position
    let current_position = store.intake_position().await?;

    // Read outbox events from the feed
    // `ResyncRequired` becomes `job.intake_resync` through `From<StorageError>`; the intake
    // stops for an operator and never guesses a new position.
    let records = feed.read_outbox(current_position, batch_size).await?;

    if records.is_empty() {
        return Ok(IntakeReport {
            position: current_position,
            inserted: 0,
            rejected: Vec::new(),
        });
    }

    // Process each record
    let mut last_position = current_position;
    let mut jobs_to_insert = Vec::new();
    let mut rejected = Vec::new();
    let mut reject = |record: &spot_storage_contract::feed::OutboxRecord, reason: &'static str| {
        rejected.push(RejectedEvent {
            event_id: record.event.id.to_string(),
            reason,
        });
    };

    for record in &records {
        // Update position to the latest record
        last_position = record.position;

        // Only process sys_job_enqueued events
        if record.event.topic.as_str() != crate::SYS_JOB_ENQUEUED_TOPIC {
            continue;
        }

        // A malformed event is never silently dropped: it is reported, and the position still
        // advances so one bad event cannot block the feed forever.
        if record.event.schema_version != 1 {
            reject(record, "unsupported_schema_version");
            continue;
        }
        let payload: JobEnqueued = match serde_json::from_str(&record.event.payload) {
            Ok(p) => p,
            Err(_) => {
                reject(record, "undecodable_payload");
                continue;
            }
        };
        if payload.validate().is_err() {
            reject(record, "job.invalid_name");
            continue;
        }
        let site = match SiteId::new(&payload.site) {
            Ok(s) => s,
            Err(_) => {
                reject(record, "invalid_site");
                continue;
            }
        };

        // Parse the not_before timestamp
        let not_before = if let Some(nb_str) = &payload.not_before {
            match time::OffsetDateTime::parse(
                nb_str,
                &time::format_description::well_known::Rfc3339,
            ) {
                Ok(dt) => dt,
                Err(_) => now,
            }
        } else {
            now
        };

        // Build the NewJob
        let mut new_job = NewJob::new(
            payload.job,
            payload.args,
            payload.queue.unwrap_or_else(|| "default".to_string()),
            site,
            not_before,
        );

        // Set idempotency key if provided
        if let Some(key) = payload.idempotency_key {
            new_job = new_job.with_idempotency_key(key);
        }

        // Set context if provided
        if let Some(ctx) = payload.context {
            new_job = new_job.with_context(ctx);
        }

        // Set the outbox event ID for deduplication
        new_job = new_job.with_outbox_event_id(record.event.id.to_string());

        jobs_to_insert.push(new_job);
    }

    let mut inserted = 0u32;
    for job in jobs_to_insert {
        if matches!(
            store.insert_if_new(job).await?,
            crate::model::InsertResult::Inserted(_)
        ) {
            inserted += 1;
        }
    }

    // Advance position only after all jobs are inserted
    store.set_intake_position(last_position).await?;

    Ok(IntakeReport {
        position: last_position,
        inserted,
        rejected,
    })
}

/// The argument name carrying the app step a scheduled job runs.
pub const ARG_STEP: &str = "step";

/// The argument name carrying the schedule's time budget in whole milliseconds.
pub const ARG_BUDGET_MS: &str = "budget_ms";

/// The queues a manifest schedule may name.
const SCHEDULE_QUEUES: [&str; 3] = ["default", "short", "long"];

/// Create a job from a scheduled run slot.
///
/// The idempotency key is the slot's `deduplication_key`, so two scheduler nodes ticking the
/// same slot create one job. The job lands on the slot's queue (`default`, `short` or `long`);
/// any other queue name is refused with [`Error::UnknownQueue`](crate::Error::UnknownQueue), it
/// is never replaced by `default`.
///
/// The arguments have a stable shape: `{"step": <manifest step>, "budget_ms": <u64>}` (see
/// [`ARG_STEP`] and [`ARG_BUDGET_MS`]). The worker reads the budget back through
/// [`JobContext::budget_ms`](crate::worker::JobContext::budget_ms) and cancels a handler that
/// runs past it.
pub fn enqueue_scheduled(slot: &RunSlot, site: &SiteId) -> Result<NewJob> {
    let queue = slot.slot_ref.queue.as_str();
    if !SCHEDULE_QUEUES.contains(&queue) {
        return Err(crate::error::Error::UnknownQueue);
    }
    let job_name = format!("{}:{}", slot.slot_ref.app, slot.slot_ref.schedule);
    // Convert jiff::Timestamp to time::OffsetDateTime
    let scheduled_at = time::OffsetDateTime::from_unix_timestamp(slot.scheduled_for.as_second())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);

    let mut args = serde_json::Map::new();
    args.insert(
        ARG_STEP.to_owned(),
        Json::String(slot.slot_ref.step.clone()),
    );
    args.insert(
        ARG_BUDGET_MS.to_owned(),
        Json::from(slot.slot_ref.budget_ms),
    );

    Ok(NewJob::new(
        job_name,
        Json::Object(args),
        queue.to_owned(),
        site.clone(),
        scheduled_at,
    )
    .with_idempotency_key(slot.deduplication_key.clone()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_job_enqueued_validation() {
        let payload = JobEnqueued {
            site: "test".to_string(),
            job: "app:test".to_string(),
            args: Json::Null,
            idempotency_key: None,
            not_before: None,
            queue: None,
            context: None,
        };

        assert!(payload.validate().is_ok());
    }

    #[test]
    fn test_job_enqueued_invalid_name() {
        let payload = JobEnqueued {
            site: "test".to_string(),
            job: "invalid".to_string(), // Missing colon
            args: Json::Null,
            idempotency_key: None,
            not_before: None,
            queue: None,
            context: None,
        };

        assert!(payload.validate().is_err());
    }

    fn slot_with(slot_ref: spot_scheduler::slot::SlotRef) -> RunSlot {
        RunSlot {
            schedule: slot_ref.schedule.clone(),
            scheduled_for: "2026-10-06T14:00:00Z".parse().unwrap(),
            deduplication_key: "app:schedule:2026-10-06T14:00:00Z".to_string(),
            slot_ref,
        }
    }

    #[test]
    fn old_slot_ref_still_enqueues_on_default() {
        let slot = slot_with(spot_scheduler::slot::SlotRef::new("app", "schedule"));
        let site = SiteId::new("test-site").unwrap();
        let job = enqueue_scheduled(&slot, &site).unwrap();

        assert_eq!(job.site, site);
        assert_eq!(job.queue, "default");
        assert_eq!(job.name, "app:schedule");
        assert_eq!(
            job.idempotency_key.as_ref().unwrap(),
            &slot.deduplication_key
        );
    }

    #[test]
    fn short_long_and_default_schedules_land_on_their_queues() {
        let site = SiteId::new("test-site").unwrap();
        for queue in ["default", "short", "long"] {
            let mut r = spot_scheduler::slot::SlotRef::new("app", "schedule");
            r.queue = queue.to_string();
            let job = enqueue_scheduled(&slot_with(r), &site).unwrap();
            assert_eq!(job.queue, queue);
        }
    }

    #[test]
    fn an_unknown_queue_is_refused_not_defaulted() {
        let site = SiteId::new("test-site").unwrap();
        for queue in ["urgent", "", "Short", "default "] {
            let mut r = spot_scheduler::slot::SlotRef::new("app", "schedule");
            r.queue = queue.to_string();
            assert_eq!(
                enqueue_scheduled(&slot_with(r), &site).unwrap_err(),
                crate::error::Error::UnknownQueue,
                "queue {queue:?}"
            );
        }
    }

    #[test]
    fn the_args_shape_is_pinned() {
        let site = SiteId::new("test-site").unwrap();
        let mut r = spot_scheduler::slot::SlotRef::new("app", "schedule");
        r.step = "app:close_books".to_string();
        r.budget_ms = 4_500;
        let job = enqueue_scheduled(&slot_with(r), &site).unwrap();
        assert_eq!(
            job.args,
            serde_json::json!({"step": "app:close_books", "budget_ms": 4500})
        );
        assert_eq!((ARG_STEP, ARG_BUDGET_MS), ("step", "budget_ms"));
    }

    #[tokio::test]
    async fn two_scheduler_nodes_ticking_one_slot_create_one_job() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let mut r = spot_scheduler::slot::SlotRef::new("app", "schedule");
        r.queue = "short".to_string();
        let slot = slot_with(r);
        let first = store
            .insert_if_new(enqueue_scheduled(&slot, &site).unwrap())
            .await
            .unwrap();
        let second = store
            .insert_if_new(enqueue_scheduled(&slot, &site).unwrap())
            .await
            .unwrap();
        assert!(matches!(first, crate::model::InsertResult::Inserted(_)));
        assert!(matches!(second, crate::model::InsertResult::Duplicate(_)));
        assert_eq!(job_count(&store).await, 1);
    }

    use crate::store::{InMemoryJobStore, JobFilter};
    use spot_storage_contract::contract_tests;
    use spot_storage_contract::*;
    use spot_storage_fake::FakeBackend;
    use std::sync::Arc;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    async fn backend() -> (FakeBackend, Arc<Catalog>) {
        let b = FakeBackend::new();
        let cat = Arc::new(contract_tests::catalog().unwrap());
        b.prepare(&cat).await.unwrap();
        (b, cat)
    }

    fn enqueued_event(key: &str) -> OutboxEvent {
        let payload = JobEnqueued {
            site: "test-site".into(),
            job: "app:send".into(),
            args: serde_json::json!({"k": key}),
            idempotency_key: Some(key.into()),
            not_before: None,
            queue: None,
            context: None,
        };
        OutboxEvent {
            id: RecordId::new_v7(),
            topic: name(crate::SYS_JOB_ENQUEUED_TOPIC),
            subject: NodeRef {
                entity: name("party"),
                id: RecordId::new_v7(),
            },
            subject_version: None,
            schema_version: 1,
            payload: serde_json::to_string(&payload).unwrap(),
        }
    }

    async fn begin(b: &FakeBackend, cat: &Arc<Catalog>) -> <FakeBackend as StorageBackend>::Tx {
        b.begin(
            cat.clone(),
            TxOptions::new(name("intake_test"), Isolation::ReadCommitted),
        )
        .await
        .unwrap()
    }

    async fn job_count(store: &InMemoryJobStore) -> usize {
        store
            .list(JobFilter {
                site: None,
                state: None,
                queue: None,
                limit: None,
            })
            .await
            .unwrap()
            .len()
    }

    fn fixed_now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    /// An event enqueued in a rolled-back transaction never reaches the feed, so the intake
    /// creates no job; the same event in a committed transaction creates exactly one.
    #[tokio::test]
    async fn rolled_back_transaction_creates_no_job() {
        let (b, cat) = backend().await;
        let store = InMemoryJobStore::new();

        let mut tx = begin(&b, &cat).await;
        tx.enqueue(&enqueued_event("rolled-back")).await.unwrap();
        tx.rollback().await.unwrap();
        process_batch(&store, &b, 100, fixed_now()).await.unwrap();
        assert_eq!(
            job_count(&store).await,
            0,
            "a rollback must not create a job"
        );

        let mut tx = begin(&b, &cat).await;
        tx.enqueue(&enqueued_event("committed")).await.unwrap();
        tx.commit().await.unwrap();
        process_batch(&store, &b, 100, fixed_now()).await.unwrap();
        assert_eq!(
            job_count(&store).await,
            1,
            "a commit creates exactly one job"
        );
    }

    /// A restart before the position was saved replays the same events: still one job each.
    #[tokio::test]
    async fn intake_replay_no_duplicates() {
        let (b, cat) = backend().await;
        let store = InMemoryJobStore::new();

        let mut tx = begin(&b, &cat).await;
        tx.enqueue(&enqueued_event("a")).await.unwrap();
        tx.enqueue(&enqueued_event("b")).await.unwrap();
        tx.commit().await.unwrap();

        let end = process_batch(&store, &b, 100, fixed_now())
            .await
            .unwrap()
            .position;
        assert_eq!(job_count(&store).await, 2);
        assert_ne!(
            end,
            FeedPosition::START,
            "position advances after the batch"
        );

        // Simulate the crash: the saved position is lost, the same events are read again.
        store
            .set_intake_position(FeedPosition::START)
            .await
            .unwrap();
        process_batch(&store, &b, 100, fixed_now()).await.unwrap();
        assert_eq!(
            job_count(&store).await,
            2,
            "replay must not create second jobs"
        );
    }

    /// A position the feed no longer accepts stops the intake with `job.intake_resync`.
    #[tokio::test]
    async fn resync_required_stops_the_intake() {
        let (b, _cat) = backend().await;
        let store = InMemoryJobStore::new();
        // A position from another stream generation can never be served: resync.
        let bogus = FeedPosition::new(9_999, 42);
        store.set_intake_position(bogus).await.unwrap();
        let err = process_batch(&store, &b, 10, fixed_now())
            .await
            .unwrap_err();
        assert!(matches!(err, crate::error::Error::IntakeResync), "{err:?}");
    }

    /// A malformed event is reported, not dropped silently, and does not block later events.
    #[tokio::test]
    async fn malformed_events_are_reported_and_do_not_block_the_feed() {
        let (b, cat) = backend().await;
        let store = InMemoryJobStore::new();
        let mut bad = enqueued_event("bad");
        bad.payload = r#"{"unexpected": 1}"#.into();
        let mut wrong_version = enqueued_event("v2");
        wrong_version.schema_version = 2;
        let mut tx = begin(&b, &cat).await;
        tx.enqueue(&bad).await.unwrap();
        tx.enqueue(&wrong_version).await.unwrap();
        tx.enqueue(&enqueued_event("good")).await.unwrap();
        tx.commit().await.unwrap();

        let report = process_batch(&store, &b, 100, fixed_now()).await.unwrap();
        assert_eq!(report.inserted, 1);
        let reasons: Vec<&str> = report.rejected.iter().map(|r| r.reason).collect();
        assert_eq!(
            reasons,
            ["undecodable_payload", "unsupported_schema_version"]
        );
        assert_eq!(job_count(&store).await, 1);
    }
}
