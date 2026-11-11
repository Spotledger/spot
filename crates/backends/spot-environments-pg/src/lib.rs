//! The PostgreSQL [`EnvironmentStore`](spot_environments::EnvironmentStore) (WP-ALM-02.T2,
//! ADR-082 class C).
//!
//! `spot-environments` keeps the *sets* that link a product's development, test and production
//! tenants in an [`EnvironmentStore`](spot_environments::EnvironmentStore) and ships an
//! in-memory implementation. [`PgEnvironmentStore`] makes the sets durable: one row per set in
//! the control schema ([`schema`]), the set as a validated `jsonb` document and an optimistic
//! `version`.
//!
//! # Where it may be used
//!
//! The trait is synchronous with no async alternative, so the store reaches the database through
//! the shared [`spot_pg_bridge::Bridge`] (there is no `block_on` here). That is legitimate only
//! off the request path: **use it from a control-plane process, a CLI command or a job worker,
//! never from a request handler**, and never inside a transaction an async store holds. Called
//! from a thread inside a tokio runtime the bridge refuses with `pg_bridge.wrong_thread`, and the
//! store returns `environments.store_failed` naming that code. [`PgEnvironmentStore::new`]
//! refuses to be built on a current-thread runtime.
//!
//! # Semantics (the same as `MemoryEnvironmentStore`)
//!
//! * `create`: validates the set, stores it at version 1, or `environments.store_name_taken`.
//! * `get`: `environments.store_not_found` for a missing name. A stored document that no longer
//!   describes a valid set is refused with `environments_pg.row_invalid`, never repaired.
//! * `replace`: a compare-and-swap on the version, in one transaction. `store_not_found` and
//!   `store_version_conflict` are told apart exactly.
//! * `list`: every set ordered by name, read in pages of [`LIST_PAGE`] in one snapshot. Above
//!   [`LIST_CEILING`] sets it fails with `environments.store_failed` rather than truncating.
//! * Every database error or bridge refusal is `environments.store_failed`, with only the
//!   bridge code or the SQLSTATE in the text: never SQL, a connection string, a name or a value.
//!   A timed-out write may or may not have been applied; read the set again before retrying.
//!
//! # Bridge sizing
//!
//! See the README: the store needs one pool connection per call in flight and is meant for a few
//! callers, so `max_in_flight` of 2 on a pool of 4 is a sound default.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod codec;
mod error;
pub mod schema;
mod store;

pub use error::codes;
pub use store::{LIST_CEILING, LIST_PAGE, PgEnvironmentStore, READ_DEADLINE, WRITE_DEADLINE};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-environments-pg";
