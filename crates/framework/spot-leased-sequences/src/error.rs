//! The `leased_sequences.*` error vocabulary (WP-EDG-03.T3).
//!
//! Every variant carries one stable code (see [`LeasedSequencesError::code`]) and maps onto an
//! existing [`ErrorKind`]: the crate adds no new storage kind. The codes' summaries live in
//! `crates/framework/spot-errors/catalogue/leased_sequences.toml`.
//!
//! A message names the series key and the counts involved. It never names the node a range is
//! leased to and never carries any part of the node's identity: the node secret (ADR-056) must
//! not reach a log, a client or a synced envelope.

use std::error::Error;
use std::fmt;

use spot_storage_contract::{ErrorKind, StorageError};

/// A failure of the leased (offline) allocator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeasedSequencesError {
    /// The series has no lease, or its lease holds fewer numbers than the caller asked for.
    ///
    /// Nothing is handed out when this is returned: a block is all or nothing, so the caller
    /// never has to reason about half a block.
    LeaseExhausted {
        /// The series that could not fill the request, as text.
        series: String,
        /// How many numbers the caller asked for.
        requested: u32,
        /// How many numbers the lease still holds.
        remaining: u64,
    },
    /// A branch tried to set the next number of a series.
    ///
    /// Leases are issued by the hub ([`spot_partition::RangeAllocator`]); a branch that could
    /// move its own cursor could reuse a number another branch will be given after sync.
    SetNextRefused {
        /// The series whose next number was refused, as text.
        series: String,
        /// The number the caller tried to set.
        next: u64,
    },
    /// A lease offered for a series that already holds one was refused, because installing it
    /// could reissue a number. The held lease is untouched.
    LeaseRefused {
        /// The series the lease was offered for, as text.
        series: String,
        /// Why the lease was refused. Never names a node.
        reason: LeaseRefusal,
    },
    /// A count of zero is not an allocation request.
    InvalidCount {
        /// The rejected count.
        count: u32,
    },
    /// The branch series high-water store failed, so no number was handed out.
    Store {
        /// Why the store failed, from the partition layer.
        reason: String,
    },
}

/// Why a renewal lease was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseRefusal {
    /// The lease is for another node than the held one.
    OtherNode,
    /// The range is not strictly above the held one: it overlaps, is lower, or repeats it.
    NotAbove,
}

impl LeaseRefusal {
    /// The reason as text, for messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OtherNode => "it is for another node",
            Self::NotAbove => "its range is not strictly above the held range",
        }
    }
}

impl LeasedSequencesError {
    /// The stable dotted code (`leased_sequences.*`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::LeaseExhausted { .. } => "leased_sequences.lease_exhausted",
            Self::SetNextRefused { .. } => "leased_sequences.set_next_refused",
            Self::LeaseRefused { .. } => "leased_sequences.lease_refused",
            Self::InvalidCount { .. } => "leased_sequences.invalid_count",
            Self::Store { .. } => "leased_sequences.store",
        }
    }

    /// The storage error kind this failure maps onto.
    ///
    /// An exhausted lease (and a failed high-water store) is [`ErrorKind::Unavailable`]: the
    /// caller can retry the same work once the sync layer has leased another range. A refused
    /// `set_next` is [`ErrorKind::Unsupported`], and a zero count is
    /// [`ErrorKind::InvalidValue`].
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::LeaseExhausted { .. } | Self::Store { .. } => ErrorKind::Unavailable,
            Self::SetNextRefused { .. } => ErrorKind::Unsupported,
            Self::InvalidCount { .. } | Self::LeaseRefused { .. } => ErrorKind::InvalidValue,
        }
    }
}

impl fmt::Display for LeasedSequencesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LeaseExhausted {
                series,
                requested,
                remaining,
            } => write!(
                f,
                "the branch series `{series}` has {remaining} of the {requested} numbers requested left in its leased range; lease another range from the hub"
            ),
            Self::SetNextRefused { series, next } => write!(
                f,
                "a branch cannot set the series `{series}` to continue at {next}: the hub alone issues numbers"
            ),
            Self::LeaseRefused { series, reason } => write!(
                f,
                "the lease offered for the branch series `{series}` was refused: {}",
                reason.as_str()
            ),
            Self::InvalidCount { count } => {
                write!(
                    f,
                    "a block of {count} numbers is not a valid allocation request"
                )
            }
            Self::Store { reason } => write!(
                f,
                "the high-water mark of the branch series could not be written, so no number was handed out: {reason}"
            ),
        }
    }
}

impl Error for LeasedSequencesError {}

impl From<LeasedSequencesError> for StorageError {
    fn from(error: LeasedSequencesError) -> Self {
        Self::new(error.kind(), error.to_string())
    }
}
