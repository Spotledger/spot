//! `spot-protected-keys-pg` -- the PostgreSQL [`KeyStore`](spot_protected_keys::KeyStore) of the
//! protected-group key hierarchy (WP-SEC-10.T4, ADR-070 rule 12.13 e).
//!
//! `spot-protected-keys` holds its keys in memory, so a hub that restarts forgets what it
//! wrote. [`PgKeyStore`] keeps the **wrapped** forms: each group key at each epoch wrapped under
//! the tenant root key, each node's wrapping key wrapped under it too, the leases, the
//! revocation records and an append-only history (see [`schema`]). The plaintext key never
//! reaches this crate.
//!
//! # The sync trait and the bridge
//!
//! `KeyStore` is synchronous and PostgreSQL is not. The bridge is the shared, bounded
//! `spot-pg-bridge` (ADR-082 addendum 1): the caller's thread blocks on a channel while the
//! statement runs on the bridge's own runtime, with a deadline, a bound on work in flight and a
//! statement cancel on timeout. This crate writes no `block_on` of its own.
//!
//! **Fail closed, never deadlock.** A call from a thread that belongs to any tokio runtime is
//! refused with `protected_keys_pg.store_failed` naming `pg_bridge.wrong_thread`; nothing is
//! run. A host drives the key store from a plain thread of its control or job process.
//!
//! # What the store keeps, and the rules
//!
//! * **Only wrapped forms.** The key columns are `wrapped bytea` with a 40-byte minimum, so a
//!   bare 256-bit key is refused by the database as well as by the API.
//! * **Compare-and-set epochs.** The head of a group moves in one `UPDATE ... WHERE
//!   current_epoch = $from`; two hubs rotating one group to different next keys have exactly
//!   one winner, and the loser gets `protected_keys_pg.epoch_conflict` and writes nothing. The
//!   new key, the head move and the history event commit in one short transaction, and the new
//!   key row records the epoch it supersedes.
//! * **Revocation is atomic.** The node is marked revoked and its wrapped key erased, every
//!   group it held is rotated by compare-and-set, the revocation epochs are recorded and its
//!   leases dropped, in one transaction. A release is one statement that needs an active node,
//!   a live epoch and no revocation epoch at or before it, and it share-locks the node row, so
//!   a revocation and a release are strictly ordered: a release that loses is refused.
//! * **Immutable history.** `spot_key_event` refuses `UPDATE`, `DELETE` and `TRUNCATE` with
//!   triggers, and [`PgKeyStore::restrict_role`] leaves the application role `SELECT` and
//!   `INSERT` on it. A wrapped key can only be retired (erased), never edited or deleted.
//!
//! # Security
//!
//! * **Tenant isolation.** The tenant is a constructor argument; every statement filters on it
//!   as parameter `$1`, and no method takes a tenant. A cross-tenant id answers exactly like a
//!   missing one.
//! * **No injection.** All statements are fixed text in [`sql`] with bound parameters; the one
//!   role name is passed as data to a function that quotes it with `%I`.
//! * **Fail closed.** A backend failure is `Failed`, never an empty result; a row that
//!   disagrees with its own columns or cannot be decoded is `Corrupt`, not skipped.
//! * **Quiet errors.** An error names the store and the SQLSTATE class only; this crate logs
//!   nothing, and no type here prints key bytes.
//! * **Bounded.** Ids, actors, wrapped lengths, rotations per revocation and page sizes are
//!   checked before a statement runs.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server (`SPOT_TEST_PG_URL`); with `SPOT_REQUIRE_PG=1`
//! a missing server is a failure, so CI cannot go green by skipping.

#![forbid(unsafe_code)]

mod codec;
pub mod error;
pub mod schema;
pub mod sql;
mod store;

pub use error::{BAD_SETUP, MIGRATION_FAILED, SetupError};
pub use store::{PREPARE_DEADLINE, PgKeyStore, READ_DEADLINE, WRITE_DEADLINE};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-protected-keys-pg";
