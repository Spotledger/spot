//! The store's configuration: the retention policy (per caller class, never a constant), the
//! reservation lease, the wait budget and the body cap. Plain data; the host builds it from its
//! own settings (WP-PLT-05.T9 parses them), this crate reads no file or environment variable.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use spot_http::api::DEFAULT_MAX_BODY_BYTES;

/// The shortest reservation lease the store accepts. A lease shorter than a request can run
/// would let a second node take over a live handler.
pub const MIN_LEASE: Duration = Duration::from_secs(30);

/// How long a completed response is kept when no setting says otherwise (ADR-055): 24 hours.
pub const DEFAULT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest any class may keep a response when no setting says otherwise: 7 days.
pub const DEFAULT_MAX_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How long a duplicate waits for the first request before the router answers 503 (25 s).
pub const DEFAULT_WAIT_BUDGET: Duration = Duration::from_secs(25);

/// The longest one statement (or pool wait) may take before the store reports itself
/// unavailable (10 s). A database that stops answering must close the request, not hold it.
pub const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest response body the table accepts, whatever the setting (1 MiB, also a table
/// constraint).
pub const MAX_BODY_LIMIT: usize = 1_048_576;

/// A setting the store refuses to start with. The text names the setting, never a value of a
/// request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The default retention is shorter than one second.
    ZeroDefaultRetention,
    /// The maximum retention is below the default retention.
    MaxBelowDefault,
    /// A class retention is shorter than one second (it would never replay anything).
    ZeroClassRetention,
    /// The lease is below [`MIN_LEASE`].
    LeaseTooShort,
    /// The wait budget is zero.
    ZeroWaitBudget,
    /// The operation timeout is zero.
    ZeroOperationTimeout,
    /// The body cap is zero or above [`MAX_BODY_LIMIT`].
    BodyCapOutOfRange,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ConfigError::ZeroDefaultRetention => {
                "the default idempotency retention must be at least one second"
            }
            ConfigError::MaxBelowDefault => {
                "the maximum idempotency retention is below the default retention"
            }
            ConfigError::ZeroClassRetention => {
                "an idempotency retention class must keep responses at least one second"
            }
            ConfigError::LeaseTooShort => {
                "the idempotency reservation lease must be at least 30 seconds"
            }
            ConfigError::ZeroWaitBudget => "the idempotency wait budget must not be zero",
            ConfigError::ZeroOperationTimeout => {
                "the idempotency operation timeout must not be zero"
            }
            ConfigError::BodyCapOutOfRange => {
                "the idempotency body cap must be between 1 byte and 1 MiB"
            }
        })
    }
}

impl std::error::Error for ConfigError {}

/// How long a completed response is kept, by caller class.
///
/// A site configures a `default`, a `max` and any number of named `classes`. The router names a
/// class per route group (`http_api`, `http_upload`); a class is chosen **by name only**, never
/// by a duration taken from a request or a header. An unknown class gets the default; every
/// duration is clamped to `max`. A caller that needs longer than `max` keeps its own outcome.
///
/// The policy is pure: [`RetentionPolicy::retain_until`] takes time as an argument. The store
/// records the result in the row's own `expires_at` when it completes, so changing the policy
/// later never shortens or extends a row already written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    default: Duration,
    max: Duration,
    classes: BTreeMap<String, Duration>,
}

impl RetentionPolicy {
    /// A policy, or the reason it is refused: a zero `default`, a `max` below `default`, or a
    /// class shorter than one second. A class longer than `max` is not an error; it clamps.
    ///
    /// # Errors
    /// [`ConfigError`] naming the first refused setting.
    pub fn new(
        default: Duration,
        max: Duration,
        classes: BTreeMap<String, Duration>,
    ) -> Result<Self, ConfigError> {
        if default.as_secs() == 0 {
            return Err(ConfigError::ZeroDefaultRetention);
        }
        if max < default {
            return Err(ConfigError::MaxBelowDefault);
        }
        if classes.values().any(|keep| keep.as_secs() == 0) {
            return Err(ConfigError::ZeroClassRetention);
        }
        Ok(Self {
            default,
            max,
            classes,
        })
    }

    /// The documented defaults: 24 hours, capped at 7 days, no class overrides.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            default: DEFAULT_RETENTION,
            max: DEFAULT_MAX_RETENTION,
            classes: BTreeMap::new(),
        }
    }

    /// The default retention.
    #[must_use]
    pub fn default_retention(&self) -> Duration {
        self.default
    }

    /// The maximum retention.
    #[must_use]
    pub fn max_retention(&self) -> Duration {
        self.max
    }

    /// How long `class` keeps a response: its override, else the default, clamped to `max`.
    #[must_use]
    pub fn retention_of(&self, class: &str) -> Duration {
        self.classes
            .get(class)
            .copied()
            .unwrap_or(self.default)
            .min(self.max)
    }

    /// The Unix second at which a response completed at `now` under `class` stops being
    /// replayed.
    #[must_use]
    pub fn retain_until(&self, class: &str, now: i64) -> i64 {
        let keep = i64::try_from(self.retention_of(class).as_secs()).unwrap_or(i64::MAX);
        now.saturating_add(keep)
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self::standard()
    }
}

/// Everything [`PgIdempotencyStore::new`](crate::PgIdempotencyStore::new) takes besides the pool
/// and the clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgIdempotencyConfig {
    /// How long a pending reservation holds its key before another request may take it over
    /// (a crashed node's key is free again after this, not after 24 h). At least [`MIN_LEASE`];
    /// the host passes more than its HTTP request timeout (WP-PLT-05.T9: the larger of twice
    /// the timeout and 120 s).
    pub lease: Duration,
    /// How long a duplicate waits for the first request before the router answers 503.
    pub wait_budget: Duration,
    /// The largest response body written; a larger one is released, not stored.
    pub max_body_bytes: usize,
    /// How long one statement (or a pool wait) may take before the store reports itself
    /// unavailable.
    pub operation_timeout: Duration,
    /// How long a completed response is kept, by caller class.
    pub retention: RetentionPolicy,
}

impl PgIdempotencyConfig {
    /// A configuration with `lease` and `retention`; the wait budget (25 s), body cap
    /// (256 KiB) and operation timeout (10 s) take their documented defaults.
    #[must_use]
    pub fn new(lease: Duration, retention: RetentionPolicy) -> Self {
        Self {
            lease,
            wait_budget: DEFAULT_WAIT_BUDGET,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            retention,
        }
    }

    /// Replaces the wait budget.
    #[must_use]
    pub fn with_wait_budget(mut self, wait_budget: Duration) -> Self {
        self.wait_budget = wait_budget;
        self
    }

    /// Replaces the body cap.
    #[must_use]
    pub fn with_max_body_bytes(mut self, max_body_bytes: usize) -> Self {
        self.max_body_bytes = max_body_bytes;
        self
    }

    /// Replaces the per-operation timeout.
    #[must_use]
    pub fn with_operation_timeout(mut self, operation_timeout: Duration) -> Self {
        self.operation_timeout = operation_timeout;
        self
    }

    /// Checks every setting.
    ///
    /// # Errors
    /// [`ConfigError`] naming the first refused setting.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.lease < MIN_LEASE {
            return Err(ConfigError::LeaseTooShort);
        }
        if self.wait_budget.is_zero() {
            return Err(ConfigError::ZeroWaitBudget);
        }
        if self.operation_timeout.is_zero() {
            return Err(ConfigError::ZeroOperationTimeout);
        }
        if self.max_body_bytes == 0 || self.max_body_bytes > MAX_BODY_LIMIT {
            return Err(ConfigError::BodyCapOutOfRange);
        }
        // The retention policy was validated when it was built, and its fields are private.
        Ok(())
    }
}
