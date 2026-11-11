//! The `lease_grants.*` error vocabulary.
//!
//! Every code has one entry in `crates/framework/spot-errors/catalogue/lease_grants.toml`.
//! Callers match on [`GrantError::code`], never on the message. A message never says whether a
//! node exists and never carries a payload, a key or a raw statement.

use spot_sync_protocol::NodeId;
use thiserror::Error;

/// Why a lease was not granted.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum GrantError {
    /// The request names another node than the one the transport authenticated.
    #[error(
        "the request names node {claimed}, but the connection authenticated as {authenticated}"
    )]
    NodeMismatch {
        /// The node the transport authenticated.
        authenticated: NodeId,
        /// The node the payload claimed.
        claimed: NodeId,
    },
    /// The node is not entitled to a range: suspended, decommissioned, unknown, or standing
    /// could not be read. The message does not say which.
    #[error("the node may not be granted a number range")]
    NodeNotActive,
    /// The node is not entitled to the series it asked for. The hub decides which series a node
    /// may lease; a node does not choose its own.
    #[error("the node is not entitled to this naming series")]
    SeriesNotEntitled,
    /// The service already tracks as many distinct series as its policy allows.
    #[error("the hub cannot track another naming series (at most {max} at once)")]
    SeriesCapReached {
        /// The most distinct series the service tracks.
        max: usize,
    },
    /// The requested size is zero or above the series maximum.
    #[error("the requested range size is not allowed for this series (1 to {max} numbers)")]
    SizeRefused {
        /// The most numbers one range of the series may hold.
        max: u64,
    },
    /// The instants involved cannot be compared: adding the minimum interval to the last grant's
    /// instant overflows the representable date range, so whether the node may ask again cannot
    /// be decided. Refused rather than guessed, and no range is taken.
    #[error("the grant times cannot be compared, so no range was issued")]
    TimeInvalid,
    /// The node was granted a range of this series too recently.
    #[error("a range of this series was granted to the node too recently; ask again later")]
    TooSoon,
    /// The grant could not be recorded. The range is lost, never reissued.
    #[error("the grant could not be recorded, so no range was issued")]
    LogFailed,
    /// The audit record could not be written. The range is lost, never reissued.
    #[error("the grant could not be audited, so no range was issued")]
    AuditFailed,
    /// The series has no numbers left for a range of this size.
    #[error("the series has no numbers left")]
    SeriesExhausted,
    /// The allocator refused for another reason.
    #[error("the allocator could not grant a range")]
    AllocatorFailed,
}

impl GrantError {
    /// The stable dotted code (`lease_grants.*`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NodeMismatch { .. } => "lease_grants.node_mismatch",
            Self::NodeNotActive => "lease_grants.node_not_active",
            Self::SeriesNotEntitled => "lease_grants.series_not_entitled",
            Self::SeriesCapReached { .. } => "lease_grants.series_cap_reached",
            Self::SizeRefused { .. } => "lease_grants.size_refused",
            Self::TimeInvalid => "lease_grants.time_invalid",
            Self::TooSoon => "lease_grants.too_soon",
            Self::LogFailed => "lease_grants.log_failed",
            Self::AuditFailed => "lease_grants.audit_failed",
            Self::SeriesExhausted => "lease_grants.series_exhausted",
            Self::AllocatorFailed => "lease_grants.allocator_failed",
        }
    }
}
