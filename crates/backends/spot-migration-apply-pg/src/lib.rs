//! `spot-migration-apply-pg` — the PostgreSQL wiring of the apply engine (WP-MIG-07.T2,
//! blueprint WS12, ADR-22).
//!
//! [`spot-migration-apply`](spot_migration_apply) is the state machine: it decides *when* a
//! site closes, which step runs next, when to roll back and when users may return. This crate
//! supplies the durable, PostgreSQL-backed pieces that state machine runs on:
//!
//! - [`PgApplyJournal`] writes the write-ahead journal into the **control schema**
//!   `spot_migration_control`, outside every tenant table. `spot-pg-migrate` only reads the
//!   current schema (`public`), so a migration can never diff, rewrite or drop the journal.
//! - [`PgAdmissionGate`] is the gate every request path consults. It caches the gate row for a
//!   short TTL, is invalidated immediately through a cluster-wide counter in
//!   `spot-coordinator-pg`, and **fails closed**: when the gate state cannot be read, the gate
//!   is reported closed.
//! - [`PgStepRunner`] runs one remaining `spot_pg_migrate` step per engine step, so a crash
//!   leaves the schema at a point the planner can re-diff. Transactional DDL commits or aborts
//!   with the step; the non-transactional statements (`CREATE INDEX CONCURRENTLY`,
//!   `DROP INDEX CONCURRENTLY`, and the batched backfill whose batches commit on their own)
//!   record a compensating restore point instead.
//! - [`PgSnapshotPort`] adapts the [`spot_snapshot`] providers (logical `pg_dump` and stopped
//!   instance) to the engine's [`SnapshotPort`] seam and records take and restore timings.
//! - [`RealtimeProgress`] publishes `migration.progress` notifications to the tenant's
//!   `doctype:migration.progress` room; [`AdminOnly`] is the [`RoomAuthorizer`] that admits
//!   only named administrators to it.
//! - [`PgDriftVerifier`] is the verify gate over `spot-pg-migrate`'s own re-plan: the new
//!   version is verified only when the live schema needs no further step.
//!
//! # Time is an argument
//!
//! Every operation takes the instant it acts at (`now`, unix microseconds) from its caller.
//! The only clock this crate reads is the injected [`spot_core::Clock`], used for cache ages
//! and timing capture; it never calls `OffsetDateTime::now_utc` or `Instant::now` itself.
//!
//! # The control schema
//!
//! ```sql
//! spot_migration_control.journal   -- (site, seq) -> the write-ahead records
//! spot_migration_control.gate      -- (site) -> open/closed, reason, since, epoch
//! spot_migration_control.snapshot  -- (site, snapshot_id) -> the directory a take wrote
//! spot_migration_control.timing    -- one row per snapshot, apply and restore measurement
//! ```
//!
//! [`PgApplySchema::prepare`] creates them idempotently under a transaction-scoped advisory
//! lock, the convention `spot-pg-migrate` and `spot-coordinator-pg` use for their own tables.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codes;
mod error;
mod gate;
mod journal;
mod progress;
mod schema;
mod snapshot;
mod steps;
mod timings;
mod verify;

pub use error::{Error, Result};
pub use gate::{PgAdmissionGate, UNREADABLE};
pub use journal::PgApplyJournal;
pub use progress::{AdminOnly, MIGRATION_ROOM, PROGRESS_KIND, RealtimeProgress};
pub use schema::{CONTROL_SCHEMA, PgApplySchema, TABLES};
pub use snapshot::PgSnapshotPort;
pub use steps::{NonSchemaStep, PgStepRunner, RefuseNonSchemaSteps, StepShape};
pub use timings::{
    NoTimings, PgTimingLog, RecordingTimings, TimingPhase, TimingRecord, TimingSink,
};
pub use verify::PgDriftVerifier;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-migration-apply-pg";
