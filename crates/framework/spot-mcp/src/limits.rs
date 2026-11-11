//! The limits every call runs under: input size, row cap, depth cap, timeout and rate limit.

use std::time::Duration;

use spot_coordinator::RateLimit;

use crate::error::McpError;

/// The most rows one query or search returns (ADR-069's row cap).
pub const MAX_ROWS: u32 = 500;

/// The most bytes one request message may carry.
pub const MAX_INPUT_BYTES: usize = 256 * 1024;

/// The deepest a query IR expression may nest.
pub const MAX_DEPTH: usize = 32;

/// The most expression nodes one query IR may carry (ADR-069 leaves room for the permission
/// filter, whose own cap is separate).
pub const MAX_QUERY_NODES: usize = 10_000;

/// The default per-call timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// The default per-principal call rate limit: 120 calls per minute.
pub const DEFAULT_RATE: RateLimit = RateLimit {
    capacity: 120,
    per: Duration::from_secs(60),
};

/// The server's limits, all configurable by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The most bytes one request message may carry.
    pub max_input_bytes: usize,
    /// The most rows one query or search returns.
    pub max_rows: u32,
    /// The deepest a query IR expression may nest.
    pub max_depth: usize,
    /// The most expression nodes one query IR may carry.
    pub max_query_nodes: usize,
    /// The per-call timeout.
    pub call_timeout: Duration,
    /// The per-principal call rate limit.
    pub rate: RateLimit,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: MAX_INPUT_BYTES,
            max_rows: MAX_ROWS,
            max_depth: MAX_DEPTH,
            max_query_nodes: MAX_QUERY_NODES,
            call_timeout: DEFAULT_TIMEOUT,
            rate: DEFAULT_RATE,
        }
    }
}

impl Limits {
    /// The defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuses a message larger than the input limit.
    ///
    /// # Errors
    ///
    /// [`McpError::limit_exceeded`].
    pub fn check_input(&self, bytes: usize) -> Result<(), McpError> {
        if bytes > self.max_input_bytes {
            return Err(McpError::limit_exceeded(format!(
                "the message is {bytes} bytes; the limit is {}",
                self.max_input_bytes
            )));
        }
        Ok(())
    }

    /// Clamps a requested row limit to the row cap.
    #[must_use]
    pub fn clamp_rows(&self, requested: Option<u32>) -> u32 {
        requested.unwrap_or(20).clamp(1, self.max_rows)
    }

    /// The per-call timeout in whole milliseconds.
    #[must_use]
    pub fn timeout_ms(&self) -> u64 {
        u64::try_from(self.call_timeout.as_millis()).unwrap_or(u64::MAX)
    }
}
