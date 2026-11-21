//! The errors the service returns: the five the HTTP layer maps (`WP-STO-14.T2`) plus the
//! storage failures it cannot explain further.

use std::fmt;

use spot_storage_contract::{ErrorKind, RecordId, StorageError};

use crate::owner::Owner;

/// Why an occupancy operation did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OccupancyError {
    /// Another hold or booking occupies part of the range. The storage error is an
    /// `ExclusionViolation`; it never names the other record (storage cannot know permission).
    Conflict(StorageError),
    /// The hold is past `expires_at`, or its row is gone (swept or released). The storage error
    /// has kind `StaleVersion`: the confirm never extends a hold, the caller must claim again
    /// (ADR-087 section 6.3 cases 4 and 5, SEM-HLD-02).
    Expired(StorageError),
    /// The caller is not the holder of the record.
    NotHolder,
    /// The request is malformed: an empty, unbounded, wrong-kind or too long range, a bad holder,
    /// or a time-to-live out of bounds. Nothing was written.
    InvalidRange(String),
    /// This node does not own the unit: only the owning node holds or confirms (ADR-087 section
    /// 6.5). The request goes through the owner. Nothing was written.
    NotOwner {
        /// The unit.
        unit: RecordId,
        /// The node that owns it.
        owner: Owner,
    },
    /// Any other storage failure, after the retries a retryable one is given.
    Storage(StorageError),
}

impl OccupancyError {
    /// A stable code for audit and logs.
    pub fn code(&self) -> &'static str {
        match self {
            OccupancyError::Conflict(_) => "occupancy.conflict",
            OccupancyError::Expired(_) => "occupancy.hold_expired",
            OccupancyError::NotHolder => "occupancy.not_holder",
            OccupancyError::InvalidRange(_) => "occupancy.invalid_range",
            OccupancyError::NotOwner { .. } => "partition.not_owner",
            OccupancyError::Storage(e) => e.kind.code(),
        }
    }

    /// The storage kind underneath, when there is one: `ExclusionViolation` for a conflict,
    /// `StaleVersion` for an expired confirm.
    pub fn storage_kind(&self) -> Option<ErrorKind> {
        match self {
            OccupancyError::Conflict(e)
            | OccupancyError::Expired(e)
            | OccupancyError::Storage(e) => Some(e.kind),
            _ => None,
        }
    }
}

impl From<StorageError> for OccupancyError {
    /// An exclusion violation is a conflict; everything else stays a storage failure.
    fn from(e: StorageError) -> Self {
        if e.kind == ErrorKind::ExclusionViolation {
            OccupancyError::Conflict(e)
        } else {
            OccupancyError::Storage(e)
        }
    }
}

impl fmt::Display for OccupancyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OccupancyError::Conflict(_) => {
                f.write_str("these dates conflict with an existing record")
            }
            OccupancyError::Expired(e) => {
                write!(f, "the hold is no longer valid: {}", e.explanation)
            }
            OccupancyError::NotHolder => f.write_str("the caller does not hold this record"),
            OccupancyError::InvalidRange(why) => write!(f, "invalid request: {why}"),
            OccupancyError::NotOwner { unit, owner } => {
                write!(
                    f,
                    "this node does not own unit {unit}; the owner is {owner}"
                )
            }
            OccupancyError::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OccupancyError {}
