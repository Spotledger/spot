//! The PostgreSQL backend of the storage contract (ADR-02, ADR-052, ADR-053).
//!
//! This crate grows over several tasks. WP-STO-01.T1 builds the connection and transaction
//! layer: the pool, [`PgBackend`], [`PgTransaction`] (declared isolation, lock and statement
//! timeouts, savepoints, an honest commit outcome), the SQLSTATE mapping and the `testing`
//! harness. Documents, queries, locks, sequences, graph and the outbox live in the area modules
//! and return `Unsupported` until the work package named in each message lands.
//!
//! Rules every module follows: values reach SQL only as bound parameters and identifiers only
//! from the validated catalog (ADR-45); no operation sends SQL to a connection whose transaction
//! is known to be aborted (ADR-46).

mod admin;
pub use admin::{create_database, database_exists};

mod backend;
mod config;
mod conn;
mod error;
mod tx;

mod documents;
mod feed;
mod graph;
mod locks;
mod query;
mod schema;
mod sequences;

#[cfg(feature = "testing")]
pub mod testing;

pub use backend::PgBackend;
pub use config::{PgConfig, Tls};
pub use error::kind_for_sqlstate;
pub use feed::{Cleanup, cleanup, envelopes, retention, set_retention, start_new_generation};
pub use tx::PgTransaction;

/// The PostgreSQL major version this backend supports (architecture decision of 29 Sep 2026).
pub const SUPPORTED_MAJOR_VERSION: u32 = 18;
