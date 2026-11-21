//! Intake: turning a committed event into one delivery job per matching webhook.
//!
//! The intake runs after commit (the caller passes committed events), matches enabled webhooks
//! by DocType and event name, evaluates each webhook's condition on the event's after-image,
//! and enqueues one delivery job per (event, webhook) with the idempotency key
//! `webhook:<id>:<event_id>`, so a replayed intake creates no duplicate while the job is still
//! outstanding. A condition that fails to evaluate is a recorded non-delivery, never a retry.

use std::sync::Arc;

use serde_json::Value;
use spot_core::{Clock, SiteId};
use spot_events_contract::{EventEnvelope, Topic};
use spot_jobs::{JobStore, NewJob};
use time::OffsetDateTime;

use crate::condition::evaluate_condition;
use crate::error::{Error, Result};
use crate::model::{DeliveryId, DeliveryJob, EventType, WebhookId};
use crate::webhook::WebhookStore;
use crate::{WEBHOOK_JOB_NAME, WEBHOOK_QUEUE};

/// A committed event the intake may deliver, with the document images the caller supplies.
///
/// The lifecycle payload carries only `name`/`changed_fields`, so the platform relay enriches
/// the envelope with the document's after-image (and before-image for updates) before intake.
#[derive(Debug, Clone)]
pub struct CommittedEvent {
    /// The event id (the deduplication key).
    pub event_id: String,
    /// The event topic.
    pub topic: Topic,
    /// The subject DocType.
    pub doctype: String,
    /// The subject record id.
    pub document_id: String,
    /// The subject version.
    pub document_version: u64,
    /// The event name.
    pub event_type: EventType,
    /// When the event happened.
    pub occurred_at: OffsetDateTime,
    /// The document after the operation.
    pub after_image: Value,
    /// The document before the operation, for updates.
    pub before_image: Option<Value>,
}

impl CommittedEvent {
    /// Builds a committed event from an envelope plus the document images.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidFilter`] when the envelope's topic is not a lifecycle topic.
    pub fn from_envelope(
        envelope: &EventEnvelope,
        after_image: Value,
        before_image: Option<Value>,
    ) -> Result<Self> {
        let event_type =
            EventType::from_topic(envelope.topic.as_str()).ok_or_else(|| Error::InvalidFilter {
                reason: format!("{} is not a lifecycle topic", envelope.topic),
            })?;
        Ok(Self {
            event_id: envelope.event_id.to_string(),
            topic: envelope.topic.clone(),
            doctype: envelope.subject.entity.clone(),
            document_id: envelope.subject.id.to_string(),
            document_version: envelope.subject_version.unwrap_or(0),
            event_type,
            occurred_at: envelope.meta.occurred_at,
            after_image,
            before_image,
        })
    }
}

/// What one intake pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntakeReport {
    /// Delivery jobs created (a replayed intake creates none while the job is outstanding).
    pub jobs_created: u32,
    /// Webhooks skipped because their condition failed to evaluate.
    pub rejected: Vec<RejectedEvent>,
}

/// A webhook whose condition could not be evaluated for an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedEvent {
    /// The webhook.
    pub webhook_id: WebhookId,
    /// Why (a condition evaluation error).
    pub reason: String,
}

/// Consumes committed events and enqueues delivery jobs.
pub struct WebhookIntake {
    store: Arc<dyn WebhookStore>,
    jobs: Arc<dyn JobStore>,
    clock: Arc<dyn Clock>,
    site: SiteId,
}

impl WebhookIntake {
    /// An intake over `store` (webhooks) and `jobs` (the delivery queue) for `site`.
    pub fn new(
        store: Arc<dyn WebhookStore>,
        jobs: Arc<dyn JobStore>,
        clock: Arc<dyn Clock>,
        site: SiteId,
    ) -> Self {
        Self {
            store,
            jobs,
            clock,
            site,
        }
    }

    /// Processes one committed event.
    ///
    /// # Errors
    ///
    /// Only infrastructure errors (the store or the job queue); a condition error is a
    /// [`RejectedEvent`], not an error.
    pub async fn process(&self, event: &CommittedEvent) -> Result<IntakeReport> {
        let mut report = IntakeReport::default();
        let webhooks = self
            .store
            .list_enabled_for_event(&event.doctype, event.event_type)
            .await?;

        for webhook in webhooks {
            if let Some(condition) = &webhook.condition {
                match evaluate_condition(condition, &event.after_image, event.occurred_at) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(e) => {
                        report.rejected.push(RejectedEvent {
                            webhook_id: webhook.id,
                            reason: e.to_string(),
                        });
                        continue;
                    }
                }
            }

            let job = DeliveryJob {
                webhook_id: webhook.id,
                delivery_id: DeliveryId::new(),
                event_id: event.event_id.clone(),
                topic: event.topic.clone(),
                doctype: event.doctype.clone(),
                document_id: event.document_id.clone(),
                document_version: event.document_version,
                event_type: event.event_type,
                occurred_at: event.occurred_at,
                after_image: event.after_image.clone(),
                before_image: event.before_image.clone(),
                attempt: 1,
                payload_override: None,
            };

            if self.enqueue(&job, &event.event_id).await? {
                report.jobs_created += 1;
            }
        }
        Ok(report)
    }

    /// Processes a batch of committed events.
    pub async fn process_batch(&self, events: &[CommittedEvent]) -> Result<Vec<IntakeReport>> {
        let mut reports = Vec::with_capacity(events.len());
        for event in events {
            reports.push(self.process(event).await?);
        }
        Ok(reports)
    }

    /// Enqueues one delivery job; returns whether it was newly created.
    async fn enqueue(&self, job: &DeliveryJob, event_id: &str) -> Result<bool> {
        let args = serde_json::to_value(job)?;
        let new_job = NewJob::new(
            WEBHOOK_JOB_NAME.to_string(),
            args,
            WEBHOOK_QUEUE.to_string(),
            self.site.clone(),
            self.clock.now_utc(),
        )
        .with_idempotency_key(idempotency_key(job.webhook_id, event_id));
        Ok(matches!(
            self.jobs.insert_if_new(new_job).await?,
            spot_jobs::model::InsertResult::Inserted(_)
        ))
    }
}

/// The idempotency key of a delivery job: `webhook:<id>:<event_id>`.
pub fn idempotency_key(webhook_id: WebhookId, event_id: &str) -> String {
    format!("webhook:{webhook_id}:{event_id}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{EventType, PayloadMode, SealedSecret, Webhook, WebhookState};
    use crate::webhook::InMemoryWebhookStore;
    use serde_json::json;
    use spot_jobs::store::{InMemoryJobStore, JobFilter};

    fn event(event_type: EventType, doctype: &str, after: Value) -> CommittedEvent {
        CommittedEvent {
            event_id: "evt-1".into(),
            topic: Topic::any(event_type.topic_name()).unwrap(),
            doctype: doctype.into(),
            document_id: "doc-1".into(),
            document_version: 1,
            event_type,
            occurred_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            after_image: after,
            before_image: None,
        }
    }

    fn webhook(doctype: &str, events: Vec<EventType>, condition: Option<&str>) -> Webhook {
        Webhook {
            id: WebhookId::new(),
            name: "wh".into(),
            enabled: true,
            state: WebhookState::Active,
            url: "https://example.com/hook".into(),
            doctype: doctype.into(),
            events,
            condition: condition.map(str::to_owned),
            payload_mode: PayloadMode::Full,
            field_allowlist: vec![],
            secret: SealedSecret {
                sealed: "spsec:v1:abc".into(),
                previous: None,
            },
            headers: vec![],
            timeout_secs: 30,
            max_attempts: 5,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
            last_delivery: None,
            state_reason: None,
        }
    }

    async fn harness(webhooks: Vec<Webhook>) -> (WebhookIntake, Arc<InMemoryJobStore>) {
        let store = Arc::new(InMemoryWebhookStore::new());
        for w in &webhooks {
            store.put(w).await.unwrap();
        }
        let jobs = Arc::new(InMemoryJobStore::new());
        let clock = Arc::new(spot_core::ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let intake = WebhookIntake::new(store, jobs.clone(), clock, SiteId::new("site").unwrap());
        (intake, jobs)
    }

    async fn job_count(jobs: &InMemoryJobStore) -> usize {
        jobs.list(JobFilter::default()).await.unwrap().len()
    }

    #[tokio::test]
    async fn a_matching_event_creates_one_delivery_job() {
        let (intake, jobs) = harness(vec![webhook("task", vec![EventType::Create], None)]).await;
        let report = intake
            .process(&event(EventType::Create, "task", json!({"status": "Open"})))
            .await
            .unwrap();
        assert_eq!(report.jobs_created, 1);
        assert_eq!(job_count(&jobs).await, 1);
    }

    #[tokio::test]
    async fn a_replayed_intake_creates_no_duplicate() {
        let (intake, jobs) = harness(vec![webhook("task", vec![EventType::Create], None)]).await;
        let evt = event(EventType::Create, "task", json!({"status": "Open"}));
        assert_eq!(intake.process(&evt).await.unwrap().jobs_created, 1);
        assert_eq!(intake.process(&evt).await.unwrap().jobs_created, 0);
        assert_eq!(job_count(&jobs).await, 1);
    }

    #[tokio::test]
    async fn the_condition_filters_events() {
        let (intake, jobs) = harness(vec![webhook(
            "task",
            vec![EventType::Update],
            Some("doc.status == 'Done'"),
        )])
        .await;
        let open = intake
            .process(&event(EventType::Update, "task", json!({"status": "Open"})))
            .await
            .unwrap();
        assert_eq!(open.jobs_created, 0);
        let done = intake
            .process(&event(EventType::Update, "task", json!({"status": "Done"})))
            .await
            .unwrap();
        assert_eq!(done.jobs_created, 1);
        assert_eq!(job_count(&jobs).await, 1);
    }

    #[tokio::test]
    async fn a_broken_condition_is_recorded_not_retried() {
        let (intake, jobs) = harness(vec![webhook(
            "task",
            vec![EventType::Update],
            Some("doc.status =="),
        )])
        .await;
        let report = intake
            .process(&event(EventType::Update, "task", json!({"status": "Open"})))
            .await
            .unwrap();
        assert_eq!(report.jobs_created, 0);
        assert_eq!(report.rejected.len(), 1);
        assert_eq!(job_count(&jobs).await, 0);
    }

    #[tokio::test]
    async fn one_event_creates_one_job_per_matching_webhook() {
        let (intake, jobs) = harness(vec![
            webhook("task", vec![EventType::Create], None),
            webhook(
                "task",
                vec![EventType::Create],
                Some("doc.status == 'Open'"),
            ),
            webhook("other", vec![EventType::Create], None),
        ])
        .await;
        let report = intake
            .process(&event(EventType::Create, "task", json!({"status": "Open"})))
            .await
            .unwrap();
        assert_eq!(report.jobs_created, 2);
        assert_eq!(job_count(&jobs).await, 2);
    }
}
