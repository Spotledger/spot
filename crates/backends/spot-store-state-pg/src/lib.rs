//! `spot-store-state-pg`: the PostgreSQL implementation of `StoreStateStore` (WP-STR-03.T4,
//! ADR-082 class C), the store server's own durable state in the control schema.
//!
//! # Where it may be used
//!
//! A synchronous store over an async driver is legitimate only because of where it runs
//! (ADR-082 section 2 and addendum A1 to A5):
//!
//! - **a control-plane process on dedicated plain OS worker threads**, never a request handler,
//!   a hook or a realtime task, and never a tokio runtime thread: `spawn_blocking` threads count
//!   as inside the runtime, so the shared bridge refuses them with `pg_bridge.wrong_thread` (the
//!   call is refused and nothing changes; it comes back as `store_state.backend`);
//! - **never inside a transaction an async store holds**: every call takes its own pooled
//!   connection and its own short transaction;
//! - **bounded by construction**: every call passes a stated deadline ([`READ_DEADLINE`],
//!   [`WRITE_DEADLINE`]) to the shared `spot-pg-bridge`; there is no private `block_on`;
//! - **fails visibly**: a bridge refusal or a database error is `store_state.backend` with a
//!   fixed sentence, never a default value and never `Ok(None)` for "database down".
//!
//! # Pool and bridge sizing
//!
//! As for `spot-registry-pg`: the bridge's `max_in_flight` must be at least one below the pool
//! size minus the bridge's reserved connections ([`PgStoreState::new`] validates this and refuses
//! with `store_state.bad_setup`). The store server runs one bridge for the whole process and
//! shares it with the registry index, so size the pool for both; a calling thread beyond
//! `max_in_flight + queue_capacity` is refused with `pg_bridge.busy` at once.
//!
//! # The listing fails closed
//!
//! Only an explicit `listed` row lists a version; see `spot-store-state`. The transition is one
//! statement (an insert for the start, a compare-and-set on the previous state otherwise), so two
//! racing writers cannot both win, across processes. `next_generation` is one `UPDATE .. RETURNING`
//! on a single counter row.
//!
//! # Secrets, SQL, time
//!
//! Publisher keys are stored as their SHA-256 only. No error or log text carries a key, a key
//! hash, a summary, SQL or a connection string. Every statement is static text with bound
//! parameters. The crate reads no clock, uses no floats and no `unsafe`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod ops;
pub mod schema;
mod store;

pub use crate::error::{BAD_SETUP, MIGRATION_FAILED, SetupError};
pub use crate::store::{PREPARE_DEADLINE, PgStoreState, READ_DEADLINE, WRITE_DEADLINE};
