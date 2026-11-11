//! The PostgreSQL [`WrappedKeyStore`](spot_journal_pii_secrets::WrappedKeyStore) for the
//! per-subject journal keys (WP-JRN-08.T6, ADR-061 s6 and s9).
//!
//! `spot-journal-pii-secrets` keeps each subject's key wrapped by the tenant's key provider and
//! leaves "where" to a port. Its in-memory double loses every key on a restart and shows a
//! destroy to one node only. [`PgWrappedKeyStore`] makes the port durable: the wrapped keys and
//! the erasure tombstones live in one table per tenant database ([`schema`]), so a restart keeps
//! every key and a tombstone written by one node is what every other node reads when its cache
//! revalidates.
//!
//! The port is synchronous with no async alternative (ADR-082 addendum 1, class C), so the store
//! reaches the database through the shared [`spot_pg_bridge::Bridge`]; there is no `block_on`
//! here. The crate never sees key material: the value stored is the wrapped form the adapter
//! gives, and nothing here logs a value, a wrapped byte or an entry id.
//!
//! # Semantics
//!
//! * `get`: `Live`, `Destroyed`, or `None` when nothing was ever stored.
//! * `put_if_absent`: `INSERT .. ON CONFLICT (entry) DO NOTHING`; `true` only when a row was
//!   inserted. It never overwrites a live key or a tombstone.
//! * `destroy`: in one transaction, a live row becomes a tombstone (the wrapped bytes are set to
//!   null in the same statement) and an absent entry gets a tombstone; it returns whether a live
//!   key was removed, and exactly one of several concurrent destroys gets `true`.
//! * Every failure is a [`PortError`](spot_journal_pii_secrets::PortError), never "absent". A
//!   timed-out write may or may not have been applied; the adapter treats it as an error.

#![warn(missing_docs)]

mod error;
pub mod schema;
mod store;

pub use error::{ROW_INVALID, STORE_FAILED, TABLE_MISSING, TableError};
pub use schema::{DDL, TABLE, ensure_present, extra_table};
pub use store::PgWrappedKeyStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-journal-pii-secrets-pg";
