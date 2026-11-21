//! Bridge configuration and its validation.

use std::convert::Infallible;
use std::time::Duration;

use crate::error::BridgeError;

/// Pool connections the bridge keeps for itself, so the pool never has every connection tied up
/// by in-flight calls: one for housekeeping (health probes, a caller's second checkout).
pub const RESERVED_CONNECTIONS: usize = 1;

/// Sizing and deadlines of one bridge. See the crate README for how to choose the numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeConfig {
    /// A short label for this bridge's metrics (`bridge="<name>"`): letters, digits, `_`, `-`.
    pub name: String,
    /// Threads of the bridge's own runtime. At least 2.
    pub workers: usize,
    /// Calls running at once. At most `pool size - RESERVED_CONNECTIONS - 1`.
    pub max_in_flight: usize,
    /// Calls allowed to wait for a slot. A call beyond `max_in_flight + queue_capacity` is
    /// refused at once with `busy`.
    pub queue_capacity: usize,
    /// The deadline used by [`crate::Bridge::run_default`].
    pub default_deadline: Duration,
    /// The ceiling on any deadline; a larger one is clamped to it, not refused.
    pub max_deadline: Duration,
}

impl Default for BridgeConfig {
    /// Conservative defaults: 2 workers, 4 in flight, 64 queued, a 5 s default and a 30 s ceiling.
    /// Four in flight needs a pool of at least 6.
    fn default() -> Self {
        Self {
            name: "default".to_owned(),
            workers: 2,
            max_in_flight: 4,
            queue_capacity: 64,
            default_deadline: Duration::from_secs(5),
            max_deadline: Duration::from_secs(30),
        }
    }
}

fn bad(setting: &'static str, reason: &'static str) -> BridgeError {
    BridgeError::Config { setting, reason }
}

impl BridgeConfig {
    /// Checks every rule against a pool of `pool_size` connections.
    ///
    /// # Errors
    ///
    /// [`BridgeError::Config`] naming the first setting that cannot hold.
    pub fn validate(&self, pool_size: usize) -> Result<(), BridgeError<Infallible>> {
        let name_ok = !self.name.is_empty()
            && self.name.len() <= 40
            && self
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !name_ok {
            return Err(bad(
                "name",
                "must be 1 to 40 of letters, digits, '_' or '-'",
            ));
        }
        if self.workers < 2 {
            return Err(bad("workers", "must be at least 2"));
        }
        if self.max_in_flight < 1 {
            return Err(bad("max_in_flight", "must be at least 1"));
        }
        if self.queue_capacity < 1 {
            return Err(bad("queue_capacity", "must be at least 1"));
        }
        if self.default_deadline.is_zero() {
            return Err(bad("default_deadline", "must be greater than zero"));
        }
        if self.max_deadline < self.default_deadline {
            return Err(bad("max_deadline", "must not be below default_deadline"));
        }
        if pool_size < RESERVED_CONNECTIONS + 2 {
            return Err(bad(
                "pool",
                "too small: needs at least the bridge's reserve plus two connections",
            ));
        }
        if self.max_in_flight > pool_size - RESERVED_CONNECTIONS - 1 {
            return Err(bad(
                "max_in_flight",
                "must be at least one lower than the pool size minus the bridge's reserve",
            ));
        }
        Ok(())
    }

    /// The largest `max_in_flight` a pool of `pool_size` connections allows, or 0 if none.
    #[must_use]
    pub fn largest_in_flight(pool_size: usize) -> usize {
        pool_size.saturating_sub(RESERVED_CONNECTIONS + 1)
    }
}
