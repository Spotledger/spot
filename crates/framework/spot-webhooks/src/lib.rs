//! Outbound webhooks with guaranteed delivery from the outbox (WP-INT-01.T1).
//!
//! A site registers a [`Webhook`](model::Webhook): a target URL, the DocTypes and event names
//! it listens to, an optional [`condition`] in the shared expression language, a payload mode,
//! a sealed signing secret and custom headers. Every committed event that matches
//! ([`intake`]) becomes one delivery [job](spot_jobs) per webhook, keyed so a replayed intake
//! creates no duplicate. The [delivery](delivery) handler signs each attempt, sends it as a
//! `POST` through the [`spot-egress`] connector only, and records the outcome in a
//! [delivery log](delivery::DeliveryLog) with bounded retries, a dead-letter state and a
//! per-endpoint circuit breaker and bulkhead ([`resilience`]).
//!
//! The verifying side of the signature is public ([`signing::verify_signature`]); a receiver
//! example in three languages is in `docs/receiver-example.md`.

#![warn(missing_docs)]

pub mod condition;
pub mod config;
pub mod delivery;
pub mod error;
pub mod intake;
pub mod model;
pub mod payload;
pub mod permissions;
pub mod replay;
pub mod resilience;
pub mod signing;
pub mod webhook;

#[cfg(test)]
mod fixtures;

#[cfg(test)]
mod delivery_tests;

pub use condition::{evaluate_condition, validate_condition};
pub use config::WebhookConfig;
pub use delivery::{
    DeliveryHandler, DeliveryLog, DeliveryOutcome, Egress, InMemoryDeliveryLog,
    SealedSecretResolver, SecretResolver, SpotEgress,
};
pub use error::{Error, Result, WebhookErrorCode};
pub use intake::{CommittedEvent, IntakeReport, RejectedEvent, WebhookIntake};
pub use model::{
    BulkheadConfig, CircuitBreakerConfig, DeliveryId, DeliveryJob, DeliveryStatus, DeliverySummary,
    EventType, Header, PayloadMode, PreviousSecret, RateLimitConfig, SealedSecret, Webhook,
    WebhookDelivery, WebhookId, WebhookState,
};
pub use payload::{PayloadBuilder, build_payload};
pub use permissions::{AllFieldsReadable, FieldVisibility, PermissionFieldVisibility};
pub use replay::{replay_delivery, test_webhook};
pub use resilience::Resilience;
pub use signing::{SignatureVerifier, SigningKey, verify_signature};
pub use webhook::{InMemoryWebhookStore, WebhookService, WebhookStore};

/// The job name delivery jobs are registered under (`<app>:<job>`).
pub const WEBHOOK_JOB_NAME: &str = "webhooks:deliver";

/// The queue delivery jobs run on.
pub const WEBHOOK_QUEUE: &str = "webhooks";

/// The default tolerance for signature verification, in seconds (five minutes).
pub const DEFAULT_SIGNATURE_TOLERANCE_SECS: i64 = 300;

/// The maximum payload size before truncation, in bytes (1 MiB).
pub const MAX_PAYLOAD_SIZE_BYTES: usize = 1_048_576;

/// The maximum response body stored in the delivery log, in bytes (10 KiB).
pub const MAX_RESPONSE_BODY_LOG_BYTES: usize = 10_240;

/// The signature header: `t=<unix>,v1=<hex hmac-sha256>`.
pub const SIGNATURE_HEADER: &str = "X-Spot-Signature";

/// The event id header.
pub const EVENT_ID_HEADER: &str = "X-Spot-Event-Id";

/// The delivery id header.
pub const DELIVERY_HEADER: &str = "X-Spot-Delivery";

/// The user agent every delivery carries.
pub const USER_AGENT: &str = "Spotledger-Webhooks/1.0";
