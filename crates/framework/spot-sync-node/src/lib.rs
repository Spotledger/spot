//! `spot-sync-node`: the receiving side of sync (WP-SYN-04.T1; ADR-064 sections 6, 7 and 9,
//! ADR-065 sections 1 and 4).
//!
//! [`NodeIntake::receive`] applies a bundle atomically when every reference of every record is
//! satisfied (already applied, or inside the bundle, cycles together). Otherwise it **parks the
//! whole bundle**, never part of it, and retries parked bundles in arrival order as records
//! arrive. A record whose links dangle is never applied.
//!
//! * Everything is per tenant (each tenant has its own journal and position space, ADR-061
//!   section 1): the applied-record check, the prefix, freshness and skew holds. A node keeps
//!   state for at most `NodeConfig::max_tenants` tenants.
//! * Acknowledgement is of the tenant's applied prefix ([`NodeIntake::applied_prefix`]): a parked bundle
//!   holds the prefix back at its first position; a bundle held for version skew never does.
//! * Parking is bounded by count and bytes per tenant queue; the oldest parked bundle is pushed
//!   to the repair list (the closure repair loop, WP-SYN-04.T2) and counted.
//! * Replays are idempotent: the same key and version is a no-op, a lower version is ignored,
//!   both counted.
//! * The freshness tracker is fed from the hub's horizons ([`NodeIntake::on_horizons`]).
//!
//! A pure library over traits ([`BundleApply`], [`ParkingStore`], [`NodeCatalog`]): no network,
//! clock, randomness, database or async runtime. Times are monotonic millisecond arguments.
//!
//! Out of scope: transport, pull-on-miss requests (WP-SYN-04.T2), the real apply sink, stamps.

mod error;
mod intake;
mod model;
mod parking;
mod prefix;
mod traits;

pub use error::{
    FOREIGN_SOURCE, INVALID_LIMITS, NodeError, REFERENCE_UNREADABLE, TOO_MANY_TENANTS,
};
pub use intake::NodeIntake;
pub use model::{
    Applied, AppliedRecord, AppliedSet, ApplyRefusal, Counters, DEFAULT_MAX_TENANTS, Delivery,
    NodeConfig, ParkedBundle, ParkingLimits, Receipt, RepairCause, RepairItem,
};
pub use parking::MemoryParking;
pub use prefix::PrefixTracker;
pub use traits::{BundleApply, NodeCatalog, ParkingStore};
