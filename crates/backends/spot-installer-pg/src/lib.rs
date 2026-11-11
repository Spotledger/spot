//! The PostgreSQL [`AppRegistryStore`](spot_installer::AppRegistryStore) for the installer
//! (WP-STR-04.T3, ADR-21).
//!
//! `spot-installer` (WP-STR-04.T2) runs an install plan step by step and can crash-resume, but
//! only if its registry and journal are durable. [`PgAppRegistry`] keeps them in four tables
//! created by [`PgAppRegistry::prepare`] from the DDL in [`schema`]: the installed apps of each
//! site, the site lifecycle flag, the install journal and the DocTypes each app owns.
//!
//! * Every row is keyed by the site and every statement carries a `site` predicate, so two sites
//!   never see each other's rows.
//! * `commit_install`, `commit_upgrade`, `commit_uninstall` and `restore` write the registry
//!   change and the journal row in **one transaction on one connection**; a failure leaves
//!   neither. `commit_upgrade` is refused, not applied, when the live version is not the change's
//!   `from` (the check is part of the `UPDATE`, so two racing upgrades serialise on the row).
//!   `commit_uninstall` with `purge = false` leaves the app's owned DocTypes recorded: its data
//!   is still there.
//! * `commit_install` of an app already installed with the same version and digest is a no-op
//!   plus the journal row (a retried commit); a different version or digest is
//!   `installer.already_installed`.
//! * The journal is unique by `(site, step_index)`: a second write for a step replaces the row.
//! * Time is an argument (`InstallJournal::started_at`); the store reads no clock. Nothing it
//!   stores or returns is a secret: capabilities are endpoints and DocType labels, and errors
//!   carry only a SQLSTATE.
//!
//! `spot_installer::testing::contract::run_all` is exercised against it on a real database in
//! `tests/`, which skip cleanly (printing nothing) when `SPOT_TEST_PG_URL` is unset; the CI
//! Postgres service sets it.
//!
//! Rows are decoded strictly: an unknown tag or a wrong-typed value is
//! `installer_pg.corrupt_row`, never a default.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

mod codec;
pub mod error;
pub mod schema;
mod store;

pub use error::{CORRUPT_ROW, MIGRATION_FAILED, STORAGE_FAILED};
pub use store::PgAppRegistry;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-installer-pg";
