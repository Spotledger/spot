//! Plain data: deliveries, receipts, parked bundles and settings.

use std::collections::BTreeSet;

use spot_events_contract::SourceId;
use spot_sync_closure::{Bundle, Position, RecordKey};
use spot_sync_skew::HoldReason;

use crate::error::NodeError;

/// One bundle as it reaches the node: who sent it, for which tenant, and the bundle itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The source the transport says the bundle came from. Must equal the configured hub.
    pub source: SourceId,
    /// The tenant tag. A parking queue never mixes tenants.
    pub tenant: String,
    /// The bundle.
    pub bundle: Bundle,
}

/// One record a sink applied, with the version (the record's latest event position) it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRecord {
    /// Which record.
    pub key: RecordKey,
    /// The version now stored.
    pub version: Position,
}

/// What `apply_bundle` stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppliedSet {
    /// The records stored, in bundle order.
    pub records: Vec<AppliedRecord>,
}

impl AppliedSet {
    /// The keys of the stored records.
    #[must_use]
    pub fn keys(&self) -> Vec<RecordKey> {
        self.records.iter().map(|r| r.key.clone()).collect()
    }
}

/// The sink refused the whole bundle (nothing was stored). Carries a short, secret-free reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyRefusal {
    /// Why, without any secret.
    pub reason: String,
}

/// A bundle was applied (or was a no-op replay).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Applied {
    /// What this delivery stored.
    pub applied: AppliedSet,
    /// Records of this delivery skipped because the same key and version were already applied.
    pub duplicates: usize,
    /// Records of this delivery skipped because a higher version was already applied.
    pub older_ignored: usize,
    /// Parked bundles this delivery unblocked and applied, in arrival order.
    pub released: Vec<AppliedSet>,
}

/// The outcome of receiving one bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Receipt {
    /// Applied atomically (or already applied).
    Applied(Applied),
    /// A reference is missing: the whole bundle is parked until these keys are applied.
    Parked {
        /// The referenced records the node lacks.
        missing: Vec<RecordKey>,
    },
    /// Too large to park at all: sent straight to the repair list.
    Overflow {
        /// The referenced records the node lacks.
        missing: Vec<RecordKey>,
    },
    /// The node cannot decode a record of the bundle: held, never parked or applied.
    Held(HoldReason),
    /// The sink refused the bundle; nothing was stored and the prefix stays behind it.
    Refused(ApplyRefusal),
}

/// A bundle waiting for records it references.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedBundle {
    /// Arrival order, assigned by the intake.
    pub seq: u64,
    /// The tenant tag of the queue it sits in.
    pub tenant: String,
    /// The whole bundle, as received.
    pub bundle: Bundle,
    /// The referenced keys still not applied.
    pub missing: BTreeSet<RecordKey>,
    /// Monotonic milliseconds when it was first parked.
    pub parked_at_ms: u64,
    /// The lowest record position in the bundle: where it holds the acknowledged prefix.
    pub first_position: Position,
    /// Size of the bundle's images.
    pub bytes: usize,
}

/// A bundle that left the parking store without being applied: the closure repair loop
/// (WP-SYN-04.T2) asks the hub for `missing` or for a resend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairItem {
    /// The tenant tag.
    pub tenant: String,
    /// The bundle's first position (where it holds the prefix).
    pub first_position: Position,
    /// The referenced records the node lacked.
    pub missing: Vec<RecordKey>,
    /// Why it is here.
    pub cause: RepairCause,
}

/// Why a bundle is on the repair list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairCause {
    /// Pushed out by the count or byte bound.
    Bound,
    /// Parked longer than the maximum age.
    Expired,
    /// Larger than the byte bound on its own.
    Oversize,
    /// The sink refused it on a retry.
    Refused,
}

/// Parking bounds (settings, never hidden constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkingLimits {
    /// Most parked bundles per tenant queue.
    pub max_bundles: usize,
    /// Most parked image bytes per tenant queue.
    pub max_bytes: usize,
    /// A parked bundle older than this many monotonic milliseconds is moved to repair.
    pub max_age_ms: u64,
}

impl Default for ParkingLimits {
    fn default() -> Self {
        Self {
            max_bundles: 1_000,
            max_bytes: 64 * 1024 * 1024,
            max_age_ms: 24 * 60 * 60 * 1000,
        }
    }
}

impl ParkingLimits {
    /// Reject limits that cannot work.
    ///
    /// # Errors
    /// `InvalidLimits` when a limit is zero.
    pub fn validate(&self) -> Result<(), NodeError> {
        if self.max_bundles == 0 || self.max_bytes == 0 || self.max_age_ms == 0 {
            return Err(NodeError::InvalidLimits {
                reason: "every limit must be above zero",
            });
        }
        Ok(())
    }
}

/// How the node is configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeConfig {
    /// The only source a delivery may come from.
    pub hub_source: SourceId,
    /// Parking bounds.
    pub limits: ParkingLimits,
    /// Most tenants the node keeps state for. A delivery for a new tenant beyond it is refused,
    /// so a hostile sender cannot grow memory by inventing tenant tags.
    pub max_tenants: usize,
}

/// A default cap on tenants per node.
pub const DEFAULT_MAX_TENANTS: usize = 1_024;

/// Counters the health surface reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Bundles moved to the repair list for any cause.
    pub to_repair: u64,
    /// Of those, pushed out by a bound.
    pub dropped_by_bound: u64,
    /// Records skipped as exact replays.
    pub duplicates: u64,
    /// Records skipped because a higher version was applied.
    pub older_ignored: u64,
}
