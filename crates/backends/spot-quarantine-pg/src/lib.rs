//! The PostgreSQL [`QuarantineStore`](spot_quarantine::QuarantineStore) (WP-EDG-06.T4, ADR-062).
//!
//! `spot-quarantine` keeps the queue's logic and an in-memory reference store; a queue that lives
//! only in memory loses money-relevant records on a restart. [`PgQuarantineStore`] makes it
//! durable. It keeps two tables, created by [`PgQuarantineStore::prepare`] from the DDL in
//! [`schema`]:
//!
//! - `spot_quarantine_item`: the metadata (sender, stream position, apply key, reason, detail,
//!   first-seen, attempts, state) under an id the database assigns, so ids increase with
//!   insertion and the queue keeps its order across a restart.
//! - `spot_quarantine_payload`: the received record image as JSON text, exactly as it arrived.
//!   **Write-once**: triggers refuse any `UPDATE`, `DELETE` or `TRUNCATE`, so not even a
//!   connection that bypasses this crate can edit or lose the sender's data.
//!
//! Rules it keeps:
//!
//! - `insert` writes the item and its payload in one transaction on one connection.
//! - `update` changes `reason_code`, `detail`, `attempts` and `state` only. The identity of the
//!   item (sender, sequence, apply key, first-seen) cannot change, and a closed (released or
//!   discarded) item cannot be reopened; both are refused rather than ignored.
//! - `find` and `list` filter in SQL, on indexes ([`schema`] lists them); every value is a bind
//!   parameter, no SQL is built from data.
//! - Time is never read: `first_seen` is an argument of the caller.
//! - An error carries the SQLSTATE or a fixed sentence, never a value from a record, and the
//!   store's `Debug` shows no connection detail.
//!
//! Tenancy: like the other `*-pg` stores, the tables live in the tenant's own database and the
//! pool the caller passes is that tenant's, so a store can only ever see its own tenant's queue.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do; CI's Postgres service sets it.

#![warn(missing_docs)]

mod convert;
pub mod error;
pub mod schema;
mod sql;
mod store;

pub use error::MIGRATION_FAILED;
pub use schema::{StateGrant, check_state_grant, check_state_grant_on};
pub use store::PgQuarantineStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-quarantine-pg";
