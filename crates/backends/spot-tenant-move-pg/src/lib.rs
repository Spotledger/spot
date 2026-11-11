//! `spot-tenant-move-pg`: the PostgreSQL implementation of `MoveStore` (WP-CTL-02.T2, ADR-082
//! class C item 3).
//!
//! # Where it may be used
//!
//! A synchronous `MoveStore` over an async driver is legitimate only because of where it runs
//! (ADR-082 section 2 and addendum A1 to A5):
//!
//! - **a control-plane process, a CLI command or a job worker**, never a request handler, a hook,
//!   a lifecycle step or a realtime task (the call is refused with `pg_bridge.wrong_thread` on any
//!   tokio runtime thread);
//! - **never inside a transaction an async store holds**: every call takes its own pooled
//!   connection and its own short transaction;
//! - **bounded by construction**: every method passes a stated deadline to the shared
//!   `spot-pg-bridge` ([`READ_DEADLINE`], [`WRITE_DEADLINE`]); there is no private `block_on`;
//! - **fails visibly**: a bridge refusal or a database error is the trait's own
//!   `Error::StoreFailed` with a fixed reason, never a default value and never `Ok(None)` for
//!   "database down".
//!
//! # Data
//!
//! See [`schema`]: `spot_control.spot_tenant_move` (one row per tenant, optimistic `revision`, the
//! record as `jsonb` in its existing serde form) and the append-only
//! `spot_control.spot_tenant_move_event` (the state history, written in the same transaction as
//! the state change). A stored document that no longer deserialises is a store failure, never
//! guessed at.
//!
//! # Time, secrets, SQL
//!
//! The crate reads no clock (every instant is in the record), uses no floats and no `unsafe`.
//! Every statement is static text with bound parameters; error text carries no SQL, no
//! connection string and no row data.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
pub mod schema;
mod store;

pub use crate::error::{BAD_SETUP, MIGRATION_FAILED, SetupError};
pub use crate::store::{PREPARE_DEADLINE, PgMoveStore, READ_DEADLINE, WRITE_DEADLINE};
