//! Typed errors with stable [`codes`](crate::codes).

use crate::codes;

/// Anything the routing policy engine can refuse to do.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A policy document failed validation.
    #[error("invalid routing policy: {reason}")]
    InvalidPolicy {
        /// What was wrong, in one sentence, naming no value that could be a secret.
        reason: String,
    },
    /// A workload class or node role name is not one of the known values.
    #[error("unknown workload class or node role: {name}")]
    UnknownClass {
        /// The offending name.
        name: String,
    },
    /// A forward was refused because the hop budget is spent.
    #[error("forward to {target} refused: hop {hops} of a maximum {max_hops}")]
    HopLimit {
        /// The node the forward wanted.
        target: String,
        /// The hop count the request already carries.
        hops: u32,
        /// The policy's maximum.
        max_hops: u32,
    },
    /// The forwarding transport could not reach the target node.
    #[error("forwarding to {target} failed: {reason}")]
    ForwardFailed {
        /// The node the forward wanted.
        target: String,
        /// What the transport reported, with no secret in it.
        reason: String,
    },
}

impl Error {
    /// The stable dotted code for this error, one entry in `catalogue/workload_routing.toml`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Error::InvalidPolicy { .. } => codes::INVALID_POLICY,
            Error::UnknownClass { .. } => codes::UNKNOWN_CLASS,
            Error::HopLimit { .. } => codes::HOP_LIMIT,
            Error::ForwardFailed { .. } => codes::FORWARD_FAILED,
        }
    }
}

/// A routing result.
pub type Result<T> = std::result::Result<T, Error>;
