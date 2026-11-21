//! Webhook error types, their stable catalogue codes and their app-API class.

use spot_app_api::{AppError, ErrorClass};
use thiserror::Error;

/// Result type for webhook operations.
pub type Result<T> = std::result::Result<T, Error>;

/// The stable catalogue codes this crate raises (`webhook.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebhookErrorCode {
    /// The webhook was not found.
    NotFound,
    /// The webhook is disabled.
    Disabled,
    /// The condition expression is invalid.
    InvalidCondition,
    /// The payload mode or its allow-list is invalid.
    InvalidPayloadMode,
    /// The signing secret is missing or cannot be opened.
    InvalidSecret,
    /// The URL is invalid.
    InvalidUrl,
    /// The headers are invalid.
    InvalidHeaders,
    /// The timeout is invalid.
    InvalidTimeout,
    /// The max attempts value is invalid.
    InvalidMaxAttempts,
    /// The event filter is invalid.
    InvalidFilter,
    /// The delivery was not found.
    DeliveryNotFound,
    /// The delivery failed permanently.
    DeliveryFailed,
    /// The delivery was blocked by egress policy.
    DeliveryBlocked,
    /// The delivery was rate limited.
    DeliveryRateLimited,
    /// The endpoint's circuit breaker is open.
    CircuitOpen,
    /// The endpoint's bulkhead is full.
    BulkheadFull,
    /// Signature verification failed.
    SignatureVerificationFailed,
    /// Secret rotation failed.
    SecretRotationFailed,
    /// The payload is too large.
    PayloadTooLarge,
    /// The condition evaluation failed.
    ConditionEvaluationFailed,
    /// The intake failed.
    IntakeFailed,
    /// The replay failed.
    ReplayFailed,
    /// The test ping failed.
    TestFailed,
    /// The configuration is invalid.
    InvalidConfig,
    /// The webhook is dead-lettered.
    DeadLetter,
}

impl WebhookErrorCode {
    /// The stable dotted code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "webhook.not_found",
            Self::Disabled => "webhook.disabled",
            Self::InvalidCondition => "webhook.invalid_condition",
            Self::InvalidPayloadMode => "webhook.invalid_payload_mode",
            Self::InvalidSecret => "webhook.invalid_secret",
            Self::InvalidUrl => "webhook.invalid_url",
            Self::InvalidHeaders => "webhook.invalid_headers",
            Self::InvalidTimeout => "webhook.invalid_timeout",
            Self::InvalidMaxAttempts => "webhook.invalid_max_attempts",
            Self::InvalidFilter => "webhook.invalid_filter",
            Self::DeliveryNotFound => "webhook.delivery_not_found",
            Self::DeliveryFailed => "webhook.delivery_failed",
            Self::DeliveryBlocked => "webhook.delivery_blocked",
            Self::DeliveryRateLimited => "webhook.delivery_rate_limited",
            Self::CircuitOpen => "webhook.circuit_open",
            Self::BulkheadFull => "webhook.bulkhead_full",
            Self::SignatureVerificationFailed => "webhook.signature_verification_failed",
            Self::SecretRotationFailed => "webhook.secret_rotation_failed",
            Self::PayloadTooLarge => "webhook.payload_too_large",
            Self::ConditionEvaluationFailed => "webhook.condition_evaluation_failed",
            Self::IntakeFailed => "webhook.intake_failed",
            Self::ReplayFailed => "webhook.replay_failed",
            Self::TestFailed => "webhook.test_failed",
            Self::InvalidConfig => "webhook.invalid_config",
            Self::DeadLetter => "webhook.dead_letter",
        }
    }

    /// The app-API class the code is reported as.
    pub fn class(self) -> ErrorClass {
        match self {
            Self::NotFound | Self::DeliveryNotFound => ErrorClass::NotFound,
            Self::Disabled | Self::DeliveryBlocked | Self::SignatureVerificationFailed => {
                ErrorClass::PermissionDenied
            }
            Self::DeliveryRateLimited | Self::CircuitOpen | Self::BulkheadFull => {
                ErrorClass::Unavailable
            }
            Self::InvalidCondition
            | Self::InvalidPayloadMode
            | Self::InvalidUrl
            | Self::InvalidHeaders
            | Self::InvalidTimeout
            | Self::InvalidMaxAttempts
            | Self::InvalidFilter
            | Self::PayloadTooLarge
            | Self::ConditionEvaluationFailed
            | Self::InvalidConfig
            | Self::ReplayFailed => ErrorClass::Validation,
            Self::InvalidSecret
            | Self::DeliveryFailed
            | Self::SecretRotationFailed
            | Self::IntakeFailed
            | Self::TestFailed
            | Self::DeadLetter => ErrorClass::Internal,
        }
    }
}

/// Errors raised by the webhook crate.
#[derive(Debug, Error)]
pub enum Error {
    /// The webhook was not found.
    #[error("Webhook not found: {id}")]
    NotFound {
        /// The webhook id.
        id: String,
    },
    /// The webhook is disabled.
    #[error("Webhook is disabled: {id}")]
    Disabled {
        /// The webhook id.
        id: String,
    },
    /// The condition expression is invalid.
    #[error("Invalid condition expression: {reason}")]
    InvalidCondition {
        /// Why.
        reason: String,
    },
    /// The payload mode or allow-list is invalid.
    #[error("Invalid payload mode: {mode}")]
    InvalidPayloadMode {
        /// Why.
        mode: String,
    },
    /// The signing secret is missing or cannot be opened.
    #[error("Invalid secret: {reason}")]
    InvalidSecret {
        /// Why.
        reason: String,
    },
    /// The URL is invalid.
    #[error("Invalid URL: {reason}")]
    InvalidUrl {
        /// Why.
        reason: String,
    },
    /// The headers are invalid.
    #[error("Invalid headers: {reason}")]
    InvalidHeaders {
        /// Why.
        reason: String,
    },
    /// The timeout is invalid.
    #[error("Invalid timeout: {reason}")]
    InvalidTimeout {
        /// Why.
        reason: String,
    },
    /// The max attempts value is invalid.
    #[error("Invalid max attempts: {reason}")]
    InvalidMaxAttempts {
        /// Why.
        reason: String,
    },
    /// The event filter is invalid.
    #[error("Invalid filter: {reason}")]
    InvalidFilter {
        /// Why.
        reason: String,
    },
    /// The delivery was not found.
    #[error("Delivery not found: {id}")]
    DeliveryNotFound {
        /// The delivery id.
        id: String,
    },
    /// The delivery failed permanently.
    #[error("Delivery failed after {attempts} attempts: {last_error}")]
    DeliveryFailed {
        /// The attempts made.
        attempts: u32,
        /// The last error.
        last_error: String,
    },
    /// The delivery was blocked by egress policy.
    #[error("Delivery blocked by egress policy: {reason}")]
    DeliveryBlocked {
        /// Why.
        reason: String,
    },
    /// The delivery was rate limited.
    #[error("Delivery rate limited: retry after {retry_after_secs} seconds")]
    DeliveryRateLimited {
        /// The server's requested delay.
        retry_after_secs: u64,
    },
    /// The endpoint's circuit breaker is open.
    #[error("Circuit open for endpoint {endpoint}: retry after {retry_after_secs} seconds")]
    CircuitOpen {
        /// The endpoint.
        endpoint: String,
        /// How long until a probe.
        retry_after_secs: u64,
    },
    /// The endpoint's bulkhead is full.
    #[error("Bulkhead full for endpoint {endpoint}")]
    BulkheadFull {
        /// The endpoint.
        endpoint: String,
    },
    /// Signature verification failed.
    #[error("Signature verification failed: {reason}")]
    SignatureVerificationFailed {
        /// Why.
        reason: String,
    },
    /// Secret rotation failed.
    #[error("Secret rotation failed: {reason}")]
    SecretRotationFailed {
        /// Why.
        reason: String,
    },
    /// The payload is too large.
    #[error("Payload too large: {size} bytes (max {max} bytes)")]
    PayloadTooLarge {
        /// The size.
        size: usize,
        /// The cap.
        max: usize,
    },
    /// The condition evaluation failed.
    #[error("Condition evaluation failed: {reason}")]
    ConditionEvaluationFailed {
        /// Why.
        reason: String,
    },
    /// The intake failed.
    #[error("Intake failed: {reason}")]
    IntakeFailed {
        /// Why.
        reason: String,
    },
    /// The replay failed.
    #[error("Replay failed: {reason}")]
    ReplayFailed {
        /// Why.
        reason: String,
    },
    /// The test ping failed.
    #[error("Test failed: {reason}")]
    TestFailed {
        /// Why.
        reason: String,
    },
    /// The configuration is invalid.
    #[error("Invalid configuration: {reason}")]
    InvalidConfig {
        /// Why.
        reason: String,
    },
    /// The webhook is dead-lettered.
    #[error("Webhook is in dead-letter state: {id}")]
    DeadLetter {
        /// The webhook id.
        id: String,
    },
    /// Any other underlying error.
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl Error {
    /// The stable catalogue code.
    pub fn code(&self) -> WebhookErrorCode {
        match self {
            Self::NotFound { .. } => WebhookErrorCode::NotFound,
            Self::Disabled { .. } => WebhookErrorCode::Disabled,
            Self::InvalidCondition { .. } => WebhookErrorCode::InvalidCondition,
            Self::InvalidPayloadMode { .. } => WebhookErrorCode::InvalidPayloadMode,
            Self::InvalidSecret { .. } => WebhookErrorCode::InvalidSecret,
            Self::InvalidUrl { .. } => WebhookErrorCode::InvalidUrl,
            Self::InvalidHeaders { .. } => WebhookErrorCode::InvalidHeaders,
            Self::InvalidTimeout { .. } => WebhookErrorCode::InvalidTimeout,
            Self::InvalidMaxAttempts { .. } => WebhookErrorCode::InvalidMaxAttempts,
            Self::InvalidFilter { .. } => WebhookErrorCode::InvalidFilter,
            Self::DeliveryNotFound { .. } => WebhookErrorCode::DeliveryNotFound,
            Self::DeliveryFailed { .. } => WebhookErrorCode::DeliveryFailed,
            Self::DeliveryBlocked { .. } => WebhookErrorCode::DeliveryBlocked,
            Self::DeliveryRateLimited { .. } => WebhookErrorCode::DeliveryRateLimited,
            Self::CircuitOpen { .. } => WebhookErrorCode::CircuitOpen,
            Self::BulkheadFull { .. } => WebhookErrorCode::BulkheadFull,
            Self::SignatureVerificationFailed { .. } => {
                WebhookErrorCode::SignatureVerificationFailed
            }
            Self::SecretRotationFailed { .. } => WebhookErrorCode::SecretRotationFailed,
            Self::PayloadTooLarge { .. } => WebhookErrorCode::PayloadTooLarge,
            Self::ConditionEvaluationFailed { .. } => WebhookErrorCode::ConditionEvaluationFailed,
            Self::IntakeFailed { .. } => WebhookErrorCode::IntakeFailed,
            Self::ReplayFailed { .. } => WebhookErrorCode::ReplayFailed,
            Self::TestFailed { .. } => WebhookErrorCode::TestFailed,
            Self::InvalidConfig { .. } => WebhookErrorCode::InvalidConfig,
            Self::DeadLetter { .. } => WebhookErrorCode::DeadLetter,
            Self::Other(_) => WebhookErrorCode::InvalidConfig,
        }
    }

    /// The app-API error this maps to.
    pub fn to_app_error(&self) -> AppError {
        let code = self.code();
        AppError::new(code.class(), code.as_str(), self.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Other(Box::new(e))
    }
}

impl From<spot_jobs::Error> for Error {
    fn from(e: spot_jobs::Error) -> Self {
        Error::Other(Box::new(e))
    }
}

impl From<spot_egress::EgressError> for Error {
    fn from(e: spot_egress::EgressError) -> Self {
        match e {
            spot_egress::EgressError::PolicyDenied
            | spot_egress::EgressError::AddressDenied { .. }
            | spot_egress::EgressError::PortDenied { .. }
            | spot_egress::EgressError::RedirectDenied => Error::DeliveryBlocked {
                reason: e.code().to_string(),
            },
            other => Error::Other(Box::new(other)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_map_to_classes() {
        assert_eq!(
            Error::NotFound { id: "x".into() }.code().as_str(),
            "webhook.not_found"
        );
        assert_eq!(
            Error::NotFound { id: "x".into() }.code().class(),
            ErrorClass::NotFound
        );
        assert_eq!(
            Error::DeliveryBlocked { reason: "p".into() }.code().class(),
            ErrorClass::PermissionDenied
        );
        assert_eq!(
            Error::CircuitOpen {
                endpoint: "e".into(),
                retry_after_secs: 1
            }
            .code()
            .class(),
            ErrorClass::Unavailable
        );
    }

    #[test]
    fn to_app_error_carries_the_code() {
        let err = Error::InvalidUrl {
            reason: "bad".into(),
        };
        let app = err.to_app_error();
        assert_eq!(app.code, "webhook.invalid_url");
        assert_eq!(app.class, ErrorClass::Validation);
    }

    #[test]
    fn every_code_is_dotted_lower_case() {
        for code in [
            WebhookErrorCode::NotFound,
            WebhookErrorCode::DeliveryBlocked,
            WebhookErrorCode::BulkheadFull,
            WebhookErrorCode::DeadLetter,
        ] {
            assert!(
                spot_app_api::error::is_code(code.as_str()),
                "{}",
                code.as_str()
            );
        }
    }
}
