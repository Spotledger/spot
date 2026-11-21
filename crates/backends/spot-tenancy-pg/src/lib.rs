//! The PostgreSQL control store and provisioning executor (WP-CTL-01.T2, ADR-34, ADR-058).
//!
//! `spot-tenancy` (WP-CTL-01.T1) defines the control plane as a pure journaled workflow over a
//! [`ControlStore`](spot_tenancy::ControlStore) and a set of ports. This crate makes two of those
//! durable on a real PostgreSQL server:
//!
//! * [`PgControlStore`] keeps the tenant registry, its placement and its workflow journal in the
//!   [`CONTROL_SCHEMA`] of the control database. Name and placement uniqueness are enforced by
//!   partial unique indexes, so a race between two control nodes loses with a typed error rather
//!   than a duplicate row; every update is optimistic on `version`; a state change appends one
//!   row to `spot_control.tenant_history`; and `insert_within` counts the placement's live tenants under
//!   a transaction-scoped advisory lock, so concurrent provisions can never exceed its capacity.
//! * [`PgProvisioner`] implements the provisioning ports. It creates the tenant's schema in its
//!   placement database, runs the baseline catalogue through `spot-pg-migrate` and seeds the
//!   tenant, in that order, idempotently: a run interrupted at any step can be resumed by the
//!   control plane and ends in the same state as an uninterrupted one. It also implements the
//!   framework's `spot_tenancy_adapters::MigrationRunner`, so the real schema and seed adapters
//!   (WP-CTL-01.T3) drive this executor while the SQL stays in this crate (ADR-45).
//!
//! # The control schema
//!
//! The control store owns three tables in `spot_control`: `tenant` (one row per tenant, its
//! placement, version, retention and name reservation), `tenant_journal` (the resumable
//! provisioning journal) and `tenant_history` (one row per state transition). They are created
//! by [`PgControlStore::prepare`], idempotently and serialised across nodes by a
//! transaction-scoped advisory lock. Nothing is created outside `spot_control` except a tenant's
//! own schema during provisioning.
//!
//! # Time is an argument
//!
//! The store reads no clock: `insert_within` takes `now`, and the transition history carries no
//! timestamp of its own because `ControlStore::update` has none to pass. The provisioner reads
//! no clock either; test timings use a monotonic `Instant` in the test, never in production
//! code.
//!
//! # Connections and the synchronous ports
//!
//! `ControlStore` and the provisioning ports are synchronous while `tokio-postgres` is not. The
//! store and the provisioner therefore take an `Arc<tokio::runtime::Runtime>` and block on it in
//! each call: from a plain thread they call `Runtime::block_on`; from inside a multi-thread
//! runtime they wrap that call in `tokio::task::block_in_place`. Do not use them from a
//! current-thread runtime.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, exactly as the other `*-pg` crates do. In CI the Postgres
//! service sets it and the suite runs.

#![warn(missing_docs)]

mod baseline;
mod blocking;
pub mod error;
mod fault;
mod provisioner;
mod schema;
mod store;

pub use error::{INVALID_BASELINE, MIGRATION_FAILED};
pub use fault::Faults;
pub use provisioner::{PgProvisioner, PlacementDb};
pub use schema::{CONTROL_SCHEMA, DDL, TABLES};
pub use store::PgControlStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-tenancy-pg";
