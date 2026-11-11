//! The one error type of this crate, with stable `sync_acks.*` codes.
//!
//! Messages carry node ids, positions and field names only, never record contents.

use spot_conditions::ConditionsError;
use spot_sync_obligations::ObligationError;

/// A refusal of an ack, a registration or a query.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AckError {
    /// A node reported an applied prefix lower than the one already recorded. Nothing changed.
    #[error(
        "node {node} of tenant {tenant} reported applied prefix {after}, below the recorded {before}"
    )]
    PrefixRegressed {
        /// The node.
        node: String,
        /// The tenant whose stream the report is about.
        tenant: String,
        /// The recorded prefix.
        before: u64,
        /// The lower prefix just reported.
        after: u64,
    },

    /// A node claimed an applied prefix above the newest position the hub's own stream holds.
    /// Nothing changed. Only the three numbers and the node id are carried.
    #[error(
        "node {node} of tenant {tenant} claimed applied prefix {claimed}, above the hub's stream head {head}"
    )]
    PrefixBeyondHead {
        /// The node.
        node: String,
        /// The tenant whose stream the report is about.
        tenant: String,
        /// The prefix the node claimed.
        claimed: u64,
        /// The hub's stream head.
        head: u64,
    },

    /// A node reported a hold above the newest position the hub's own stream holds. Nothing
    /// changed.
    #[error(
        "node {node} of tenant {tenant} reported a hold at {claimed}, above the hub's stream head {head}"
    )]
    HoldBeyondHead {
        /// The node.
        node: String,
        /// The tenant whose stream the report is about.
        tenant: String,
        /// The highest offending held position.
        claimed: u64,
        /// The hub's stream head.
        head: u64,
    },

    /// The node was never registered, so nothing it reports is accepted.
    #[error("node {node} is not registered for tenant {tenant}")]
    UnknownNode {
        /// The node id as given.
        node: String,
        /// The tenant id as given. Empty for `set_version` only, which is node-level
        /// and names no tenant.
        tenant: String,
    },

    /// No mandatory event is registered at this stream position.
    #[error("no mandatory event is registered for tenant {tenant} at position {position}")]
    UnknownEvent {
        /// The tenant asked about.
        tenant: String,
        /// The position asked about.
        position: u64,
    },

    /// A hold names a position that is not a registered mandatory event.
    #[error(
        "node {node} of tenant {tenant} holds position {position}, which is not a registered mandatory event"
    )]
    HoldUnknown {
        /// The node.
        node: String,
        /// The tenant whose stream the report is about.
        tenant: String,
        /// The unknown position.
        position: u64,
    },

    /// A node or tenant id contains `@`, which separates the two in a condition subject
    /// (`node:<node>@tenant:<tenant>`), so it would let one pair's subject read as another's.
    #[error("id {id} contains '@', which is reserved")]
    IdInvalid {
        /// The offending id as given.
        id: String,
    },

    /// The condition definitions given to the ledger lack one it raises.
    #[error("the condition definitions lack {id}")]
    ConditionMissing {
        /// The missing condition id.
        id: &'static str,
    },

    /// The obligation model refused (settings out of range). Its code is catalogued with
    /// `sync_obligations.*`.
    #[error(transparent)]
    Obligations(#[from] ObligationError),

    /// The conditions framework refused (a duplicate or malformed definition). Its code is
    /// catalogued with `conditions.*`.
    #[error(transparent)]
    Conditions(#[from] ConditionsError),
}

impl AckError {
    /// The stable machine-readable code, for logs, API responses and metrics.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PrefixRegressed { .. } => PREFIX_REGRESSED,
            Self::PrefixBeyondHead { .. } => PREFIX_BEYOND_HEAD,
            Self::HoldBeyondHead { .. } => HOLD_BEYOND_HEAD,
            Self::UnknownNode { .. } => UNKNOWN_NODE,
            Self::UnknownEvent { .. } => UNKNOWN_EVENT,
            Self::HoldUnknown { .. } => HOLD_UNKNOWN,
            Self::IdInvalid { .. } => ID_INVALID,
            Self::ConditionMissing { .. } => CONDITION_MISSING,
            Self::Obligations(e) => e.code(),
            Self::Conditions(e) => e.code(),
        }
    }
}

/// The applied prefix went backwards.
pub const PREFIX_REGRESSED: &str = "sync_acks.prefix_regressed";
/// The claimed applied prefix is above the hub's stream head.
pub const PREFIX_BEYOND_HEAD: &str = "sync_acks.prefix_beyond_head";
/// A reported hold is above the hub's stream head.
pub const HOLD_BEYOND_HEAD: &str = "sync_acks.hold_beyond_head";
/// The node is not registered.
pub const UNKNOWN_NODE: &str = "sync_acks.unknown_node";
/// No mandatory event at that position.
pub const UNKNOWN_EVENT: &str = "sync_acks.unknown_event";
/// A hold names an unregistered event.
pub const HOLD_UNKNOWN: &str = "sync_acks.hold_unknown";
/// A condition definition the ledger raises is absent.
pub const CONDITION_MISSING: &str = "sync_acks.condition_missing";
/// A node or tenant id contains the reserved `@`.
pub const ID_INVALID: &str = "sync_acks.id_invalid";
