//! The one error type of the shared-balance crate, with stable [`codes`](crate::codes).

use crate::codes;
use crate::ledger::{BalanceId, EntryId, NodeId};

/// What a shared-balance operation failed with.
///
/// It never carries a secret. Money is named by balance and entry ids only.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum BalanceError {
    /// An amount was zero or too large for an integer minor unit.
    #[error("the amount is not a valid number of minor units")]
    InvalidAmount,
    /// Adding entries overflowed an integer minor unit.
    #[error("the balance overflowed an integer minor unit")]
    Overflow,
    /// An entry id was reused for different content.
    #[error("entry {id} was reused for different content")]
    EntryConflict {
        /// The reused id.
        id: EntryId,
    },
    /// The balance does not exist.
    #[error("balance {balance} does not exist")]
    UnknownBalance {
        /// The balance named.
        balance: BalanceId,
    },
    /// The balance already exists.
    #[error("balance {balance} already exists")]
    BalanceExists {
        /// The balance named.
        balance: BalanceId,
    },
    /// A quota grant asked for more than the balance has available.
    #[error("balance {balance} does not have that much available to grant")]
    InsufficientAvailable {
        /// The balance named.
        balance: BalanceId,
    },
    /// The node already holds an open grant on the balance.
    #[error("node {node} already holds an open grant on {balance}")]
    GrantExists {
        /// The node.
        node: NodeId,
        /// The balance.
        balance: BalanceId,
    },
    /// A policy configuration is not valid.
    #[error("the policy configuration is not valid: {why}")]
    Config {
        /// What is wrong.
        why: &'static str,
    },
}

impl BalanceError {
    /// The stable code from [`codes`](crate::codes) that names this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            BalanceError::InvalidAmount => codes::INVALID_AMOUNT,
            BalanceError::Overflow => codes::OVERFLOW,
            BalanceError::EntryConflict { .. } => codes::ENTRY_CONFLICT,
            BalanceError::UnknownBalance { .. } => codes::UNKNOWN_BALANCE,
            BalanceError::BalanceExists { .. } => codes::BALANCE_EXISTS,
            BalanceError::InsufficientAvailable { .. } => codes::INSUFFICIENT_AVAILABLE,
            BalanceError::GrantExists { .. } => codes::GRANT_EXISTS,
            BalanceError::Config { .. } => codes::CONFIG_INVALID,
        }
    }
}
