//! Shared test fixtures: a scripted [`Egress`], a fake secret resolver and a test rig.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::{HeaderMap, HeaderValue, StatusCode};
use spot_core::{ManualClock, SiteId};
use spot_egress::{EgressError, Response};
use spot_secrets::SecretString;
use time::OffsetDateTime;

use crate::config::WebhookConfig;
use crate::delivery::{DeliveryHandler, Egress, InMemoryDeliveryLog, SecretResolver};
use crate::model::{
    BulkheadConfig, CircuitBreakerConfig, DeliveryJob, EventType, PayloadMode, RateLimitConfig,
    SealedSecret, Webhook, WebhookId, WebhookState,
};
use crate::permissions::{AllFieldsReadable, FieldVisibility};
use crate::resilience::Resilience;
use crate::webhook::{InMemoryWebhookStore, WebhookStore};
use spot_jobs::JobStore;

/// One scripted response.
pub enum Step {
    /// Respond with this response.
    Respond(Response),
    /// Fail with this egress error.
    Fail(EgressError),
}

/// A request the scripted egress received.
#[derive(Clone)]
pub struct Recorded {
    /// The URL.
    pub url: String,
    /// The headers.
    pub headers: HeaderMap,
    /// The body bytes.
    pub body: Vec<u8>,
}

/// An [`Egress`] that returns scripted responses and records what it received.
pub struct ScriptedEgress {
    steps: Mutex<VecDeque<Step>>,
    recorded: Mutex<Vec<Recorded>>,
}

impl ScriptedEgress {
    /// A scripted egress with these steps, in order.
    pub fn new(steps: Vec<Step>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// The requests received, in order.
    pub fn recorded(&self) -> Vec<Recorded> {
        self.recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl Egress for ScriptedEgress {
    async fn post(
        &self,
        _tenant: &str,
        url: &url::Url,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> std::result::Result<Response, EgressError> {
        self.recorded
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Recorded {
                url: url.to_string(),
                headers,
                body,
            });
        let step = self
            .steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        match step {
            Some(Step::Respond(response)) => Ok(response),
            Some(Step::Fail(error)) => Err(error),
            None => Err(EgressError::ResolutionFailed),
        }
    }
}

/// A [`SecretResolver`] that returns one fixed value for every sealed secret.
pub struct FakeSecrets {
    /// The value returned.
    pub value: String,
}

impl FakeSecrets {
    /// A resolver returning `value`.
    pub fn new(value: &str) -> Self {
        Self {
            value: value.to_string(),
        }
    }
}

#[async_trait]
impl SecretResolver for FakeSecrets {
    async fn resolve(&self, _sealed: &str, _aad: &[u8]) -> crate::Result<SecretString> {
        Ok(SecretString::new(self.value.clone()))
    }
}

/// A [`FieldVisibility`] that hides the listed fields.
pub struct DenyFields {
    /// The hidden field names.
    pub denied: Vec<String>,
}

impl FieldVisibility for DenyFields {
    fn may_read(&self, _doctype: &str, field: &str) -> bool {
        !self.denied.iter().any(|denied| denied == field)
    }
}

/// A response with no body and no extra headers.
pub fn response(status: u16) -> Response {
    Response {
        status: StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        headers: HeaderMap::new(),
        body: Vec::new(),
    }
}

/// A response with a body.
pub fn response_with_body(status: u16, body: &str) -> Response {
    Response {
        status: StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        headers: HeaderMap::new(),
        body: body.as_bytes().to_vec(),
    }
}

/// A response with a `Retry-After` header.
pub fn response_retry_after(status: u16, seconds: u64) -> Response {
    let mut headers = HeaderMap::new();
    if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
        headers.insert("retry-after", value);
    }
    Response {
        status: StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
        headers,
        body: Vec::new(),
    }
}

/// A test webhook with a fixed URL and defaults.
pub fn webhook(url: &str) -> Webhook {
    Webhook {
        id: WebhookId::new(),
        name: "test".into(),
        enabled: true,
        state: WebhookState::Active,
        url: url.into(),
        doctype: "task".into(),
        events: vec![EventType::Update],
        condition: None,
        payload_mode: PayloadMode::Full,
        field_allowlist: vec![],
        secret: SealedSecret {
            sealed: "spsec:v1:test-secret".into(),
            previous: None,
        },
        headers: vec![],
        timeout_secs: 30,
        max_attempts: 3,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
        last_delivery: None,
        state_reason: None,
    }
}

/// A delivery job for `webhook`.
pub fn delivery_job(webhook: &Webhook) -> DeliveryJob {
    DeliveryJob {
        webhook_id: webhook.id,
        delivery_id: crate::model::DeliveryId::new(),
        event_id: "evt-1".into(),
        topic: spot_events_contract::Topic::any("doc_updated").unwrap_or_else(|_| unreachable!()),
        doctype: webhook.doctype.clone(),
        document_id: "doc-1".into(),
        document_version: 1,
        event_type: EventType::Update,
        occurred_at: OffsetDateTime::UNIX_EPOCH,
        after_image: serde_json::json!({"status": "Done"}),
        before_image: None,
        attempt: 1,
        payload_override: None,
    }
}

/// The parts of a test rig, so a test can inspect them.
pub struct Rig {
    /// The webhook store.
    pub store: Arc<InMemoryWebhookStore>,
    /// The job store.
    pub jobs: Arc<dyn JobStore>,
    /// The delivery log.
    pub deliveries: Arc<InMemoryDeliveryLog>,
    /// The clock.
    pub clock: Arc<ManualClock>,
    /// The scripted egress.
    pub egress: Arc<ScriptedEgress>,
    /// The resilience limits.
    pub resilience: Resilience,
    /// The visibility the handler uses.
    pub visibility: Arc<dyn FieldVisibility>,
    /// The signing secret value.
    pub secret: String,
}

impl Rig {
    /// A rig with the given delivery steps.
    pub fn new(steps: Vec<Step>) -> Self {
        Self::with_limits(
            steps,
            CircuitBreakerConfig {
                failure_threshold: 2,
                open_secs: 10,
                half_open_probes: 1,
            },
            BulkheadConfig { max_concurrent: 4 },
        )
    }

    /// A rig with custom resilience limits and full field visibility.
    pub fn with_limits(
        steps: Vec<Step>,
        breaker: CircuitBreakerConfig,
        bulkhead: BulkheadConfig,
    ) -> Self {
        Self::with_visibility(steps, breaker, bulkhead, Arc::new(AllFieldsReadable))
    }

    /// A rig with custom resilience limits and a visibility.
    pub fn with_visibility(
        steps: Vec<Step>,
        breaker: CircuitBreakerConfig,
        bulkhead: BulkheadConfig,
        visibility: Arc<dyn FieldVisibility>,
    ) -> Self {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        Self {
            store: Arc::new(InMemoryWebhookStore::new()),
            jobs: Arc::new(spot_jobs::store::InMemoryJobStore::new()),
            deliveries: Arc::new(InMemoryDeliveryLog::new()),
            clock: clock.clone(),
            egress: Arc::new(ScriptedEgress::new(steps)),
            resilience: Resilience::new(clock, breaker, bulkhead, RateLimitConfig::default()),
            visibility,
            secret: "the-signing-secret-value".into(),
        }
    }

    /// Stores `webhook` and returns it.
    pub async fn register(&self, webhook: Webhook) -> Webhook {
        self.store.put(&webhook).await.ok();
        webhook
    }

    /// The handler over this rig.
    pub fn handler(&self) -> Arc<DeliveryHandler> {
        Arc::new(
            DeliveryHandler::new(
                self.store.clone(),
                self.jobs.clone(),
                self.deliveries.clone(),
                self.egress.clone(),
                Arc::new(FakeSecrets::new(&self.secret)),
                self.visibility.clone(),
                self.resilience.clone(),
                self.clock.clone(),
                "site".into(),
                SiteId::new("site").unwrap_or_else(|_| unreachable!()),
                WebhookConfig::default(),
            )
            .unwrap_or_else(|_| unreachable!()),
        )
    }
}
