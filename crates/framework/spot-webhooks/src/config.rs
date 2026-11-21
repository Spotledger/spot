//! Configuration for the webhooks crate.

use serde::{Deserialize, Serialize};

use crate::model::{BulkheadConfig, CircuitBreakerConfig, RateLimitConfig};

/// Webhook configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Default timeout for webhook requests in seconds.
    #[serde(default = "default_timeout_secs")]
    pub default_timeout_secs: u64,
    /// Default maximum delivery attempts.
    #[serde(default = "default_max_attempts")]
    pub default_max_attempts: u32,
    /// Maximum payload size in bytes before truncation.
    #[serde(default = "default_max_payload_size_bytes")]
    pub max_payload_size_bytes: usize,
    /// Maximum response body size kept in the delivery log, in bytes.
    #[serde(default = "default_max_response_body_log_bytes")]
    pub max_response_body_log_bytes: usize,
    /// Default signature tolerance in seconds.
    #[serde(default = "default_signature_tolerance_secs")]
    pub default_signature_tolerance_secs: i64,
    /// Secret rotation overlap period in seconds.
    #[serde(default = "default_secret_rotation_overlap_secs")]
    pub secret_rotation_overlap_secs: i64,
    /// Circuit breaker configuration.
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
    /// Bulkhead configuration.
    #[serde(default)]
    pub bulkhead: BulkheadConfig,
    /// Per-endpoint rate limit.
    #[serde(default)]
    pub rate: RateLimitConfig,
}

fn default_timeout_secs() -> u64 {
    30
}
fn default_max_attempts() -> u32 {
    5
}
fn default_max_payload_size_bytes() -> usize {
    crate::MAX_PAYLOAD_SIZE_BYTES
}
fn default_max_response_body_log_bytes() -> usize {
    crate::MAX_RESPONSE_BODY_LOG_BYTES
}
fn default_signature_tolerance_secs() -> i64 {
    crate::DEFAULT_SIGNATURE_TOLERANCE_SECS
}
fn default_secret_rotation_overlap_secs() -> i64 {
    86_400
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 30,
            default_max_attempts: 5,
            max_payload_size_bytes: crate::MAX_PAYLOAD_SIZE_BYTES,
            max_response_body_log_bytes: crate::MAX_RESPONSE_BODY_LOG_BYTES,
            default_signature_tolerance_secs: crate::DEFAULT_SIGNATURE_TOLERANCE_SECS,
            secret_rotation_overlap_secs: 86_400,
            circuit_breaker: CircuitBreakerConfig::default(),
            bulkhead: BulkheadConfig::default(),
            rate: RateLimitConfig::default(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_stable() {
        let config = WebhookConfig::default();
        assert_eq!(config.default_timeout_secs, 30);
        assert_eq!(config.default_max_attempts, 5);
        assert_eq!(config.default_signature_tolerance_secs, 300);
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = WebhookConfig::default();
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("default_timeout_secs = 30"));
        assert!(text.contains("default_max_attempts = 5"));
    }
}
