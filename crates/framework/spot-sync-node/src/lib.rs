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
//! * What the node acknowledges to the hub is the **serve-sequence prefix**
//!   ([`NodeIntake::served_prefix`], WP-SYN-11.T2): the hub's queue numbers every bundle it
//!   sends, and a bundle received with [`NodeIntake::receive_served`] counts into the prefix only
//!   when everything the queue held below it has. A parked, refused or repaired bundle holds the
//!   prefix back; a bundle held for version skew never does. The prefix cannot claim a bundle the
//!   node was never sent, however the hub orders its lanes.
//! * The journal prefix ([`NodeIntake::applied_prefix`]), the highest position settled with a parked
//!   bundle holding it back at its first position, stays for the freshness tracker and the
//!   handshake; it is **not** what the hub trims by.
//! * Parking is bounded by count and bytes per tenant queue; the oldest parked bundle is pushed
//!   to the repair list (the closure repair loop, WP-SYN-04.T2) and counted.
//! * Replays are idempotent: the same key and version is a no-op, a lower version is ignored,
//!   both counted.
//! * Deletions travel as tombstones (`BundleRecord::deleted`): the sink applies one as
//!   [`BundleApply::delete`] inside the bundle's transaction, and it counts as applied for the
//!   prefix. A tombstone is never held for skew. A delete the sink applied already is a counted
//!   replay, and an old image a later delete supersedes is ignored (never brings the record
//!   back). A bundle that references a deleted record is **parked**, because the key is not
//!   present: when the hub answers `Gone` for it ([`NodeIntake::resolve_gone`]) the reference is
//!   released and the bundle is applied.
//! * A restarted node seeds each tenant with [`NodeIntake::restore`] (a [`RestoreState`] from its
//!   durable stores) before the first delivery: parked and repair-listed bundles hold the prefix
//!   again, so nothing unapplied is acknowledged.
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
mod restore;
mod serve;
mod traits;

pub use error::{
    FOREIGN_SOURCE, INVALID_LIMITS, NodeError, REFERENCE_UNREADABLE, TOO_MANY_TENANTS,
};
pub use intake::NodeIntake;
pub use model::{
    Applied, AppliedRecord, AppliedSet, ApplyRefusal, Counters, DEFAULT_MAX_TENANTS, Delivery,
    GoneOutcome, MAX_GONE_PER_TENANT, NodeConfig, ParkedBundle, ParkingLimits, Receipt,
    RepairCause, RepairItem,
};
pub use parking::MemoryParking;
pub use prefix::PrefixTracker;
pub use restore::{RESTORE_REFUSED, RestoreError, RestoreState};
pub use serve::{ServeStamp, ServeTracker};
pub use spot_sync_closure::{Position, RecordKey};
pub use spot_sync_skew::HoldSet;
pub use traits::{BundleApply, NodeCatalog, ParkingStore};
