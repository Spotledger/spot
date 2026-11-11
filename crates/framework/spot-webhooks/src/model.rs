//! Webhook model types: the webhook record, its deliveries, the delivery job's arguments and
//! the resilience configuration.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spot_events_contract::Topic;
use time::OffsetDateTime;
use uuid::Uuid;

/// Unique identifier for a webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WebhookId(pub Uuid);

impl WebhookId {
    /// A new random (UUIDv7) webhook id.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WebhookId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for WebhookId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for WebhookId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// Unique identifier for one delivery series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeliveryId(pub Uuid);

impl DeliveryId {
    /// A new random (UUIDv7) delivery id.
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for DeliveryId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for DeliveryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for DeliveryId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// The state of a webhook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookState {
    /// Active: matching events create deliveries.
    Active,
    /// Paused by an administrator: no new deliveries.
    Paused,
    /// Disabled, for example after a `410 Gone` response.
    Disabled,
    /// Dead-lettered after its deliveries exhausted their retries.
    DeadLetter,
}

/// The event names a webhook can listen to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// A document was created.
    Create,
    /// A document was updated.
    Update,
    /// A document was deleted.
    Delete,
    /// A document was posted.
    Post,
    /// A document was cancelled.
    Cancel,
    /// A document was renamed.
    Rename,
    /// A posted document's allowed fields were updated.
    UpdatedPosted,
}

impl EventType {
    /// The snake_case name used on the wire and in the DocType.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::Post => "post",
            Self::Cancel => "cancel",
            Self::Rename => "rename",
            Self::UpdatedPosted => "updated_posted",
        }
    }

    /// The lifecycle operation's topic name (`doc_created`, ...).
    pub fn topic_name(self) -> &'static str {
        match self {
            Self::Create => "doc_created",
            Self::Update => "doc_updated",
            Self::Delete => "doc_deleted",
            Self::Post => "doc_posted",
            Self::Cancel => "doc_cancelled",
            Self::Rename => "doc_renamed",
            Self::UpdatedPosted => "doc_updated_posted",
        }
    }

    /// The event name of a lifecycle topic, or `None` for any other topic.
    pub fn from_topic(topic: &str) -> Option<Self> {
        match topic {
            "doc_created" => Some(Self::Create),
            "doc_updated" => Some(Self::Update),
            "doc_deleted" => Some(Self::Delete),
            "doc_posted" => Some(Self::Post),
            "doc_cancelled" => Some(Self::Cancel),
            "doc_renamed" => Some(Self::Rename),
            "doc_updated_posted" => Some(Self::UpdatedPosted),
            _ => None,
        }
    }
}

/// How much of the document a delivery carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PayloadMode {
    /// Only the document id and version, no fields.
    Ids,
    /// Only the fields in the allow-list that the owner may read.
    Fields,
    /// Every field of the document the owner may read.
    Full,
}

/// One custom header a delivery carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    /// The header name.
    pub name: String,
    /// The header value; when `is_secret` it is the sealed (`spsec:v1:`) form, opened at
    /// delivery time and never stored in the delivery record.
    pub value: String,
    /// Whether the value is a sealed secret.
    pub is_secret: bool,
}

/// A sealed secret (the `spsec:v1:` form) plus an optional previous value kept valid during a
/// rotation overlap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedSecret {
    /// The current sealed value.
    pub sealed: String,
    /// The previous sealed value, accepted until `expires_at`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PreviousSecret>,
}

/// A previous secret kept valid during a rotation overlap period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousSecret {
    /// The previous sealed value.
    pub sealed: String,
    /// When the overlap ends.
    pub expires_at: OffsetDateTime,
}

/// A summary of the last delivery attempt, shown on the webhook record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverySummary {
    /// When the attempt happened.
    pub attempted_at: OffsetDateTime,
    /// The HTTP status, when there was a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    /// The error class, when the attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    /// The attempt's duration in milliseconds.
    pub duration_ms: u64,
    /// Whether the attempt succeeded.
    pub success: bool,
}

/// A webhook: an endpoint, a filter and the delivery settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Webhook {
    /// Unique identifier.
    pub id: WebhookId,
    /// Human-readable name.
    pub name: String,
    /// Whether the webhook is enabled.
    pub enabled: bool,
    /// Current state.
    pub state: WebhookState,
    /// The target URL.
    pub url: String,
    /// The DocType this webhook listens to.
    pub doctype: String,
    /// The event names that trigger it.
    pub events: Vec<EventType>,
    /// An optional condition in the shared expression language, parsed on save and evaluated
    /// on the event's after-image at intake.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// The payload mode.
    pub payload_mode: PayloadMode,
    /// The field allow-list for `fields` mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub field_allowlist: Vec<String>,
    /// The signing secret, sealed.
    pub secret: SealedSecret,
    /// Custom headers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
    /// Request timeout in seconds.
    pub timeout_secs: u64,
    /// Maximum delivery attempts before dead-lettering.
    pub max_attempts: u32,
    /// When the webhook was created.
    pub created_at: OffsetDateTime,
    /// When the webhook was last updated.
    pub updated_at: OffsetDateTime,
    /// The last delivery summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery: Option<DeliverySummary>,
    /// Why the webhook is disabled or dead-lettered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
}

/// The status of a delivery series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    /// Queued or retrying: not terminal.
    Pending,
    /// Delivered with a 2xx response.
    Delivered,
    /// Failed permanently (a non-retryable response or an exhausted series).
    Failed,
    /// Dead-lettered after the attempts were exhausted.
    DeadLetter,
    /// Blocked by the egress policy; never retried.
    Blocked,
}

impl DeliveryStatus {
    /// Whether the series is over (no further attempts).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Delivered | Self::Failed | Self::DeadLetter | Self::Blocked
        )
    }
}

/// A delivery series: one record per (event, webhook) with the attempt history summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDelivery {
    /// Unique delivery id.
    pub id: DeliveryId,
    /// The webhook.
    pub webhook_id: WebhookId,
    /// The event id that triggered it.
    pub event_id: String,
    /// The event topic.
    pub topic: Topic,
    /// The document id.
    pub document_id: String,
    /// The document version.
    pub document_version: u64,
    /// Current status.
    pub status: DeliveryStatus,
    /// Attempts made so far.
    pub attempts: u32,
    /// Maximum attempts allowed.
    pub max_attempts: u32,
    /// The last HTTP status, when there was a response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status_code: Option<u16>,
    /// The last error class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_class: Option<String>,
    /// The last error message, redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_message: Option<String>,
    /// Total duration across attempts in milliseconds.
    pub total_duration_ms: u64,
    /// When the delivery was created.
    pub created_at: OffsetDateTime,
    /// When the delivery was last updated.
    pub updated_at: OffsetDateTime,
    /// When the next attempt is due, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_retry_at: Option<OffsetDateTime>,
    /// The idempotency key that created the first job.
    pub idempotency_key: String,
    /// The payload sent (or to send), for replay. Never carries a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_sent: Option<Value>,
    /// The last response body, truncated and redacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

/// The arguments of one delivery job: what it needs to build the payload and send it.
///
/// Deliberately carries the event's after-image, not the built payload: the payload is built at
/// delivery time so the webhook owner's current read rights decide which fields go out. Secrets
/// are never here; they are opened from the webhook record at delivery time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryJob {
    /// The webhook to deliver to.
    pub webhook_id: WebhookId,
    /// The delivery series this attempt belongs to.
    pub delivery_id: DeliveryId,
    /// The event id.
    pub event_id: String,
    /// The event topic.
    pub topic: Topic,
    /// The DocType of the subject.
    pub doctype: String,
    /// The document id.
    pub document_id: String,
    /// The document version.
    pub document_version: u64,
    /// The event name.
    pub event_type: EventType,
    /// When the event happened.
    pub occurred_at: OffsetDateTime,
    /// The event's after-image.
    pub after_image: Value,
    /// The event's before-image, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_image: Option<Value>,
    /// The attempt number this job represents (1 for the first).
    pub attempt: u32,
    /// A payload to send verbatim instead of building one (replay and test pings), so a replay
    /// resends the original bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_override: Option<Value>,
}

/// Circuit breaker configuration for one endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Consecutive failures before the breaker opens.
    pub failure_threshold: u32,
    /// How long the breaker stays open before a probe, in seconds.
    pub open_secs: u64,
    /// Probe calls allowed while half-open.
    pub half_open_probes: u32,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            open_secs: 30,
            half_open_probes: 1,
        }
    }
}

/// Bulkhead configuration: how many deliveries may be in flight to one endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BulkheadConfig {
    /// Maximum concurrent deliveries per endpoint.
    pub max_concurrent: u32,
}

impl Default for BulkheadConfig {
    fn default() -> Self {
        Self { max_concurrent: 10 }
    }
}

/// Per-endpoint rate limit: how many deliveries may start per second.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RateLimitConfig {
    /// Deliveries per second; `0` means unlimited.
    pub per_second: u32,
}
