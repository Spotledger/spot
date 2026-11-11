//! `spot-bpm-defs-pg`: the PostgreSQL [`DefinitionStore`](spot_bpm_definitions::DefinitionStore)
//! (WP-BPM-09.T2, ADR-49).
//!
//! [`PgDefinitionStore`] keeps published process versions in `spot_bpm_definition`, immutable
//! (see [`schema`]), with `spot_bpm_definition_head` as the atomic version counter.
//!
//! # Guarantees
//!
//! * **Atomic numbering.** `publish` runs in one transaction: it locks the head row by
//!   incrementing it only `WHERE latest = expected`, so two publishes with the same expectation
//!   cannot both win and numbers have no gaps. A refused publish rolls back and leaves nothing.
//! * **Immutable.** A trigger refuses any change to a stored version except `published ->
//!   retired`, and any delete; `CHECK`s hold the 256 KiB cap and the name grammar in the database.
//! * **One tenant.** The tenant is the store's constructor argument ([`PgDefinitionStore::with_tenant`]);
//!   it is in every primary key and every statement. No method takes a tenant.
//! * **Fail closed.** A backend failure, a closed pool or a corrupt row is
//!   [`DefinitionError::StoreFailed`](spot_bpm_definitions::DefinitionError::StoreFailed) with the
//!   store name and the SQLSTATE class; never an empty result. Nothing here logs.
//! * **Fixed statements.** Every statement in [`sql`] is constant text with bound parameters
//!   (ADR-45); no identifier or fragment is built from input.
//!
//! `running_pins` reads `spot_bpm_case` (the Case table of `spot-bpm-pg`) when it exists in the
//! database; a database without that table has no Case, so no pin.
//!
//! The integration tests need a PostgreSQL server and skip (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset; with `SPOT_REQUIRE_PG=1` a missing server fails, as in CI.

#![forbid(unsafe_code)]

pub mod error;
pub mod schema;
pub mod sql;
mod store;

pub use store::{DEFAULT_TENANT, PgDefinitionStore};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-defs-pg";
