//! Snapshot providers for tenant databases and managed PostgreSQL instances, with
//! restore and invariant checks (WP-MIG-05, ADR-058, ADR-068).
//!
//! The crate defines the [`SnapshotProvider`](provider::SnapshotProvider) interface and
//! ships two providers that work on every platform: [`PgDumpProvider`](pgdump::PgDumpProvider)
//! (online-consistent logical dumps) and [`StoppedInstanceProvider`](stopped::StoppedInstanceProvider)
//! (fast offline copies of a supervised instance's data directory). A canonical
//! [`manifest::Snapshot`] names every file with a SHA-256 checksum; a snapshot is only
//! complete — and only openable — once its `snapshot.toml` is written atomically beside
//! the data. The brief's boundary: a snapshot is tenant data (ADR-058 rule 1); it is
//! written only where the caller says, its location is recorded in the manifest, and
//! nothing here sends it anywhere.
//!
//! Failure behaviour follows the fixed list of ADR-068 rule 1: a failing copy of a
//! stopped instance still restarts the instance (the guard in [`stopped`]); a restore
//! into a populated target is refused unless the caller passes `replace` with a
//! confirmation token and the old data is moved aside, never dropped by this crate;
//! invariants that fail leave the restore in a clearly named non-activated state
//! ([`restore::Resourced`]).

pub mod conn;
pub mod error;
pub mod facts;
pub mod hash;
pub mod manifest;
pub mod pgdump;
pub mod provider;
pub mod restore;
pub mod scratch;
pub mod stopped;

pub use error::{SnapshotError, SnapshotResult};
pub use facts::{DatabaseFacts, shipped_checks};
pub use manifest::{Consistency, Snapshot, SnapshotManifest};
pub use pgdump::PgDumpProvider;
pub use provider::{
    Capabilities, Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget,
    SnapshotOptions, SnapshotProvider, SnapshotTarget, TakeOutcome, VerifyReport,
};
pub use restore::{
    InvariantCheck, Restored, Violation, invariants_failed_name, post_restore, reset_node_local,
    restore_and_check, run_checks,
};
pub use scratch::{DropOutcome, ScratchMarker, drop_scratch, drop_scratch_dir};
pub use stopped::StoppedInstanceProvider;

/// The PostgreSQL major version this crate snapshots and restores. Any other major
/// version in a manifest is refused at verify and at restore (`snapshot.version_mismatch`);
/// the remedy tells the operator that a logical restore into another major may be possible.
pub const SUPPORTED_MAJOR_VERSION: u32 = spot_postgres::SUPPORTED_MAJOR_VERSION;

/// The crate's provider name and version, as the manifest records them.
pub const PROVIDER_NAME: &str = "spot-snapshot";

/// The workspace version the manifest records as the provider's version.
pub const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Key under which a manifest's `node_local` table records the node id, together with
/// its value so the caller can reset it after a restore (the brief's item 5).
pub const NODE_LOCAL_NODE_ID_KEY: &str = "node_id";
