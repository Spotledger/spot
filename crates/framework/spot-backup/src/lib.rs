//! Scheduled backups to a folder, removable drive or cloud target, with retention,
//! verification and one-click restore (WP-PRD-04).
//!
//! # What one backup does
//!
//! 1. A [`spot_scheduler::Schedule`] fires and [`enqueue_due`] turns the slot into a
//!    `spot-jobs` job whose idempotency key is the slot's deduplication key.
//! 2. [`runner::BackupRunner`] asks its [`BackupProducer`](runner::BackupProducer) for a
//!    package. The shipped producer wraps a `spot-snapshot` [`SnapshotProvider`]
//!    (logical dump or stopped instance, whichever the desktop profile chose).
//! 3. The package is a single self-describing file: a `backup.toml` manifest and a
//!    framed payload ([`package`]). It carries the tenant, platform/schema versions,
//!    app set, SHA-256 digest, byte size and chain id.
//! 4. A [`BackupTarget`] stores it. [`FolderTarget`] writes temp + fsync + atomic
//!    rename, [`RemovableTarget`] waits for absent USB media instead of failing, and
//!    [`CloudTarget`] is the port `spot-cloud-backup` implements later.
//! 5. Verification is [`VerifyMode::Quick`] (digest, size, header) or
//!    [`VerifyMode::Full`] (scratch restore, schema checks, L7 invariants through
//!    [`InvariantRunner`]). Only a verified backup becomes the last good one; a failure
//!    raises a health condition and sends an alert.
//! 6. [`retention`] plans a grandfather-father-son keep/delete set, protecting the last
//!    verified backup, recent backups and those an open restore references.
//!
//! # A restore
//!
//! [`restore`] checks the preflight matrix (tenant, PostgreSQL major, schema direction,
//! app set, free space, emptiness), closes the [`ServiceGate`], applies the package,
//! migrates forward when the backup is older, runs schema checks and the L7 invariants,
//! resets node-local state and only then opens the gate. A backup from a newer schema
//! is refused without [`RestoreOptions::allow_newer_schema`]. [`one_click_restore`] is
//! the entry point `spot restore` registers.
//!
//! # Rules
//!
//! Time is always an argument; no function reads the wall clock. Money and quantities
//! are never floats (sizes are integers). No `unsafe`. A secret is never logged or
//! returned: an encryption reference carries only a key id and version.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod manifest;
pub mod model;
pub mod package;
pub mod restore;
pub mod retention;
pub mod runner;
pub mod schedule;
pub mod site;
pub mod target;
pub mod testing;
pub mod verify;

pub use error::{BackupError, Result};
pub use manifest::{BackupManifest, MANIFEST_FILE, MANIFEST_FORMAT};
pub use model::{
    BackupJob, BackupRecord, BackupRequest, BackupRunOutcome, BackupRunReport, KeyRef, SiteInfo,
    StoredBackup, TargetRef, TargetStatus, VerifyMode,
};
pub use package::{PackageEntry, PackageFile, PackagedBackup};
pub use restore::{
    PreflightBlocker, PreflightReport, RESTORE_COMMAND, RestoreOptions, RestorePorts,
    RestoreReport, RestoreStatus, one_click_restore, preflight, restore,
};
pub use retention::{KeepDecision, KeepReason, RetentionPlan, RetentionPolicy};
pub use runner::{BackupLedger, BackupProducer, BackupRunner, InMemoryLedger, SnapshotProducer};
pub use schedule::{BACKUP_APP, BACKUP_JOB_NAME, BackupSchedule, enqueue_due, slot_instant};
pub use site::{
    AlertSink, BackupAlert, BoxFuture, HealthCondition, HealthSeverity, HealthSink,
    InvariantReport, InvariantRunner, MediaNotifier, MigrationApplied, RestoreTargetSite,
    RestoredSite, SchemaCheck, ScratchSite, ServiceGate,
};
pub use target::{
    BackupTarget, CloudObject, CloudStore, CloudTarget, FakeVolumeProbe, FixedSpaceProbe,
    FolderTarget, RecordingCloud, RemovableTarget, SpaceProbe, SystemSpaceProbe, SystemVolumeProbe,
    Volume, VolumeProbe,
};
pub use verify::{VerifyReport, full, quick};

/// The crate's own name, for messages and tests.
pub const CRATE_NAME: &str = "spot-backup";
