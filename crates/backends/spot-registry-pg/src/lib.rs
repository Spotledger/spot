//! `spot-registry-pg`: the PostgreSQL implementation of `IndexStore` (WP-STR-03.T2, ADR-082
//! class C item 2).
//!
//! # Where it may be used
//!
//! A synchronous `IndexStore` over an async driver is legitimate only because of where it runs
//! (ADR-082 section 2 and addendum A1 to A5):
//!
//! - **a control-plane process, a CLI command or a job worker**, never a request handler, a hook,
//!   a lifecycle step or a realtime task (the call is refused with `pg_bridge.wrong_thread` on any
//!   tokio runtime thread);
//! - **never inside a transaction an async store holds**: every call takes its own pooled
//!   connection and its own short transaction;
//! - **bounded by construction**: every method passes a stated deadline to the shared
//!   `spot-pg-bridge` ([`READ_DEADLINE`], [`WRITE_DEADLINE`], [`SEARCH_DEADLINE`]); there is no
//!   private `block_on`;
//! - **fails visibly**: a bridge refusal or a database error is the registry's own
//!   `registry.storage` refusal with a fixed sentence, never a default value and never `Ok(None)`
//!   for "database down".
//!
//! # Several registries, one database
//!
//! Every mutation takes a transaction-scoped advisory lock on the app it touches before it reads
//! the ownership, publisher and version rows, and repeats the checks of the in-memory reference
//! under that lock. Two registry instances sharing the database therefore behave as one: exactly
//! one first claim wins, versions only increase, a re-commit is idempotent, a removal marks once
//! and a transfer is a compare-and-swap on the signed ownership generation.
//!
//! # Data
//!
//! See [`schema`]. The signed evidence of a release (signature, certificate, capabilities,
//! dependencies) is stored as `jsonb`, built field by field from the models' public parts; a row
//! that no longer rebuilds is a storage failure, never guessed at.
//!
//! # Time, secrets, SQL
//!
//! The crate reads no clock (every instant is an argument), uses no floats and no `unsafe`.
//! Every statement is static text with bound parameters; error text carries no SQL, no
//! connection string and no row data.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod codec;
mod error;
mod ops;
pub mod schema;
mod search;
mod store;

pub use crate::error::{BAD_SETUP, MIGRATION_FAILED, SetupError};
pub use crate::store::{
    PREPARE_DEADLINE, PgIndexStore, READ_DEADLINE, SEARCH_DEADLINE, WRITE_DEADLINE,
};
