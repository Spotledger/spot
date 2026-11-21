//! The PostgreSQL [`ConsumerStore`](spot_event_stream::ConsumerStore) (WP-INT-02.T2).
//!
//! Event-stream consumers acknowledge before they advance, so the checkpoint must survive a
//! restart or every consumer would replay or skip. [`PgConsumerStore`] keeps the consumer
//! definitions and the per-partition checkpoints and parking state in two tables created by
//! [`PgConsumerStore::prepare`] (see [`schema`]); it implements the object-safe trait exactly as
//! `InMemoryConsumerStore` does and passes the shared `spot_event_stream::contract::run_all`
//! suite.
//!
//! # Rules the store keeps
//!
//! - **Scoped by site.** A store is built for one `SiteId`; every key and every statement
//!   starts with it. Two sites in one database never see each other's consumers, even under the
//!   same consumer name.
//! - **`advance` is one conditional `UPDATE`** (`WHERE checkpoint = expected`). Of two racing
//!   writers presenting the same `expected`, exactly one changes the row; the other matches
//!   nothing and gets `eventstream.ack_refused`. A move backwards is refused without a write;
//!   `to == expected` succeeds without a write when `expected` is the stored checkpoint (a stale
//!   writer is refused even then).
//! - **`restart` is the only way backwards** and also clears parking, as the in-memory store
//!   does.
//! - **`register` replaces the whole record** (definition, checkpoints, parking, registration
//!   instant) in one transaction, as the in-memory store does: the checkpoints of the record
//!   given are the stored ones, so a caller that must keep existing progress passes the stored
//!   record (the engine does: it refuses a changed definition and returns the stored record for
//!   an identical one without calling `register`). Partition rows beyond a smaller count are
//!   removed.
//! - **Unknown names and partitions** answer `eventstream.consumer_not_found` and
//!   `eventstream.spec_invalid`, as the in-memory store does.
//! - **Reads fail closed.** A consumer whose rows do not form a whole record (a missing
//!   partition, an unparsable definition, a negative position) is a storage error, never a
//!   partial record.
//! - Every statement is a primary-key lookup or a primary-key prefix range; every value is a
//!   bound parameter; the store reads no clock (instants are arguments).
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do; CI sets it and the suite runs.

#![warn(missing_docs)]

pub mod error;
pub mod schema;
mod sql;
mod store;

pub use error::MIGRATION_FAILED;
pub use store::PgConsumerStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-event-stream-pg";
