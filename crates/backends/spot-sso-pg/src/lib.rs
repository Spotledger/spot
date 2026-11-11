//! The PostgreSQL [`ReplayStore`](spot_sso::ReplayStore) (WP-IAM-05.T6).
//!
//! `spot-sso` verifies every signed message and then claims its single-use identifier — a SAML
//! assertion `ID`, an OIDC back-channel logout `jti` — through a [`ReplayStore`]. The in-memory
//! store in that crate is per process: on a second node the same assertion would be accepted
//! again, and a restart would forget every id. [`PgReplayStore`] makes the claim durable and
//! shared. It keeps one table, created by [`PgReplayStore::prepare`] from the DDL in [`schema`]:
//!
//! | Column | Holds |
//! | --- | --- |
//! | `namespace` | the store's namespace — one deployment (or tenant) per value, so one table serves several |
//! | `id` | the namespaced single-use id, exactly as [`namespaced`](spot_sso::namespaced) builds it |
//! | `expires_at` | when the id becomes claimable again (Unix seconds, the caller's `expires_at`) |
//!
//! The primary key is `(namespace, id)` and every statement is bound parameters, so two different
//! keys can never collide and no SQL is built from data.
//!
//! Rules it keeps:
//!
//! - **`claim` is one statement.** `INSERT … ON CONFLICT (namespace, id) DO UPDATE … WHERE
//!   spot_sso_replay.expires_at <= $now RETURNING 1`: a row comes back exactly when the id was absent
//!   or had expired at `now`, and that one row means [`Claim::Fresh`]. A live id matches the
//!   `WHERE` not at all, so nothing is returned ([`Claim::Replayed`]) and — importantly — the
//!   refused replay does not push its expiry out. Two nodes racing for one id therefore see
//!   exactly one fresh claim: the conflict is resolved by the primary key inside the database,
//!   not by anything this process decides.
//! - **Fails closed.** Any pool or database failure is [`ReplayError::unavailable`] with a fixed
//!   sentence; no driver message, SQLSTATE or key value is carried or logged. The caller refuses
//!   the message, so an outage can turn a sign-out away but can never let a replay through.
//! - **Time is an argument.** The store never reads a clock: `now` and `expires_at` come from the
//!   caller, so a test states the instant and a node's clock skew stays visible at the caller.
//! - **[`PgReplayStore::prune`] is bounded.** It deletes at most `limit` expired rows per call,
//!   served by the `expires_at` index ([`schema`] lists it), so a cleanup job never takes a long
//!   lock. Pruning only removes rows the contract already treats as free, so it cannot lose a
//!   claim; a pruned id is simply claimable again.
//!
//! Tenancy: like the other `*-pg` stores, the table lives in the tenant's own database and the
//! pool the caller passes is that tenant's. A deployment that shares one database between tenants
//! scopes the rows by giving each tenant its own namespace.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do; with `SPOT_REQUIRE_PG=1` a missing
//! server fails. They run the `spot-sso` replay contract suite against the store, plus restart
//! persistence, a two-pool race and the prune batches.

#![warn(missing_docs)]

mod error;
pub mod schema;
mod sql;
mod store;

pub use error::MIGRATION_FAILED;
pub use store::PgReplayStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sso-pg";
