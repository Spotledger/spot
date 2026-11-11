//! Moving one tenant between clusters (WP-CTL-02.T1, ADR-34, ADR-058).
//!
//! A tenant is the unit of placement and residency. Moving it is a per-tenant export and
//! import, not a copy of a shared database:
//!
//! 1. [`plan::MovePlan`] names the tenant, the source and target placement, the target
//!    region and the mode. [`residency::ResidencyPolicy`] must allow the target region
//!    before anything is quiesced.
//! 2. [`engine::MoveEngine`] walks the persisted state machine
//!    `Planned -> Quiesced -> Exported -> Imported -> Verified -> CutOver -> Cleaned`.
//!    Every transition and every transferred chunk is saved through [`store::MoveStore`],
//!    so an interrupted move resumes at its last checkpoint.
//! 3. Quiescing closes the maintenance gate and begins the durable side-effect hold
//!    through [`quiesce::QuiescePort`]; the production adapter drives the migration
//!    lifecycle's `AdmissionGate` and `spot-effect-hold`'s `HoldService`.
//! 4. Export and import stream checksummed chunks through [`transfer::TransferPort`]; the
//!    production adapter ([`snapshot_transfer::SnapshotTransfer`]) drives `spot-snapshot`'s
//!    logical dump/restore provider.
//! 5. Before cut-over the source and target fingerprints are measured through
//!    [`verify::FingerprintSource`] (the `spot-rehearsal` engine by default). The digests
//!    and the rows must be identical, or the move halts in `Imported`.
//! 6. Cut-over flips the placement atomically through [`placement::PlacementPort`] and
//!    invalidates the routing caches.
//!
//! Aborting is allowed in every state before `CutOver` and restores the source: it
//! releases the hold, opens the gate, restores any placement a partial cut-over changed
//! and discards the transfer staging.
//!
//! Nothing here reads a clock, a random source, a secret or the network. Time is an
//! argument, integer arithmetic is used for every size and money-adjacent measure, and
//! every public fallible operation reports a typed error with a stable `tenant_move.*`
//! code.

#![warn(missing_docs)]

pub mod codes;
pub mod engine;
pub mod error;
pub mod placement;
pub mod plan;
pub mod quiesce;
pub mod residency;
pub mod snapshot_transfer;
pub mod store;
pub mod transfer;
pub mod verify;

pub use engine::MoveEngine;
pub use error::{Error, Result};
pub use placement::{ControlStorePlacement, PlacementPort, RoutingCacheInvalidator};
pub use plan::{MoveEvent, MoveHalt, MoveId, MoveMode, MovePlan, MoveState};
pub use quiesce::{LifecycleQuiesce, QuiescePort};
pub use residency::{AllowedRegions, ResidencyPolicy, TenancyResidency};
pub use snapshot_transfer::SnapshotTransfer;
pub use store::{MemoryMoveStore, MoveRecord, MoveStore};
pub use transfer::{Chunk, TransferPort, TransferTotals};
pub use verify::{FingerprintSource, RehearsalFingerprintSource, Side, Verification};

/// A boxed future: what the object-safe ports return.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-tenant-move";

/// The largest chunk the engine accounts as one unit, in bytes: 1 MiB. A transfer port
/// may return smaller chunks; a larger one is refused as malformed.
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
