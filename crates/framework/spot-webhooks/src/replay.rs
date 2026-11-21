//! Replay and test pings: re-enqueue a delivery with its original payload.

use std::sync::Arc;

use spot_core::{Clock, SiteId};
use spot_events_contract::Topic;
use spot_jobs::{JobStore, NewJob};

use crate::delivery::DeliveryLog;
use crate::error::{Error, Result};
use crate::model::{DeliveryId, DeliveryJob, DeliveryStatus, EventType, Webhook, WebhookDelivery};
use crate::{WEBHOOK_JOB_NAME, WEBHOOK_QUEUE};

/// Re-enqueues a failed delivery as a new delivery series with a fresh attempt count and the
/// original payload.
///
/// # Errors
///
/// [`Error::ReplayFailed`] when the delivery is unknown, has no stored payload, or already
/// succeeded.
pub async fn replay_delivery(
    deliveries: &dyn DeliveryLog,
    jobs: &Arc<dyn JobStore>,
    clock: &dyn Clock,
    site: &SiteId,
    delivery_id: DeliveryId,
) -> Result<DeliveryId> {
    let original = deliveries
        .get(delivery_id)
        .await?
        .ok_or_else(|| Error::ReplayFailed {
            reason: format!("delivery {delivery_id} not found"),
        })?;
    if original.status == DeliveryStatus::Delivered {
        return Err(Error::ReplayFailed {
            reason: format!("delivery {delivery_id} already succeeded"),
        });
    }
    let payload = original
        .payload_sent
        .clone()
        .ok_or_else(|| Error::ReplayFailed {
            reason: format!("delivery {delivery_id} has no stored payload"),
        })?;

    let now = clock.now_utc();
    let new_id = DeliveryId::new();
    let idempotency_key = format!(
        "webhook:{}:{}:replay:{new_id}",
        original.webhook_id, original.event_id
    );
    let replay = WebhookDelivery {
        id: new_id,
        webhook_id: original.webhook_id,
        event_id: original.event_id.clone(),
        topic: original.topic.clone(),
        document_id: original.document_id.clone(),
        document_version: original.document_version,
        status: DeliveryStatus::Pending,
        attempts: 0,
        max_attempts: original.max_attempts,
        last_status_code: None,
        last_error_class: None,
        last_error_message: None,
        total_duration_ms: 0,
        created_at: now,
        updated_at: now,
        next_retry_at: None,
        idempotency_key: idempotency_key.clone(),
        payload_sent: Some(payload.clone()),
        response_body: None,
    };
    deliveries.put(&replay).await?;

    let job = DeliveryJob {
        webhook_id: original.webhook_id,
        delivery_id: new_id,
        event_id: original.event_id.clone(),
        topic: original.topic.clone(),
        doctype: String::new(),
        document_id: original.document_id.clone(),
        document_version: original.document_version,
        event_type: EventType::from_topic(original.topic.as_str()).unwrap_or(EventType::Update),
        occurred_at: now,
        after_image: serde_json::Value::Null,
        before_image: None,
        attempt: 1,
        payload_override: Some(payload),
    };
    enqueue(jobs, site, clock, job, idempotency_key).await?;
    Ok(new_id)
}

/// Sends a signed test ping to a webhook.
///
/// # Errors
///
/// [`Error::TestFailed`] when the webhook is not active.
pub async fn test_webhook(
    webhook: &Webhook,
    jobs: &Arc<dyn JobStore>,
    deliveries: &dyn DeliveryLog,
    clock: &dyn Clock,
    site: &SiteId,
) -> Result<DeliveryId> {
    if !webhook.enabled || webhook.state != crate::model::WebhookState::Active {
        return Err(Error::TestFailed {
            reason: format!("webhook {} is not active", webhook.id),
        });
    }
    let now = clock.now_utc();
    let event_id = format!("test-{}", uuid::Uuid::now_v7());
    let ping = serde_json::json!({
        "event_id": event_id,
        "event_type": "ping",
        "doctype": "webhook",
        "document_id": webhook.id.to_string(),
        "document_version": 0,
        "timestamp": now.to_string(),
        "test": true,
        "data": { "message": "webhook test ping" },
    });

    let delivery_id = DeliveryId::new();
    let idempotency_key = format!("webhook:{}:test:{event_id}", webhook.id);
    let delivery = WebhookDelivery {
        id: delivery_id,
        webhook_id: webhook.id,
        event_id: event_id.clone(),
        topic: Topic::app("webhook_test").map_err(|e| Error::TestFailed {
            reason: e.to_string(),
        })?,
        document_id: webhook.id.to_string(),
        document_version: 0,
        status: DeliveryStatus::Pending,
        attempts: 0,
        max_attempts: webhook.max_attempts,
        last_status_code: None,
        last_error_class: None,
        last_error_message: None,
        total_duration_ms: 0,
        created_at: now,
        updated_at: now,
        next_retry_at: None,
        idempotency_key: idempotency_key.clone(),
        payload_sent: Some(ping.clone()),
        response_body: None,
    };
    deliveries.put(&delivery).await?;

    let job = DeliveryJob {
        webhook_id: webhook.id,
        delivery_id,
        event_id,
        topic: Topic::app("webhook_test").map_err(|e| Error::TestFailed {
            reason: e.to_string(),
        })?,
        doctype: String::new(),
        document_id: webhook.id.to_string(),
        document_version: 0,
        event_type: EventType::Update,
        occurred_at: now,
        after_image: serde_json::Value::Null,
        before_image: None,
        attempt: 1,
        payload_override: Some(ping),
    };
    enqueue(jobs, site, clock, job, idempotency_key).await?;
    Ok(delivery_id)
}

async fn enqueue(
    jobs: &Arc<dyn JobStore>,
    site: &SiteId,
    clock: &dyn Clock,
    job: DeliveryJob,
    idempotency_key: String,
) -> Result<()> {
    let args = serde_json::to_value(&job)?;
    let new_job = NewJob::new(
        WEBHOOK_JOB_NAME.to_string(),
        args,
        WEBHOOK_QUEUE.to_string(),
        site.clone(),
        clock.now_utc(),
    )
    .with_idempotency_key(idempotency_key);
    jobs.insert_if_new(new_job).await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::delivery::InMemoryDeliveryLog;
    use crate::model::{PayloadMode, SealedSecret, WebhookId, WebhookState};
    use spot_core::ManualClock;
    use spot_jobs::store::InMemoryJobStore;
    use time::OffsetDateTime;

    fn webhook() -> Webhook {
        Webhook {
            id: WebhookId::new(),
            name: "wh".into(),
            enabled: true,
            state: WebhookState::Active,
            url: "https://example.com/hook".into(),
            doctype: "task".into(),
            events: vec![EventType::Update],
            condition: None,
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

    #[tokio::test]
    async fn replay_requires_a_stored_delivery() {
        let deliveries = InMemoryDeliveryLog::new();
        let jobs: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH);
        let err = replay_delivery(
            &deliveries,
            &jobs,
            &clock,
            &SiteId::new("s").unwrap(),
            DeliveryId::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::ReplayFailed { .. }));
    }

    #[tokio::test]
    async fn test_webhook_enqueues_a_ping_and_a_delivery() {
        let deliveries = InMemoryDeliveryLog::new();
        let jobs: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH);
        let site = SiteId::new("s").unwrap();
        let id = test_webhook(&webhook(), &jobs, &deliveries, &clock, &site)
            .await
            .unwrap();
        let stored = deliveries.get(id).await.unwrap().unwrap();
        assert_eq!(stored.status, DeliveryStatus::Pending);
        assert!(stored.payload_sent.is_some());
    }

    #[tokio::test]
    async fn replay_creates_a_fresh_series_with_the_original_payload() {
        let deliveries = InMemoryDeliveryLog::new();
        let jobs: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH);
        let site = SiteId::new("s").unwrap();
        let original_id = test_webhook(&webhook(), &jobs, &deliveries, &clock, &site)
            .await
            .unwrap();
        let new_id = replay_delivery(&deliveries, &jobs, &clock, &site, original_id)
            .await
            .unwrap();
        assert_ne!(new_id, original_id);
        let replay = deliveries.get(new_id).await.unwrap().unwrap();
        let original = deliveries.get(original_id).await.unwrap().unwrap();
        assert_eq!(replay.payload_sent, original.payload_sent);
        assert_eq!(replay.attempts, 0);
    }
}
