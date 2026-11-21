//! Queue policies: concurrency, retry, timeout and rate limits.

use crate::backoff::Backoff;
use std::time::Duration;

/// A queue name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QueueName(pub String);

impl QueueName {
    /// Create a queue name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Rate limiting policy for a queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimit {
    /// Jobs per second.
    pub per_second: u32,
}

/// Policy for executing a queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuePolicy {
    /// Name of the queue.
    pub name: String,
    /// Concurrent jobs per worker node.
    pub concurrency: u32,
    /// Maximum concurrent jobs per site (across all nodes).
    pub max_per_site_running: u32,
    /// Priority weight for work stealing (higher = more preferred).
    pub priority_weight: u32,
    /// Timeout per job on this queue.
    pub timeout: Duration,
    /// Maximum attempts before dead-lettering.
    pub max_attempts: u32,
    /// Backoff policy for retries.
    pub backoff: Backoff,
    /// Optional rate limiting.
    pub rate: Option<RateLimit>,
}

impl Default for QueuePolicy {
    fn default() -> Self {
        Self {
            name: "default".to_string(),
            concurrency: 10,
            max_per_site_running: 1000,
            priority_weight: 1,
            timeout: Duration::from_secs(30),
            max_attempts: 3,
            backoff: Backoff::default(),
            rate: None,
        }
    }
}

impl QueuePolicy {
    /// Creates the default queue policy.
    pub fn new() -> Self {
        Self::default()
    }

    /// The short queue policy (fast jobs).
    pub fn short() -> Self {
        Self {
            name: "short".to_string(),
            concurrency: 20,
            max_per_site_running: 500,
            priority_weight: 2,
            timeout: Duration::from_secs(5),
            max_attempts: 2,
            backoff: Backoff {
                base: Duration::from_millis(100),
                max: Duration::from_secs(10),
                exponent: 2,
            },
            rate: None,
        }
    }

    /// The long queue policy (slow, best-effort work).
    pub fn long() -> Self {
        Self {
            name: "long".to_string(),
            concurrency: 5,
            max_per_site_running: 500,
            priority_weight: 1,
            timeout: Duration::from_secs(600), // 10 minutes
            max_attempts: 5,
            backoff: Backoff {
                base: Duration::from_secs(10),
                max: Duration::from_secs(3600), // 1 hour
                exponent: 2,
            },
            rate: None,
        }
    }

    /// Create a custom queue policy.
    pub fn custom(name: impl Into<String>) -> QueuePolicyBuilder {
        QueuePolicyBuilder {
            name: name.into(),
            concurrency: 10,
            max_per_site_running: 1000,
            priority_weight: 1,
            timeout: Duration::from_secs(30),
            max_attempts: 3,
            backoff: Backoff::default(),
            rate: None,
        }
    }
}

/// Builder for custom queue policies.
pub struct QueuePolicyBuilder {
    name: String,
    concurrency: u32,
    max_per_site_running: u32,
    priority_weight: u32,
    timeout: Duration,
    max_attempts: u32,
    backoff: Backoff,
    rate: Option<RateLimit>,
}

impl QueuePolicyBuilder {
    /// Set the concurrency limit.
    pub fn with_concurrency(mut self, n: u32) -> Self {
        self.concurrency = n;
        self
    }

    /// Set the max per site limit.
    pub fn with_max_per_site_running(mut self, n: u32) -> Self {
        self.max_per_site_running = n;
        self
    }

    /// Set the priority weight.
    pub fn with_priority_weight(mut self, w: u32) -> Self {
        self.priority_weight = w;
        self
    }

    /// Set the timeout.
    pub fn with_timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    /// Set the max attempts.
    pub fn with_max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n;
        self
    }

    /// Set the backoff policy.
    pub fn with_backoff(mut self, b: Backoff) -> Self {
        self.backoff = b;
        self
    }

    /// Set the rate limit.
    pub fn with_rate_limit(mut self, rate: Option<RateLimit>) -> Self {
        self.rate = rate;
        self
    }

    /// Build the policy.
    pub fn build(self) -> QueuePolicy {
        QueuePolicy {
            name: self.name,
            concurrency: self.concurrency,
            max_per_site_running: self.max_per_site_running,
            priority_weight: self.priority_weight,
            timeout: self.timeout,
            max_attempts: self.max_attempts,
            backoff: self.backoff,
            rate: self.rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_policies() {
        let default = QueuePolicy::default();
        assert_eq!(default.name, "default");
        assert!(default.concurrency > 0);

        let short = QueuePolicy::short();
        assert_eq!(short.name, "short");
        assert!(short.timeout < default.timeout);

        let long = QueuePolicy::long();
        assert_eq!(long.name, "long");
        assert!(long.timeout > default.timeout);
    }

    #[test]
    fn test_custom_policy() {
        let policy = QueuePolicy::custom("my_queue")
            .with_concurrency(5)
            .with_max_attempts(10)
            .build();

        assert_eq!(policy.name, "my_queue");
        assert_eq!(policy.concurrency, 5);
        assert_eq!(policy.max_attempts, 10);
    }
}
