//! Delivery: sending one signed `POST` per attempt through the egress connector, with bounded
//! retries, a dead-letter state, and a per-endpoint circuit breaker and bulkhead.

use std::str::FromStr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde_json::Value;
use spot_core::{Clock, SiteId};
use spot_egress::{
    Bytes, EgressError, Guard, HttpRequest, Method, Purpose, Request, Response, Sender,
};
use spot_jobs::worker::{HandlerResult, JobContext, JobHandlerRegistry};
use spot_jobs::{JobStore, NewJob};
use spot_redact::{RedactionPolicy, Redactor};
use spot_secrets::{KeyProvider, Sealed, SecretBox, SecretString};
use time::OffsetDateTime;
use url::Url;

use crate::config::WebhookConfig;
use crate::error::{Error, Result};
use crate::intake::idempotency_key;
use crate::model::{
    DeliveryId, DeliveryJob, DeliveryStatus, DeliverySummary, Webhook, WebhookDelivery,
};
use crate::payload::build_payload;
use crate::permissions::FieldVisibility;
use crate::resilience::{Refusal, Resilience};
use crate::signing::{SigningKey, build_signature_header};
use crate::webhook::WebhookStore;
use crate::{USER_AGENT, WEBHOOK_JOB_NAME, WEBHOOK_QUEUE};

/// Header names the platform owns; a webhook may not override them.
const SENDER_OWNED: [&str; 8] = [
    "host",
    "content-length",
    "content-type",
    "user-agent",
    "x-spot-event-id",
    "x-spot-delivery",
    "x-spot-signature",
    "transfer-encoding",
];

/// One outbound POST, through whatever transport the caller wires (production uses
/// [`SpotEgress`], which is the egress connector only).
#[async_trait]
pub trait Egress: Send + Sync {
    /// Sends `body` to `url` for `tenant`.
    async fn post(
        &self,
        tenant: &str,
        url: &Url,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> std::result::Result<Response, EgressError>;
}

/// The production [`Egress`]: authorize through the egress guard, then send through the egress
/// [`Sender`] to the pinned addresses only.
pub struct SpotEgress {
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    max_body: u64,
}

impl SpotEgress {
    /// An egress over `guard` and `sender`; responses are read up to `max_body` bytes.
    pub fn new(guard: Arc<Guard>, sender: Arc<dyn Sender>, max_body: u64) -> Self {
        Self {
            guard,
            sender,
            max_body,
        }
    }
}

#[async_trait]
impl Egress for SpotEgress {
    async fn post(
        &self,
        tenant: &str,
        url: &Url,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> std::result::Result<Response, EgressError> {
        let request = Request {
            url: url.clone(),
            purpose: Purpose::Webhook,
            tenant: tenant.to_string(),
            actor: None,
        };
        let authorized = self.guard.authorize(&request).await?;
        let http_request =
            HttpRequest::new(Method::Post, url.clone(), headers, Some(Bytes::from(body)));
        self.sender
            .send_request(&http_request, &authorized, self.max_body)
            .await
    }
}

/// Opens a sealed secret at delivery time. A rotating provider may keep two values open.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Opens the sealed `spsec:v1:` value bound to `aad`.
    async fn resolve(&self, sealed: &str, aad: &[u8]) -> Result<SecretString>;
}

/// A [`SecretResolver`] over the secrets service's [`SecretBox`].
pub struct SealedSecretResolver<P: KeyProvider> {
    secret_box: SecretBox<P>,
}

impl<P: KeyProvider> SealedSecretResolver<P> {
    /// A resolver over `provider`.
    pub fn new(provider: P) -> Self {
        Self {
            secret_box: SecretBox::new(provider),
        }
    }
}

#[async_trait]
impl<P: KeyProvider> SecretResolver for SealedSecretResolver<P> {
    async fn resolve(&self, sealed: &str, aad: &[u8]) -> Result<SecretString> {
        let sealed = Sealed::from_str(sealed).map_err(|e| Error::InvalidSecret {
            reason: e.to_string(),
        })?;
        self.secret_box
            .open(&sealed, aad)
            .await
            .map_err(|e| Error::InvalidSecret {
                reason: e.to_string(),
            })
    }
}

/// The delivery log: one record per (event, webhook) series.
#[async_trait]
pub trait DeliveryLog: Send + Sync {
    /// Inserts or replaces a delivery record.
    async fn put(&self, delivery: &WebhookDelivery) -> Result<()>;
    /// Loads a delivery record.
    async fn get(&self, id: DeliveryId) -> Result<Option<WebhookDelivery>>;
}

/// An in-memory delivery log, for tests.
#[derive(Debug, Default)]
pub struct InMemoryDeliveryLog {
    deliveries: std::sync::RwLock<std::collections::HashMap<DeliveryId, WebhookDelivery>>,
}

impl InMemoryDeliveryLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    fn read(
        &self,
    ) -> std::sync::RwLockReadGuard<'_, std::collections::HashMap<DeliveryId, WebhookDelivery>>
    {
        self.deliveries
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(
        &self,
    ) -> std::sync::RwLockWriteGuard<'_, std::collections::HashMap<DeliveryId, WebhookDelivery>>
    {
        self.deliveries
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl DeliveryLog for InMemoryDeliveryLog {
    async fn put(&self, delivery: &WebhookDelivery) -> Result<()> {
        self.write().insert(delivery.id, delivery.clone());
        Ok(())
    }

    async fn get(&self, id: DeliveryId) -> Result<Option<WebhookDelivery>> {
        Ok(self.read().get(&id).cloned())
    }
}

/// How one delivery attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// A 2xx response.
    Delivered,
    /// Retryable; the job engine should retry with its backoff.
    Retry {
        /// Why.
        reason: String,
    },
    /// Retryable, but a successor job was scheduled at the server's requested time.
    Rescheduled {
        /// Why.
        reason: String,
    },
    /// Permanently failed or the attempts are exhausted.
    Dead {
        /// Why.
        reason: String,
    },
    /// Blocked by the egress policy; never retried.
    Blocked {
        /// Why.
        reason: String,
    },
}

/// Executes webhook delivery jobs.
pub struct DeliveryHandler {
    store: Arc<dyn WebhookStore>,
    jobs: Arc<dyn JobStore>,
    deliveries: Arc<dyn DeliveryLog>,
    egress: Arc<dyn Egress>,
    secrets: Arc<dyn SecretResolver>,
    visibility: Arc<dyn FieldVisibility>,
    resilience: Resilience,
    clock: Arc<dyn Clock>,
    redactor: Mutex<Redactor>,
    tenant: String,
    site: SiteId,
    config: WebhookConfig,
}

impl DeliveryHandler {
    /// Builds a handler. `tenant` is recorded in egress denials; `site` owns the jobs.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidConfig`] when the redaction policy cannot be built.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Arc<dyn WebhookStore>,
        jobs: Arc<dyn JobStore>,
        deliveries: Arc<dyn DeliveryLog>,
        egress: Arc<dyn Egress>,
        secrets: Arc<dyn SecretResolver>,
        visibility: Arc<dyn FieldVisibility>,
        resilience: Resilience,
        clock: Arc<dyn Clock>,
        tenant: String,
        site: SiteId,
        config: WebhookConfig,
    ) -> Result<Self> {
        let redactor = Redactor::from_policy(&RedactionPolicy::default()).map_err(|e| {
            Error::InvalidConfig {
                reason: format!("redaction policy: {e}"),
            }
        })?;
        Ok(Self {
            store,
            jobs,
            deliveries,
            egress,
            secrets,
            visibility,
            resilience,
            clock,
            redactor: Mutex::new(redactor),
            tenant,
            site,
            config,
        })
    }

    /// Registers this handler for `webhooks:deliver` in a job worker's registry.
    pub fn register(self: &Arc<Self>, registry: &mut JobHandlerRegistry) {
        let handler = Arc::clone(self);
        registry.register(WEBHOOK_JOB_NAME, move |ctx| {
            let handler = Arc::clone(&handler);
            Box::pin(async move { handler.execute(ctx).await })
        });
    }

    /// The job-engine entry point: runs the attempt and maps the outcome onto a
    /// [`HandlerResult`].
    pub async fn execute(&self, ctx: JobContext) -> spot_jobs::Result<HandlerResult> {
        let job: DeliveryJob = match serde_json::from_value(ctx.job.args.clone()) {
            Ok(job) => job,
            Err(e) => return Ok(HandlerResult::Fail(format!("malformed_job: {e}"))),
        };
        match self.run(&job).await {
            DeliveryOutcome::Delivered => Ok(HandlerResult::Ok),
            DeliveryOutcome::Retry { reason } => Ok(HandlerResult::Retry(reason)),
            DeliveryOutcome::Rescheduled { reason }
            | DeliveryOutcome::Dead { reason }
            | DeliveryOutcome::Blocked { reason } => Ok(HandlerResult::Fail(reason)),
        }
    }

    /// Runs one attempt: build, sign, send, interpret, record.
    pub async fn run(&self, job: &DeliveryJob) -> DeliveryOutcome {
        let started = self.clock.now_utc();
        let webhook = match self.store.get(job.webhook_id).await {
            Ok(Some(webhook)) => webhook,
            Ok(None) => {
                return DeliveryOutcome::Dead {
                    reason: format!("webhook.not_found: {}", job.webhook_id),
                };
            }
            Err(e) => {
                return DeliveryOutcome::Dead {
                    reason: e.to_string(),
                };
            }
        };
        if !webhook.enabled || webhook.state != crate::model::WebhookState::Active {
            return DeliveryOutcome::Dead {
                reason: format!("webhook.disabled: {}", webhook.id),
            };
        }

        let mut delivery = match self.deliveries.get(job.delivery_id).await {
            Ok(Some(delivery)) => delivery,
            _ => Self::new_delivery(job, &webhook, started),
        };
        let attempt = delivery.attempts.saturating_add(1);
        delivery.attempts = attempt;
        delivery.updated_at = started;
        if attempt > webhook.max_attempts {
            delivery.status = DeliveryStatus::DeadLetter;
            self.persist(&mut delivery, &webhook, None).await;
            return DeliveryOutcome::Dead {
                reason: format!("webhook.dead_letter: {attempt} attempts exhausted"),
            };
        }

        let payload = match &job.payload_override {
            Some(payload) => payload.clone(),
            None => build_payload(
                webhook.payload_mode,
                webhook.field_allowlist.clone(),
                &job.event_id,
                job.event_type,
                &job.doctype,
                &job.document_id,
                job.document_version,
                job.occurred_at,
                &job.after_image,
                job.before_image.as_ref(),
                self.visibility.as_ref(),
            ),
        };
        let body = match serde_json::to_vec(&payload) {
            Ok(body) => body,
            Err(e) => {
                return self
                    .finish_dead(&mut delivery, &webhook, format!("serialize: {e}"), started)
                    .await;
            }
        };
        delivery.payload_sent = Some(payload);

        let aad = format!("webhook={}", webhook.id);
        let secret = match self
            .secrets
            .resolve(&webhook.secret.sealed, aad.as_bytes())
            .await
        {
            Ok(secret) => secret,
            Err(e) => {
                return self
                    .finish_dead(&mut delivery, &webhook, e.to_string(), started)
                    .await;
            }
        };
        let timestamp = started.unix_timestamp();
        self.register_secret("webhook_signing", &secret);
        let signature = SigningKey::new(secret.expose().as_bytes()).sign(timestamp, &body);
        let signature_header = build_signature_header(timestamp, &signature);

        let headers = match self
            .build_headers(job, &webhook, &signature_header, &aad)
            .await
        {
            Ok(headers) => headers,
            Err(e) => {
                return self
                    .finish_dead(&mut delivery, &webhook, e.to_string(), started)
                    .await;
            }
        };

        let endpoint = webhook.url.clone();
        let permit = match self.resilience.acquire(&endpoint) {
            Ok(permit) => permit,
            Err(Refusal::CircuitOpen { retry_after_secs }) => {
                return self
                    .finish_retry(
                        &mut delivery,
                        &webhook,
                        attempt,
                        format!("webhook.circuit_open: {retry_after_secs}s"),
                        Some(retry_after_secs),
                        started,
                    )
                    .await;
            }
            Err(Refusal::BulkheadFull) => {
                return self
                    .finish_retry(
                        &mut delivery,
                        &webhook,
                        attempt,
                        "webhook.bulkhead_full".to_string(),
                        None,
                        started,
                    )
                    .await;
            }
            Err(Refusal::RateLimited { retry_after_secs }) => {
                return self
                    .finish_retry(
                        &mut delivery,
                        &webhook,
                        attempt,
                        "webhook.delivery_rate_limited".to_string(),
                        Some(retry_after_secs),
                        started,
                    )
                    .await;
            }
        };

        let url = match Url::parse(&webhook.url) {
            Ok(url) => url,
            Err(e) => {
                return self
                    .finish_dead(&mut delivery, &webhook, format!("url: {e}"), started)
                    .await;
            }
        };

        let result = self
            .egress
            .post(&self.tenant, &url, headers, body.clone())
            .await;
        let duration_ms = (self.clock.now_utc() - started).whole_milliseconds().max(0) as u64;
        delivery.total_duration_ms = delivery.total_duration_ms.saturating_add(duration_ms);

        match result {
            Ok(response) => {
                let code = response.status.as_u16();
                delivery.last_status_code = Some(code);
                let text = String::from_utf8_lossy(&response.body).into_owned();
                delivery.response_body = Some(self.truncate_redacted(&text));
                if response.status.is_success() {
                    permit.record_success();
                    delivery.status = DeliveryStatus::Delivered;
                    delivery.last_error_class = None;
                    delivery.last_error_message = None;
                    self.persist(&mut delivery, &webhook, Some(duration_ms))
                        .await;
                    DeliveryOutcome::Delivered
                } else if response.status == StatusCode::GONE {
                    permit.record_success();
                    let mut disabled = webhook.clone();
                    disabled.enabled = false;
                    disabled.state = crate::model::WebhookState::Disabled;
                    disabled.state_reason = Some("endpoint returned 410 Gone".to_string());
                    delivery.status = DeliveryStatus::Failed;
                    delivery.last_error_class = Some("gone".to_string());
                    delivery.last_error_message = Some("endpoint returned 410 Gone".to_string());
                    self.persist(&mut delivery, &disabled, Some(duration_ms))
                        .await;
                    DeliveryOutcome::Dead {
                        reason: "gone: endpoint returned 410 Gone".to_string(),
                    }
                } else if response.status == StatusCode::TOO_MANY_REQUESTS
                    || response.status == StatusCode::SERVICE_UNAVAILABLE
                {
                    permit.record_failure();
                    let retry_after = parse_retry_after(&response.headers);
                    self.finish_retry(
                        &mut delivery,
                        &webhook,
                        attempt,
                        format!("webhook.delivery_rate_limited: HTTP {code}"),
                        retry_after,
                        started,
                    )
                    .await
                } else if response.status.is_server_error()
                    || response.status == StatusCode::REQUEST_TIMEOUT
                {
                    permit.record_failure();
                    self.finish_retry(
                        &mut delivery,
                        &webhook,
                        attempt,
                        format!("webhook.delivery_failed: HTTP {code}"),
                        None,
                        started,
                    )
                    .await
                } else {
                    permit.record_success();
                    delivery.status = DeliveryStatus::Failed;
                    delivery.last_error_class = Some("http_error".to_string());
                    delivery.last_error_message = Some(format!("HTTP {code}"));
                    self.persist(&mut delivery, &webhook, Some(duration_ms))
                        .await;
                    DeliveryOutcome::Dead {
                        reason: format!("http_error: HTTP {code}"),
                    }
                }
            }
            Err(e) => {
                let blocked = matches!(
                    e,
                    EgressError::PolicyDenied
                        | EgressError::AddressDenied { .. }
                        | EgressError::PortDenied { .. }
                        | EgressError::RedirectDenied
                );
                if blocked {
                    permit.record_success();
                    delivery.status = DeliveryStatus::Blocked;
                    delivery.last_error_class = Some(e.code().to_string());
                    delivery.last_error_message = Some(e.to_string());
                    self.persist(&mut delivery, &webhook, Some(duration_ms))
                        .await;
                    return DeliveryOutcome::Blocked {
                        reason: e.code().to_string(),
                    };
                }
                permit.record_failure();
                self.finish_retry(
                    &mut delivery,
                    &webhook,
                    attempt,
                    format!("transport: {}", e.code()),
                    None,
                    started,
                )
                .await
            }
        }
    }

    async fn build_headers(
        &self,
        job: &DeliveryJob,
        webhook: &Webhook,
        signature_header: &str,
        aad: &str,
    ) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static(USER_AGENT),
        );
        let delivery_id = job.delivery_id.to_string();
        for (name, value) in [
            ("x-spot-event-id", job.event_id.as_str()),
            ("x-spot-delivery", delivery_id.as_str()),
            ("x-spot-signature", signature_header),
        ] {
            let name = HeaderName::from_static(name);
            let value = HeaderValue::from_str(value).map_err(|e| Error::InvalidHeaders {
                reason: e.to_string(),
            })?;
            headers.insert(name, value);
        }
        for header in &webhook.headers {
            let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|e| {
                Error::InvalidHeaders {
                    reason: e.to_string(),
                }
            })?;
            if SENDER_OWNED.contains(&name.as_str()) {
                return Err(Error::InvalidHeaders {
                    reason: format!("`{}` is set by the platform and cannot be overridden", name),
                });
            }
            let value = if header.is_secret {
                let secret = self.secrets.resolve(&header.value, aad.as_bytes()).await?;
                self.register_secret("webhook_header", &secret);
                secret.expose().to_string()
            } else {
                header.value.clone()
            };
            let value = HeaderValue::from_str(&value).map_err(|e| Error::InvalidHeaders {
                reason: e.to_string(),
            })?;
            headers.insert(name, value);
        }
        Ok(headers)
    }

    fn new_delivery(job: &DeliveryJob, webhook: &Webhook, now: OffsetDateTime) -> WebhookDelivery {
        WebhookDelivery {
            id: job.delivery_id,
            webhook_id: job.webhook_id,
            event_id: job.event_id.clone(),
            topic: job.topic.clone(),
            document_id: job.document_id.clone(),
            document_version: job.document_version,
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
            idempotency_key: idempotency_key(job.webhook_id, &job.event_id),
            payload_sent: job.payload_override.clone(),
            response_body: None,
        }
    }

    async fn finish_dead(
        &self,
        delivery: &mut WebhookDelivery,
        webhook: &Webhook,
        reason: String,
        started: OffsetDateTime,
    ) -> DeliveryOutcome {
        delivery.status = DeliveryStatus::Failed;
        delivery.last_error_class = Some("failed".to_string());
        delivery.last_error_message = Some(self.redact(&reason));
        let duration_ms = (self.clock.now_utc() - started).whole_milliseconds().max(0) as u64;
        self.persist(delivery, webhook, Some(duration_ms)).await;
        DeliveryOutcome::Dead { reason }
    }

    async fn finish_retry(
        &self,
        delivery: &mut WebhookDelivery,
        webhook: &Webhook,
        attempt: u32,
        reason: String,
        retry_after_secs: Option<u64>,
        started: OffsetDateTime,
    ) -> DeliveryOutcome {
        // A retry-after request is honoured by scheduling a successor job at the requested
        // time, so the engine's own backoff cannot retry earlier than the endpoint asked.
        if let Some(secs) = retry_after_secs
            && attempt < webhook.max_attempts
        {
            if let Err(e) = self.schedule_successor(delivery, attempt, secs).await {
                delivery.status = DeliveryStatus::DeadLetter;
                delivery.last_error_message = Some(self.redact(&e.to_string()));
                return DeliveryOutcome::Dead {
                    reason: e.to_string(),
                };
            }
            delivery.status = DeliveryStatus::Pending;
            delivery.last_error_class = Some("rate_limited".to_string());
            delivery.last_error_message = Some(self.redact(&reason));
            delivery.next_retry_at = Some(started + time::Duration::seconds(secs as i64));
            self.persist(delivery, webhook, None).await;
            return DeliveryOutcome::Rescheduled { reason };
        }
        if attempt >= webhook.max_attempts {
            delivery.status = DeliveryStatus::DeadLetter;
            delivery.last_error_class = Some("webhook.dead_letter".to_string());
            delivery.last_error_message = Some(self.redact(&reason));
            self.persist(delivery, webhook, None).await;
            return DeliveryOutcome::Dead {
                reason: format!("webhook.dead_letter: {reason}"),
            };
        }
        delivery.status = DeliveryStatus::Pending;
        delivery.last_error_class = Some(classify_reason(&reason));
        delivery.last_error_message = Some(self.redact(&reason));
        if let Some(secs) = retry_after_secs {
            delivery.next_retry_at = Some(started + time::Duration::seconds(secs as i64));
        }
        self.persist(delivery, webhook, None).await;
        DeliveryOutcome::Retry { reason }
    }

    async fn schedule_successor(
        &self,
        delivery: &WebhookDelivery,
        attempt: u32,
        retry_after_secs: u64,
    ) -> Result<()> {
        let successor = DeliveryJob {
            webhook_id: delivery.webhook_id,
            delivery_id: delivery.id,
            event_id: delivery.event_id.clone(),
            topic: delivery.topic.clone(),
            doctype: String::new(),
            document_id: delivery.document_id.clone(),
            document_version: delivery.document_version,
            event_type: crate::model::EventType::Update,
            occurred_at: delivery.created_at,
            after_image: Value::Null,
            before_image: None,
            attempt: attempt + 1,
            payload_override: delivery.payload_sent.clone(),
        };
        let args = serde_json::to_value(&successor)?;
        let not_before = self.clock.now_utc() + time::Duration::seconds(retry_after_secs as i64);
        let key = format!(
            "{}#a{}",
            idempotency_key(delivery.webhook_id, &delivery.event_id),
            attempt + 1
        );
        let new_job = NewJob::new(
            WEBHOOK_JOB_NAME.to_string(),
            args,
            WEBHOOK_QUEUE.to_string(),
            self.site.clone(),
            not_before,
        )
        .with_idempotency_key(key);
        self.jobs.insert_if_new(new_job).await?;
        Ok(())
    }

    async fn persist(
        &self,
        delivery: &mut WebhookDelivery,
        webhook: &Webhook,
        duration_ms: Option<u64>,
    ) {
        delivery.updated_at = self.clock.now_utc();
        let _ = self.deliveries.put(delivery).await;
        let mut updated = webhook.clone();
        updated.last_delivery = Some(DeliverySummary {
            attempted_at: delivery.updated_at,
            status_code: delivery.last_status_code,
            error_class: delivery.last_error_class.clone(),
            duration_ms: duration_ms.unwrap_or(0),
            success: delivery.status == DeliveryStatus::Delivered,
        });
        let _ = self.store.put(&updated).await;
    }

    fn truncate_redacted(&self, text: &str) -> String {
        let redacted = self.redact(text);
        let cap = self.config.max_response_body_log_bytes;
        if redacted.len() <= cap {
            return redacted;
        }
        let mut end = cap;
        while end > 0 && !redacted.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = redacted;
        out.truncate(end);
        out.push('…');
        out
    }

    fn redact(&self, text: &str) -> String {
        self.redactor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .redact_str(text)
            .into_owned()
    }

    fn register_secret(&self, label: &str, secret: &SecretString) {
        let _ = self
            .redactor
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .register_secret(label, secret);
    }
}

/// Parses a `Retry-After` header as seconds (the delta-seconds form).
pub fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.trim().parse().ok())
}

/// A coarse error class from a reason string.
pub(crate) fn classify_reason(reason: &str) -> String {
    reason
        .split_once(':')
        .map_or(reason, |(class, _)| class)
        .to_string()
}
