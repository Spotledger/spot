//! Restoring an intake after a restart (WP-RTB-02.T12).
//!
//! A fresh [`NodeIntake`](crate::NodeIntake) starts with empty prefix trackers, an empty serve
//! tracker, no skew holds and no repair queue. After a restart that would let the host
//! acknowledge a prefix it has not applied: parked bundles in the store would no longer hold the
//! prefix, and a bundle that went to the repair list would be forgotten. [`RestoreState`] is what
//! the durable stores hand back, and [`NodeIntake::restore`](crate::NodeIntake::restore) seeds one
//! tenant with it, before the first delivery.
//!
//! The function is pure: it reads nothing, and the caller (the host's intake factory) decides
//! where the state comes from.
//!
//! # What a restored tenant means
//!
//! * The **journal prefix** starts at `applied_prefix` (the ledger's value, already below every
//!   parked and repair-listed bundle) and every parked bundle holds it again at its first
//!   position.
//! * The **serve prefix** starts at `served_prefix`, the last value the host stored after the
//!   bundles below it were applied. It is never above what the node acknowledged before the
//!   restart, so the hub re-serves what is still above it and the stamps rebuild the rest.
//! * Each **repair item** holds the journal prefix at its first position, as it did before the
//!   restart, until a bundle with that first position applies. The items themselves are not put
//!   back on the in-memory repair queue: they are in the durable repair list already, and queueing
//!   them again would store them twice.
//! * The **holds** are the skew holds saved after the last receive.

use spot_sync_closure::Position;
use spot_sync_skew::HoldSet;

use crate::model::RepairItem;

/// Stable code: a restore was refused.
pub const RESTORE_REFUSED: &str = "sync_node_intake.restore_refused";

/// What one tenant needs to continue after a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreState {
    /// The journal prefix recomputed from the ledger, the parked bundles and the repair list.
    pub applied_prefix: Option<Position>,
    /// The serve-sequence prefix the host last stored (what the hub has been acknowledged by).
    pub served_prefix: Option<u64>,
    /// The skew holds saved after the last receive; `None` when none were saved.
    pub holds: Option<HoldSet>,
    /// The first position of every parked bundle, one entry per bundle (duplicates count).
    pub parked_first_positions: Vec<Position>,
    /// The repair list, for the tenant being restored.
    pub repair: Vec<RepairItem>,
}

/// Why a restore was refused. The intake is left as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreError {
    /// The tenant already has state in this intake (it received something, or was restored).
    AlreadyStarted,
    /// The node already keeps its maximum number of tenants.
    TooManyTenants,
    /// A repair item names another tenant, or the saved holds are not usable.
    Invalid,
}

impl RestoreError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(self) -> &'static str {
        RESTORE_REFUSED
    }
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::AlreadyStarted => "The tenant has state already; a restore must come first.",
            Self::TooManyTenants => "The node already serves its maximum number of tenants.",
            Self::Invalid => "The saved state does not belong to this tenant.",
        };
        write!(f, "{}: {text}", self.code())
    }
}

impl std::error::Error for RestoreError {}
