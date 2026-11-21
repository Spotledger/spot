//! The PostgreSQL [`NonceStore`](spot_signed_command::NonceStore) (WP-ADM-01.T3, ADR-082,
//! Recorded security finding).
//!
//! `spot-signed-command` keeps the replay window's contract and an in-memory store for one
//! process and for tests; a window that lives only in memory is emptied by a restart and never
//! seen by a second node, so a captured signed command replays inside its expiry. [`PgNonceStore`]
//! makes the window durable and shared.
//!
//! # The claim
//!
//! One table (`spot_` prefixed so the migrator treats it as the platform's own), `spot_signed_command_nonce (key_id bytea, nonce bytea, expires_at bigint, primary key
//! (key_id, nonce))`, created by [`PgNonceStore::prepare`] (idempotent, serialised across nodes).
//! A claim is **one statement**:
//!
//! ```text
//! INSERT ... ON CONFLICT (key_id, nonce)
//! DO UPDATE SET expires_at = EXCLUDED.expires_at WHERE spot_signed_command_nonce.expires_at < $4
//! RETURNING 1
//! ```
//!
//! A returned row means the pair is fresh: it was absent, or its window had run out
//! (`expires_at + skew < now`, the memory store's rule; `$4` is `now - skew`). A live pair
//! returns no row. Concurrent claims of one pair are serialised by the primary key: the loser
//! waits for the winner's commit, re-reads the now-live row and gets no row, so exactly one wins.
//!
//! # Fail closed
//!
//! Any failure (no connection, a failed statement, a timeout the pool reports) is
//! [`StoreError::Unavailable`](spot_signed_command::StoreError::Unavailable): the verifier then
//! refuses the command with `cmd.nonce_store`. It is never `true`, and the error carries no driver
//! text, SQLSTATE, key or nonce. Every value is a bound parameter; the SQL text is fixed.
//!
//! # No cap
//!
//! The memory store has a hard cap because memory is its limit. This store has none: it never
//! refuses with `StoreError::Full` and never evicts a live row. The row count is bounded by the
//! window instead: a nonce is accepted only for a command inside its validity (at most the
//! policy's maximum lifetime plus skew), so at any instant the live rows are the commands
//! accepted during that span, and [`PgNonceStore::prune`] deletes the expired ones. Time is an
//! argument; nothing here reads a clock or runs a timer.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do; with `SPOT_REQUIRE_PG=1` a missing
//! server fails them instead. CI's Postgres service sets it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod schema;
mod store;

pub use error::{MIGRATION_FAILED, SetupError};
pub use store::{PRUNE_BATCH, PgNonceStore};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-signed-command-pg";
