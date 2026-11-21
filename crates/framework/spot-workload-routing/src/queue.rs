//! Job queue names: the unit of queue affinity (ADR-24).
//!
//! A queue name is a plain, non-empty identifier of a class of jobs a worker consumes, in the
//! same shape as [`spot_jobs::QueueName`](https://docs.rs/spot-jobs) so a host maps one to the
//! other with no translation. The routing engine decides *which* queues a node's workers
//! consume; the job engine consumes them.

use serde::{Deserialize, Serialize};

/// The longest queue name, in bytes. A queue name is a short identifier, never free text.
pub const MAX_QUEUE_NAME_BYTES: usize = 200;

/// A queue name, for example `default`, `interactive`, `reports` or `exports`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueueName(String);

impl QueueName {
    /// A queue name. Validation happens in [`crate::policy::RoutingPolicy::validate`], which
    /// reports every offending name at once, rather than in the constructor.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name as an owned string (what `spot-jobs`' worker startup takes).
    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }

    /// Whether the name is well formed: non-empty, at most [`MAX_QUEUE_NAME_BYTES`] bytes and
    /// printable (no control characters).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.0.is_empty()
            && self.0.len() <= MAX_QUEUE_NAME_BYTES
            && !self.0.chars().any(char::is_control)
    }
}

impl From<&str> for QueueName {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<String> for QueueName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for QueueName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
