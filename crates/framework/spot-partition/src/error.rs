//! The `partition.*` error vocabulary (WP-EDG-03.T1).
//!
//! Every code here has one entry in `crates/framework/spot-errors/catalogue/partition.toml`.
//! Callers match on [`PartitionError::code`] or [`NotOwner::code`], never on the message.
//! Messages name nodes, DocTypes and scope keys; they never carry a secret or a raw statement.

use std::fmt;

use spot_sync_protocol::NodeId;

use crate::scope::ScopeKey;

/// A write or a replica was refused because the acting node is not the record's owner.
///
/// Returned by [`crate::OwnerCheck::allow_write`] and [`crate::OwnerCheck::allow_replica`].
/// The `owner` names the node the partition map holds, so a caller can say plainly "owned by
/// head office" (ADR-063 section 2) instead of "forbidden".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotOwner {
    node: NodeId,
    doctype: String,
    scope: Option<Box<ScopeKey>>,
    owner: Option<NodeId>,
}

impl NotOwner {
    /// A refusal naming the acting node, the DocType, the scope key (when the check used one)
    /// and the owner the map holds (when one is assigned).
    #[must_use]
    pub fn new(
        node: NodeId,
        doctype: String,
        scope: Option<ScopeKey>,
        owner: Option<NodeId>,
    ) -> Self {
        Self {
            node,
            doctype,
            scope: scope.map(Box::new),
            owner,
        }
    }

    /// The node whose write or replica was refused.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The DocType the record belongs to.
    #[must_use]
    pub fn doctype(&self) -> &str {
        &self.doctype
    }

    /// The scope key the check used, when it used one.
    #[must_use]
    pub fn scope(&self) -> Option<&ScopeKey> {
        self.scope.as_deref()
    }

    /// The node the partition map names as owner, when one is assigned.
    #[must_use]
    pub fn owner(&self) -> Option<&NodeId> {
        self.owner.as_ref()
    }

    /// The stable dotted code, `partition.not_owner`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        "partition.not_owner"
    }
}

impl fmt::Display for NotOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.scope(), self.owner()) {
            (Some(scope), Some(owner)) => write!(
                f,
                "node {} may not write `{}` for scope {scope}: it is owned by {owner}",
                self.node, self.doctype
            ),
            (Some(scope), None) => write!(
                f,
                "node {} may not write `{}` for scope {scope}: no node owns it",
                self.node, self.doctype
            ),
            (None, Some(owner)) => write!(
                f,
                "node {} may not forward `{}`: the record is owned by {owner}",
                self.node, self.doctype
            ),
            (None, None) => write!(
                f,
                "node {} may not forward `{}`: no node owns it",
                self.node, self.doctype
            ),
        }
    }
}

impl std::error::Error for NotOwner {}

/// The most DocType names a `partition.scope_uncovered` message lists.
pub const MAX_NAMED_DOCTYPES: usize = 20;

/// A failure of a partition operation, with a stable `partition.*` code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PartitionError {
    /// A branch naming series ran out of its leased range while offline.
    RangeExhausted {
        /// The series that ran out.
        series: String,
        /// The node that owns the series.
        node: String,
    },
    /// No range lease is installed for a branch naming series.
    NoLease {
        /// The series with no lease.
        series: String,
        /// The node that would allocate.
        node: String,
    },
    /// A range lease is malformed or its persisted high-water is outside it.
    InvalidRange {
        /// Why the range is refused.
        reason: String,
    },
    /// An offered lease is not above the one already stored for the branch naming series.
    ///
    /// Installing it would rewrite the persisted high-water mark down to the lease's first
    /// number, so a branch would hand out numbers it already issued. The message names no
    /// number of the stored lease: another node's high-water mark is not this caller's business.
    LeaseNotAboveStored {
        /// The series the offered lease is for.
        series: String,
        /// The node the offered lease is for.
        node: String,
    },
    /// A scope key is malformed.
    InvalidScope {
        /// Why the key is refused.
        reason: String,
    },
    /// Two nodes would own the same scope key at the same time.
    OwnershipConflict {
        /// The scope key, as text.
        scope: String,
        /// The node that already owns it in the overlapping interval.
        owner: String,
        /// The node the assignment tried to give it to.
        challenger: String,
    },
    /// An argument to a partition operation is invalid.
    InvalidArgument {
        /// Why the argument is refused.
        reason: String,
    },
    /// Partitioned DocTypes have no scope field entry, so their writes would be unprotected.
    ScopeUncovered {
        /// The DocTypes with no scope field entry, sorted, without repeats.
        doctypes: Vec<String>,
    },
    /// The running-low notice of a branch series could not be written to the outbox.
    ///
    /// Numbering continues: this is reported (counted and logged once), never raised from an
    /// allocation.
    RangeLowNotRecorded {
        /// The series whose notice was lost.
        series: String,
        /// The node that owns the series.
        node: String,
    },
    /// The branch series high-water store failed.
    Store {
        /// Why the store failed.
        reason: String,
    },
}

impl PartitionError {
    /// The stable dotted code (`partition.*`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RangeExhausted { .. } => "partition.range_exhausted",
            Self::NoLease { .. } => "partition.no_lease",
            Self::InvalidRange { .. } => "partition.invalid_range",
            Self::LeaseNotAboveStored { .. } => "partition.lease_not_above_stored",
            Self::InvalidScope { .. } => "partition.invalid_scope",
            Self::OwnershipConflict { .. } => "partition.ownership_conflict",
            Self::InvalidArgument { .. } => "partition.invalid_argument",
            Self::ScopeUncovered { .. } => "partition.scope_uncovered",
            Self::RangeLowNotRecorded { .. } => "partition.range_low_not_recorded",
            Self::Store { .. } => "partition.store",
        }
    }
}

impl fmt::Display for PartitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RangeExhausted { series, node } => write!(
                f,
                "the branch series `{series}` on node {node} has exhausted its leased range"
            ),
            Self::NoLease { series, node } => write!(
                f,
                "node {node} holds no lease for the branch series `{series}`"
            ),
            Self::InvalidRange { reason } => write!(f, "the number range is invalid: {reason}"),
            Self::LeaseNotAboveStored { series, node } => write!(
                f,
                "the lease offered for the branch series `{series}` on node {node} is not above \
                 the one already stored, so installing it would reissue a number"
            ),
            Self::InvalidScope { reason } => write!(f, "the scope key is invalid: {reason}"),
            Self::OwnershipConflict {
                scope,
                owner,
                challenger,
            } => write!(
                f,
                "scope {scope} is owned by {owner} in that interval; {challenger} cannot also own it"
            ),
            Self::InvalidArgument { reason } => write!(f, "invalid argument: {reason}"),
            Self::ScopeUncovered { doctypes } => {
                let shown = doctypes
                    .iter()
                    .take(MAX_NAMED_DOCTYPES)
                    .map(|d| format!("`{d}`"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = doctypes.len().saturating_sub(MAX_NAMED_DOCTYPES);
                write!(
                    f,
                    "partitioned DocTypes without a scope field are unprotected and refuse \
                     writes: {shown}"
                )?;
                if more > 0 {
                    write!(f, " and {more} more")?;
                }
                Ok(())
            }
            Self::RangeLowNotRecorded { series, node } => write!(
                f,
                "the running-low notice of the branch series `{series}` on node {node} could not \
                 be recorded; numbering continues"
            ),
            Self::Store { reason } => write!(f, "the branch series store failed: {reason}"),
        }
    }
}

impl std::error::Error for PartitionError {}
