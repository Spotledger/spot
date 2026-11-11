//! `spot-partition` — single-writer partition ownership and per-branch naming ranges
//! (WP-EDG-03.T1; ADR-062, ADR-023).
//!
//! A record has exactly one owning node and flows only from it (ADR-023). This crate makes that
//! rule true in two places and gives a branch its own naming numbers so two branches never
//! collide:
//!
//! * **Ownership.** [`PartitionMap`] maps opaque scope keys to their owning node; a
//!   [`PartitionRegistry`] stores the effective-dated, audited assignments and hands out
//!   snapshots. [`OwnerCheck`] refuses a write by a node that does not own the record's scope
//!   keys ([`OwnerCheck::allow_write`], the document engine's pre-write hook through
//!   [`WriteGate`] and the [`OWNER_CHECK_SEAM`] seam) and refuses a replica whose sender is not
//!   the owner ([`OwnerCheck::allow_replica`], the apply path's check). Both consult the same
//!   map, so a two-writer attempt is rejected by design.
//! * **Per-branch naming.** A [`RangeAllocator`] grants each node a disjoint numeric range of a
//!   naming series ([`RangeLease`]); a [`BranchSeries`] allocates inside its lease with no
//!   coordination, records its high-water mark before each number so a reboot never reissues
//!   one, reports [`BranchSeries::running_low`] so the hub can grant another range in time, and
//!   fails with `partition.range_exhausted` when a lease runs out offline instead of ever
//!   reusing a number.
//!
//! # The platform never interprets a scope key
//!
//! A scope key is an opaque `(key type, value)` pair an app defines (ADR-062 section 1). Nothing
//! in this crate knows what `warehouse` or `company` means; it compares keys, routes ownership
//! and stops there.
//!
//! # Time is an argument
//!
//! No function reads the wall clock. Effective dates, lease grants and audit instants are
//! [`OffsetDateTime`](time::OffsetDateTime) values a caller passes in (ADR-077). Lease expiry is
//! decided from those instants, never from a clock read.
//!
//! # What is not decided here
//!
//! The policy vocabulary (presets, `owner_key`, the node registry, scope-key holdings) is
//! ADR-063, still *Proposed*. This crate implements the mechanism behind traits and leaves the
//! policy choices to a later work package; the places a later ADR will matter are listed under
//! `## Open questions` in the task's report.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//!
//! use spot_partition::{
//!     AssignmentRequest, MemoryPartitionRegistry, OwnerCheck, PartitionRegistry, ScopeKey,
//! };
//! use spot_sync_protocol::NodeId;
//! use time::OffsetDateTime;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let at = OffsetDateTime::from_unix_timestamp(1_700_000_000)?;
//! let warehouse = ScopeKey::new("warehouse", "W7")?;
//! let branch = NodeId::new("edge-khi#branch_07")?;
//! let registry = MemoryPartitionRegistry::new();
//! registry.assign(AssignmentRequest {
//!     scope: warehouse.clone(),
//!     node: branch.clone(),
//!     effective_from: at,
//!     effective_to: None,
//!     actor: "admin".to_owned(),
//!     reason: "branch 7 opens".to_owned(),
//!     at,
//! })?;
//! let check = OwnerCheck::new(Arc::new(registry.map_at(at)));
//! assert!(check.allow_write(&branch, "stock_move", std::slice::from_ref(&warehouse)).is_ok());
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

pub mod error;
pub mod lease;
pub mod map;
pub mod notify;
pub mod owner;
pub mod participant;
pub mod scope;
pub mod series;

pub use error::{NotOwner, PartitionError};
pub use lease::{LeaseId, MemoryRangeAllocator, RangeAllocator, RangeGrant, RangeLease};
pub use map::{
    Assignment, AssignmentRequest, MemoryPartitionRegistry, PartitionChange, PartitionMap,
    PartitionRegistry,
};
pub use notify::{
    Allocated, OutboxWriter, RANGE_LOW_TOPIC, RunningLowNotice, RunningLowNotifier, TxOutbox,
    running_low_event,
};
pub use owner::{
    OWNER_CHECK_SEAM, OwnerCheck, OwnerCheckSeam, ReplicaRecord, ReplicaView, WriteGate,
    check_write,
};
pub use participant::{DocumentRecord, OwnerParticipant, ScopeFields};
pub use scope::ScopeKey;
pub use series::{
    BranchSeries, HighWaterStore, InstallDecision, LowWater, MemorySeriesStore, StoredSeries,
};
