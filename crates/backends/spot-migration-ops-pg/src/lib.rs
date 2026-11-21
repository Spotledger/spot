//! The PostgreSQL stores of the migration lifecycle (WP-MIG-04.T6, ADR-082 class C).
//!
//! [`PgMigrationStore`] implements, over one shared [`spot_pg_bridge::Bridge`]:
//!
//! * [`spot_migration_ops::PlanStore`]: the canonical plan text by hash, the author, and the
//!   state machine position. A plan row is immutable except for `state` and `version`; a state
//!   change is `UPDATE ... WHERE state = $expected AND version = $v`, so a lost race is
//!   `migration.plan_changed`, never a blind overwrite. The digest is recomputed over the stored
//!   text on every load: a mismatch is `migration.plan_changed` and a [`TamperAlert`].
//! * [`spot_migration_approval::ApprovalStore`]: approvals are append-only (a revoking decision
//!   is a new row), and the plan author is recorded once, so separation of duties survives a
//!   restart.
//! * [`RehearsalLedger`]: the rehearsal report by plan hash and clock.
//!
//! # Tables
//!
//! Four tables (see [`schema`]), named with the platform's `spot_` prefix: `spot_migration_plan`,
//! `spot_migration_plan_author`, `spot_migration_approval`, `spot_migration_rehearsal`. The host
//! passes [`schema::extra_tables`] to the migrator's `PlanOptions::extra_tables`, so `spot migrate`
//! creates them like any other framework table; [`PgMigrationStore::protect`] then installs the
//! application role's grants and the guard triggers (a migrator step cannot carry them).
//!
//! # Where it may be used
//!
//! The traits are synchronous, so the stores reach the database through the shared bridge:
//! control-plane processes and CLI commands only, never a request handler.
//!
//! # Retention
//!
//! Nothing here deletes a row. Applied plans are kept for the site's audit period (a setting
//! the host enforces by archiving, never through this crate).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod approvals;
mod ledger;
mod plans;
pub mod schema;
mod store;

pub use ledger::{RehearsalLedger, RehearsalRecord};
pub use plans::MAX_PLAN_BYTES;
pub use store::{PgMigrationStore, RecordingAlert, TamperAlert};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-migration-ops-pg";
