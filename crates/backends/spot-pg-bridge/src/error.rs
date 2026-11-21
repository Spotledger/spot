//! The bridge's refusals and their stable catalogue codes.
//!
//! A refusal never carries SQL text, parameters or row data: its message is fixed text and the
//! only variable part is the name of a configuration setting. A database error is the store's
//! own error type, carried through untouched as [`BridgeError::Store`].

use std::convert::Infallible;
use std::fmt;

/// Stable code: the calling thread is inside an async runtime.
pub const WRONG_THREAD: &str = "pg_bridge.wrong_thread";
/// Stable code: the queue is full; nothing was queued.
pub const BUSY: &str = "pg_bridge.busy";
/// Stable code: the deadline passed; the statement was cancelled.
pub const TIMEOUT: &str = "pg_bridge.timeout";
/// Stable code: the bridge is shut down, or a call on it ended without an answer.
pub const CLOSED: &str = "pg_bridge.closed";
/// Stable code: no connection could be taken from the pool.
pub const CONNECT: &str = "pg_bridge.connect";
/// Stable code: the configuration is not usable.
pub const CONFIG: &str = "pg_bridge.config";

/// Why a bridge call did not produce the store's answer, or the store's own error.
///
/// `E` is the error type of the closure's future; the constructor, which has no such type,
/// returns `BridgeError<Infallible>` (the default), whose `Store` variant cannot occur.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum BridgeError<E = Infallible> {
    /// `pg_bridge.wrong_thread`: the calling thread is inside a tokio runtime. Nothing was run.
    WrongThread,
    /// `pg_bridge.busy`: the queue was full. Nothing was queued; retry later or refuse.
    Busy,
    /// `pg_bridge.timeout`: the deadline passed. The statement was cancelled and its connection
    /// discarded; treat the work as not done.
    Timeout,
    /// `pg_bridge.closed`: the bridge was shut down, or the call ended without an answer (the
    /// closure panicked).
    Closed,
    /// `pg_bridge.connect`: no connection could be taken from the pool.
    Connect,
    /// `pg_bridge.config`: the configuration names a setting that cannot hold.
    Config {
        /// The setting, such as `max_in_flight`.
        setting: &'static str,
        /// A fixed sentence saying what is wrong with it.
        reason: &'static str,
    },
    /// The closure's future failed; this is the store's own error, unchanged.
    Store(E),
}

impl<E> BridgeError<E> {
    /// The catalogue code, or `None` for the store's own error, which has its own code.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::WrongThread => Some(WRONG_THREAD),
            Self::Busy => Some(BUSY),
            Self::Timeout => Some(TIMEOUT),
            Self::Closed => Some(CLOSED),
            Self::Connect => Some(CONNECT),
            Self::Config { .. } => Some(CONFIG),
            Self::Store(_) => None,
        }
    }

    /// Whether retrying later can help: `busy`, `timeout` and `connect` only. A timed-out write
    /// may or may not have been applied; the caller decides whether a retry is safe.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Busy | Self::Timeout | Self::Connect)
    }
}

impl<E: fmt::Display> fmt::Display for BridgeError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongThread => {
                f.write_str("pg_bridge.wrong_thread: called from a thread inside an async runtime")
            }
            Self::Busy => f.write_str("pg_bridge.busy: the bridge queue is full"),
            Self::Timeout => f.write_str("pg_bridge.timeout: the call missed its deadline"),
            Self::Closed => f.write_str("pg_bridge.closed: the bridge is shut down"),
            Self::Connect => f.write_str("pg_bridge.connect: no database connection available"),
            Self::Config { setting, reason } => {
                write!(f, "pg_bridge.config: {setting}: {reason}")
            }
            Self::Store(e) => e.fmt(f),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for BridgeError<E> {}
