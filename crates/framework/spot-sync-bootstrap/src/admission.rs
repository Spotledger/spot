//! Load control (ADR-066 section 7): many nodes returning at once must not all bootstrap at once.
//!
//! The hub admits a limited number of concurrent bootstraps and tells the others when to come back,
//! with a delay spread by a hash of the node id so 100 nodes do not return in the same millisecond.
//! Per node, a token bucket paces chunks. There is no clock here: `now_ms` is an argument, and
//! every number is an integer.

use crate::error::BootstrapError;
use crate::hash::of_str;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The answer to a request for work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    /// Go ahead.
    Admit,
    /// Come back after this many milliseconds.
    RetryAfter(u64),
}

/// The tuning values. ADR-066 section 7 starts at 8 concurrent catch-ups per tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionSettings {
    /// Most bootstraps running at once.
    pub max_concurrent_bootstraps: usize,
    /// The least a refused node waits, in milliseconds.
    pub retry_base_ms: u64,
    /// The jitter window added on top, in milliseconds. The node id picks the spot in it.
    pub retry_jitter_ms: u64,
    /// One chunk token is added per node every this many milliseconds.
    pub chunk_interval_ms: u64,
    /// The most chunk tokens a node can hold (the burst).
    pub chunk_burst: u32,
}

impl Default for AdmissionSettings {
    fn default() -> Self {
        Self {
            max_concurrent_bootstraps: 8,
            retry_base_ms: 5_000,
            retry_jitter_ms: 55_000,
            chunk_interval_ms: 250,
            chunk_burst: 4,
        }
    }
}

impl AdmissionSettings {
    /// Whether the settings can run.
    ///
    /// # Errors
    /// `InvalidSettings` for a zero concurrency, interval or burst.
    pub fn validate(&self) -> Result<(), BootstrapError> {
        if self.max_concurrent_bootstraps == 0 {
            return Err(BootstrapError::InvalidSettings {
                reason: "at least one bootstrap must be allowed at a time",
            });
        }
        if self.chunk_interval_ms == 0 || self.chunk_burst == 0 {
            return Err(BootstrapError::InvalidSettings {
                reason: "the chunk interval and burst must be above zero",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: u32,
    /// The time up to which refills have been counted.
    at_ms: i64,
}

/// The admission state of one tenant's hub.
#[derive(Debug, Clone)]
pub struct Admission {
    settings: AdmissionSettings,
    active: BTreeMap<String, Bucket>,
}

impl Admission {
    /// A controller with nothing running.
    ///
    /// # Errors
    /// `InvalidSettings`.
    pub fn new(settings: AdmissionSettings) -> Result<Self, BootstrapError> {
        settings.validate()?;
        Ok(Self {
            settings,
            active: BTreeMap::new(),
        })
    }

    /// How many bootstraps are running.
    #[must_use]
    pub fn running(&self) -> usize {
        self.active.len()
    }

    /// The delay a refused node is told: base plus a spot in the jitter window chosen by its id.
    /// The same node always gets the same spot, and different nodes spread across the window.
    #[must_use]
    pub fn retry_delay_ms(&self, node_id: &str) -> u64 {
        let window = self.settings.retry_jitter_ms;
        let jitter = if window == 0 {
            0
        } else {
            of_str(node_id) % window
        };
        self.settings.retry_base_ms.saturating_add(jitter)
    }

    /// Ask to start (or continue) a bootstrap. A node that is already running is admitted again
    /// (its session resumed after a cut), so a retry never takes a second slot.
    pub fn admit(&mut self, node_id: &str, now_ms: i64) -> Decision {
        if self.active.contains_key(node_id) {
            return Decision::Admit;
        }
        if self.active.len() >= self.settings.max_concurrent_bootstraps {
            return Decision::RetryAfter(self.retry_delay_ms(node_id));
        }
        self.active.insert(
            node_id.to_owned(),
            Bucket {
                tokens: self.settings.chunk_burst,
                at_ms: now_ms,
            },
        );
        Decision::Admit
    }

    /// Ask to send the next chunk to an admitted node. A token bucket per node: it refills one
    /// token every `chunk_interval_ms`, up to the burst.
    ///
    /// # Errors
    /// `NotAdmitted` for a node that holds no slot.
    pub fn chunk(&mut self, node_id: &str, now_ms: i64) -> Result<Decision, BootstrapError> {
        let interval = self.settings.chunk_interval_ms;
        let burst = self.settings.chunk_burst;
        let bucket = self
            .active
            .get_mut(node_id)
            .ok_or(BootstrapError::NotAdmitted)?;
        let elapsed = u64::try_from(now_ms.saturating_sub(bucket.at_ms)).unwrap_or(0);
        let gained = elapsed / interval;
        if gained > 0 {
            let gained = u32::try_from(gained).unwrap_or(u32::MAX);
            bucket.tokens = bucket.tokens.saturating_add(gained).min(burst);
            // Keep the remainder of the interval when still short of the burst.
            bucket.at_ms = if bucket.tokens == burst {
                now_ms
            } else {
                bucket
                    .at_ms
                    .saturating_add(i64::try_from(u64::from(gained) * interval).unwrap_or(i64::MAX))
            };
        }
        if bucket.tokens > 0 {
            bucket.tokens -= 1;
            return Ok(Decision::Admit);
        }
        let waited = u64::try_from(now_ms.saturating_sub(bucket.at_ms)).unwrap_or(0);
        Ok(Decision::RetryAfter(interval.saturating_sub(waited).max(1)))
    }

    /// The node finished, failed or gave up: free its slot.
    pub fn release(&mut self, node_id: &str) {
        self.active.remove(node_id);
    }
}
