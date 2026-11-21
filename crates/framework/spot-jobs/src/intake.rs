//! The job intake: reading jobs from the outbox and enqueuing scheduled jobs.

use crate::model::NewJob;
use crate::store::JobStore;
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_core::SiteId;
use spot_events_contract::PayloadDocument;
use spot_scheduler::slot::RunSlot;
use spot_storage_contract::feed::{ChangeFeed, FeedPosition};
use time::OffsetDateTime;

/// The data of a `sys_job_enqueued` outbox event (schema version 1): the `data` of the payload
/// document, in the shape the events contract fixes (`contracts/events`, 0.7.7).
///
/// An absent field and an explicit `null` mean the same thing for every optional field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobEnqueued {
    /// The site ID that owns the job. **Legacy:** no producer writes it (an intake runs for one
    /// site and the caller supplies it). Events already in the outbox may carry it; it is
    /// accepted only when it equals the caller's site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// The job name, app-qualified: `app:job`.
    pub job: String,
    /// Job arguments (absent or `null` is `null`; no floating-point number at any depth).
    #[serde(default)]
    pub args: Json,
    /// Idempotency key (optional, not empty).
    #[serde(default)]
    pub idempotency_key: Option<String>,
    /// Earliest start time (optional, RFC 3339).
    #[serde(default)]
    pub not_before: Option<String>,
    /// Queue name (optional; defaults to `default`).
    #[serde(default)]
    pub queue: Option<String>,
    /// Opaque context for extensions (optional, an object).
    #[serde(default)]
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

/// Whether `value` holds a JSON number that is not an integer, at any depth.
fn has_float(value: &Json) -> bool {
    match value {
        Json::Number(n) => n.is_f64(),
        Json::Array(items) => items.iter().any(has_float),
        Json::Object(map) => map.values().any(has_float),
        _ => false,
    }
}

/// A queue name as the contract spells it: 1 to 64 bytes of `a-z`, `0-9`, `_` and `-`.
fn is_queue_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Why a decoded payload is not a job, or the job to insert.
fn job_from_payload(
    payload: JobEnqueued,
    site: &SiteId,
    now: OffsetDateTime,
    event_id: String,
) -> std::result::Result<NewJob, &'static str> {
    if payload.validate().is_err() {
        return Err("job.invalid_name");
    }
    // A legacy `site` is accepted only when it is the caller's: an event for another site is
    // never run here.
    if let Some(named) = &payload.site {
        match SiteId::new(named) {
            Ok(named) if named == *site => {}
            _ => return Err("invalid_site"),
        }
    }
    // The contract's own rules (a decimal is a string, never a float; ADR-055). An event that
    // breaks them is refused, not repaired.
    if has_float(&payload.args)
        || payload
            .context
            .as_ref()
            .is_some_and(|c| (!c.is_object() && !c.is_null()) || has_float(c))
        || payload.idempotency_key.as_deref() == Some("")
        || payload.queue.as_deref().is_some_and(|q| !is_queue_name(q))
    {
        return Err("undecodable_payload");
    }
    let not_before = match &payload.not_before {
        Some(text) => OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
            .map_err(|_| "undecodable_payload")?,
        None => now,
    };
    let mut job = NewJob::new(
        payload.job,
        payload.args,
        payload.queue.unwrap_or_else(|| "default".to_string()),
        site.clone(),
        not_before,
    );
    if let Some(key) = payload.idempotency_key {
        job = job.with_idempotency_key(key);
    }
    if let Some(context) = payload.context.filter(|c| !c.is_null()) {
        job = job.with_context(context);
    }
    Ok(job.with_outbox_event_id(event_id))
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
/// The intake runs for one site: `site` is the caller's context (the consumer lease is per
/// site), not part of the payload. Every job is created for `site`. A payload that still names
/// a `site` (an event written before the field was dropped) is accepted only when it equals
/// `site`, otherwise it is rejected `invalid_site`.
///
/// This function:
/// - Reads up to `batch_size` events from the feed starting at the stored position
/// - Filters for `sys_job_enqueued` events
/// - Reads the outbox payload as the events contract's [`PayloadDocument`] (`meta` and `data`)
///   and decodes `data` as [`JobEnqueued`] v1
/// - Inserts jobs using the outbox event ID as an additional dedupe key
/// - Advances the position only after all jobs are successfully inserted
/// - Returns `job.intake_resync` if `ResyncRequired` is encountered
///
/// An event that cannot become a job is reported in [`IntakeReport::rejected`] and never
/// dropped silently. The reasons are `unsupported_schema_version`, `undecodable_payload` (not a
/// payload document, not the job shape, a floating-point number in `args` or `context`, an
/// empty key, a bad queue name or a bad `not_before`), `job.invalid_name` and `invalid_site`.
pub async fn process_batch<F: ChangeFeed + ?Sized>(
    store: &dyn JobStore,
    feed: &F,
    site: &SiteId,
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
        // The outbox holds the payload document; the job is its `data`.
        let payload: JobEnqueued = match PayloadDocument::from_outbox_event(&record.event)
            .ok()
            .and_then(|doc| serde_json::from_value(doc.data).ok())
        {
            Some(p) => p,
            None => {
                reject(record, "undecodable_payload");
                continue;
            }
        };
        match job_from_payload(payload, site, now, record.event.id.to_string()) {
            Ok(job) => jobs_to_insert.push(job),
            Err(reason) => reject(record, reason),
        }
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
            site: None,
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
            site: None,
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

    /// The outbox event the document engine writes: the payload document, data nested.
    fn enqueued_event(key: &str) -> OutboxEvent {
        let id = RecordId::new_v7();
        let doc = spot_events_contract::PayloadDocument {
            meta: spot_events_contract::EventMeta {
                occurred_at: fixed_now(),
                actor: spot_events_contract::Actor::System {
                    component: name("intake_test"),
                },
                command_id: None,
                causation_id: None,
                correlation_id: id,
                trace_id: None,
            },
            data: serde_json::json!({
                "job": "app:send",
                "args": {"k": key},
                "idempotency_key": key,
                "not_before": null,
            }),
        };
        OutboxEvent {
            id,
            topic: name(crate::SYS_JOB_ENQUEUED_TOPIC),
            subject: NodeRef {
                entity: name("party"),
                id: RecordId::new_v7(),
            },
            subject_version: None,
            schema_version: 1,
            payload: serde_json::to_string(&doc).unwrap(),
        }
    }

    fn test_site() -> SiteId {
        SiteId::new("test-site").unwrap()
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
        process_batch(&store, &b, &test_site(), 100, fixed_now())
            .await
            .unwrap();
        assert_eq!(
            job_count(&store).await,
            0,
            "a rollback must not create a job"
        );

        let mut tx = begin(&b, &cat).await;
        tx.enqueue(&enqueued_event("committed")).await.unwrap();
        tx.commit().await.unwrap();
        process_batch(&store, &b, &test_site(), 100, fixed_now())
            .await
            .unwrap();
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

        let end = process_batch(&store, &b, &test_site(), 100, fixed_now())
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
        process_batch(&store, &b, &test_site(), 100, fixed_now())
            .await
            .unwrap();
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
        let err = process_batch(&store, &b, &test_site(), 10, fixed_now())
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

        let report = process_batch(&store, &b, &test_site(), 100, fixed_now())
            .await
            .unwrap();
        assert_eq!(report.inserted, 1);
        let reasons: Vec<&str> = report.rejected.iter().map(|r| r.reason).collect();
        assert_eq!(
            reasons,
            ["undecodable_payload", "unsupported_schema_version"]
        );
        assert_eq!(job_count(&store).await, 1);
    }
}
