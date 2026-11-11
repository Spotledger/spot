//! Typed, matchable errors for the Redis plugin: the `cluster_redis.*` family.
//!
//! Only the parts of the plugin that surface a failure of their own use these codes: the health
//! probe and the connection wrapper. The coordinator maps a Redis failure to the
//! `coordinator.unavailable` code its trait already defines, the cache degrades to a miss (it is
//! lossy by contract) and the bus maps one to `metalive.store`, so callers keep matching the
//! codes of the interface they called, never a backend-specific one.

use std::fmt;

/// The Redis/Valkey server could not be reached or answered a command with an error.
pub const UNAVAILABLE: &str = "cluster_redis.unavailable";

/// The server answered something the plugin could not parse (a malformed reply or message).
pub const PROTOCOL: &str = "cluster_redis.protocol";

/// What went wrong in one plugin operation.
///
/// `code` is stable and callers match on it, never on the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisError {
    /// The stable machine-readable code (`cluster_redis.unavailable`, ...).
    pub code: &'static str,
    /// What went wrong, with the key or channel involved.
    pub message: String,
}

impl RedisError {
    /// An error with the given code and message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The server could not be reached or refused a command.
    #[must_use]
    pub fn unavailable(what: &str, detail: impl fmt::Display) -> Self {
        Self::new(UNAVAILABLE, format!("{what}: {detail}"))
    }

    /// A reply could not be understood.
    #[must_use]
    pub fn protocol(what: &str, detail: impl fmt::Display) -> Self {
        Self::new(PROTOCOL, format!("{what}: {detail}"))
    }
}

impl fmt::Display for RedisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RedisError {}

/// Result of a plugin operation that carries a [`RedisError`].
pub type RedisResult<T> = Result<T, RedisError>;
