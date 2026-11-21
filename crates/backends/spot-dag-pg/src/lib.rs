//! The PostgreSQL [`ExecStore`](spot_dag_exec::ExecStore) for DAG run state (WP-DAG-02.T2,
//! ADR-13).
//!
//! `spot-dag-exec` (WP-DAG-02.T1) defines a run as a pure state machine over a compiled graph;
//! this crate makes it durable. [`PgExecStore`] keeps one row per run in `spot_dag_run` and one row
//! per node in `spot_dag_node_state`, both created by [`PgExecStore::prepare`] from the DDL in
//! [`schema`], and implements the object-safe `ExecStore` trait:
//!
//! * [`ExecStore::create_run`](spot_dag_exec::ExecStore::create_run) stores a fresh run and all
//!   its node rows in one transaction; a duplicate run id is `dag_exec.run_exists` and a
//!   duplicate business key is `dag_exec.key_conflict`.
//! * [`ExecStore::apply`](spot_dag_exec::ExecStore::apply) and
//!   [`PgExecStore::apply_in`] run the same body: it takes the run row `FOR UPDATE` (so two
//!   applies of one run serialise), checks the `(run, node, attempt)` idempotency key first (a
//!   retried completion is a no-op), then the optimistic version (`dag_exec.version_conflict`),
//!   applies the transition and bumps the version. It opens no second connection. The rules it
//!   applies are `spot_dag_exec::rules`' — this crate keeps no copy.
//! * [`ExecStore::list_ready`](spot_dag_exec::ExecStore::list_ready) reads the runs that have a
//!   ready node `FOR UPDATE SKIP LOCKED`, so two workers listing at once never block and never
//!   are offered the node another worker is taking.
//! * [`ExecStore::find_by_key`](spot_dag_exec::ExecStore::find_by_key) uses the unique index on
//!   the business key.
//!
//! The store reads no clock and holds no random source: instants and ids are arguments, as the
//! T1 brief requires. It is the reference contract suite's second implementation;
//! `spot_dag_exec::contract::run` is exercised against it on a real database in `tests/`.
//!
//! # The transaction it shares with the caller
//!
//! [`ExecStore::apply`](spot_dag_exec::ExecStore::apply) commits its own transaction, but
//! [`PgExecStore::apply_in`] writes on the caller's [`spot_postgres::PgTransaction`] and leaves
//! the commit to the caller. That is what makes
//! [`Unit::commit_with`](spot_dag_exec::Unit::commit_with) real for this backend: it opens one
//! transaction, runs the caller's document work on it, applies the transition on the same
//! transaction and commits once, so a document write and a node completion commit or roll back
//! together.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, exactly as the other `*-pg` crates do. In CI the Postgres service
//! sets it and the suite runs.

#![warn(missing_docs)]

mod convert;
pub mod error;
pub mod schema;
mod sql;
mod state;
mod store;

pub use error::MIGRATION_FAILED;
pub use store::{PgExecStore, PgExecStoreFactory};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-dag-pg";
