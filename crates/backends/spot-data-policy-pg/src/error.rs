//! Errors: the two `data_policy_pg.*` codes this crate adds, and the mapping of bridge refusals
//! and database failures onto the fixed-reason [`DataPolicyError::StoreFailed`] the trait carries.
//!
//! No failure carries SQL text, a connection string, a driver message or a row value: the trait's
//! reason is a `&'static str`, and the construction errors name a fixed sentence only.

use std::fmt;

use spot_data_policy::DataPolicyError;
use spot_pg_bridge::BridgeError;

/// Stable code: the ledger tables could not be created.
pub const PREPARE_FAILED: &str = "data_policy_pg.prepare_failed";
/// Stable code: the store cannot be built in this context.
pub const UNSUPPORTED_CONTEXT: &str = "data_policy_pg.unsupported_context";

/// Why the store could not be built or prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PgPolicyError {
    /// `data_policy_pg.prepare_failed`: the DDL did not apply or no connection was available.
    PrepareFailed,
    /// `data_policy_pg.unsupported_context`: the thread is inside an async runtime, or the bridge
    /// is configured for more calls in flight than the pool can serve.
    UnsupportedContext {
        /// A fixed sentence saying what is wrong.
        reason: &'static str,
    },
}

impl PgPolicyError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PrepareFailed => PREPARE_FAILED,
            Self::UnsupportedContext { .. } => UNSUPPORTED_CONTEXT,
        }
    }
}

impl fmt::Display for PgPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrepareFailed => write!(
                f,
                "{PREPARE_FAILED}: the retention ledger tables could not be created"
            ),
            Self::UnsupportedContext { reason } => {
                write!(f, "{UNSUPPORTED_CONTEXT}: {reason}")
            }
        }
    }
}

impl std::error::Error for PgPolicyError {}

/// What went wrong inside one bridged operation. Never shown: it is mapped to a fixed reason.
#[derive(Debug)]
pub(crate) enum OpError {
    /// The driver failed (its message is dropped: it can quote a key or a value).
    Db,
    /// A stored row could not be decoded.
    Decode,
    /// The item already has a different outcome, or a proof differs from the one on file.
    Conflict(&'static str),
    /// The record source refused the effect.
    Effect(DataPolicyError),
    /// A value does not fit the column.
    OutOfRange,
}

impl From<tokio_postgres::Error> for OpError {
    fn from(_error: tokio_postgres::Error) -> Self {
        Self::Db
    }
}

/// A store failure with a fixed reason.
pub(crate) fn store_failed(reason: &'static str) -> DataPolicyError {
    DataPolicyError::StoreFailed { reason }
}

/// Maps a bridged operation's failure to the trait's error. The reason names the bridge code or
/// the kind of failure, never a statement or a value.
pub(crate) fn map(error: BridgeError<OpError>) -> DataPolicyError {
    match error {
        BridgeError::WrongThread => store_failed(
            "pg_bridge.wrong_thread: the retention ledger was called inside an async runtime",
        ),
        BridgeError::Busy => store_failed("pg_bridge.busy: the retention ledger queue is full"),
        BridgeError::Timeout => store_failed(
            "pg_bridge.timeout: the retention ledger call missed its deadline; treat it as not done",
        ),
        BridgeError::Closed => {
            store_failed("pg_bridge.closed: the retention ledger bridge is shut down")
        }
        BridgeError::Connect => {
            store_failed("pg_bridge.connect: no database connection for the retention ledger")
        }
        BridgeError::Config { .. } => {
            store_failed("pg_bridge.config: the retention ledger bridge is misconfigured")
        }
        BridgeError::Store(op) => match op {
            OpError::Db => store_failed("the retention ledger query failed"),
            OpError::Decode => store_failed("a retention ledger row could not be decoded"),
            OpError::Conflict(reason) => store_failed(reason),
            OpError::OutOfRange => store_failed("a retention ledger value is out of range"),
            OpError::Effect(DataPolicyError::StoreFailed { reason }) => store_failed(reason),
            OpError::Effect(_) => store_failed("the record source refused the effect"),
        },
        // The bridge error is non-exhaustive: an unknown refusal is still a visible failure.
        _ => store_failed("the retention ledger bridge refused the call"),
    }
}

/// Maps the failure of one **read** to the trait's error, naming the operation (`outcome`,
/// `checkpoint`, ...) and the bridge code or kind of failure. `$op` is a string literal, because
/// the trait's reason is a `&'static str`; the text never carries a statement, a value or a
/// record's content.
macro_rules! read_error {
    ($op:literal, $error:expr) => {{
        use spot_pg_bridge::BridgeError as Bridge;
        match $error {
            Bridge::WrongThread => $crate::error::store_failed(concat!(
                "pg_bridge.wrong_thread: the ",
                $op,
                " read was called inside an async runtime"
            )),
            Bridge::Busy => $crate::error::store_failed(concat!(
                "pg_bridge.busy: the retention ledger queue is full, the ",
                $op,
                " read did not run"
            )),
            Bridge::Timeout => $crate::error::store_failed(concat!(
                "pg_bridge.timeout: the ",
                $op,
                " read missed its deadline"
            )),
            Bridge::Closed => $crate::error::store_failed(concat!(
                "pg_bridge.closed: the retention ledger bridge is shut down, the ",
                $op,
                " read did not run"
            )),
            Bridge::Connect => $crate::error::store_failed(concat!(
                "pg_bridge.connect: no database connection for the ",
                $op,
                " read"
            )),
            Bridge::Config { .. } => $crate::error::store_failed(concat!(
                "pg_bridge.config: the retention ledger bridge is misconfigured, the ",
                $op,
                " read did not run"
            )),
            Bridge::Store($crate::error::OpError::Decode) => $crate::error::store_failed(concat!(
                "a row of the ",
                $op,
                " read could not be decoded"
            )),
            Bridge::Store(_) => {
                $crate::error::store_failed(concat!("the ", $op, " read failed in the database"))
            }
            // The bridge error is non-exhaustive: an unknown refusal is still a visible failure.
            _ => $crate::error::store_failed(concat!("the ", $op, " read failed")),
        }
    }};
}
pub(crate) use read_error;
