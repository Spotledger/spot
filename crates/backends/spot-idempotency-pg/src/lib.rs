//! The PostgreSQL [`IdempotencyStore`](spot_http::api::IdempotencyStore) (WP-PLT-05.T7,
//! ADR-082 class R, ADR-055).
//!
//! A client that retries a `POST` after a timeout or a deploy must not run the command twice. The
//! in-memory store in `spot-http` is per process: a restart forgets every key and a second node
//! knows none of them. [`PgIdempotencyStore`] makes the reservation and the stored response
//! durable and shared. It keeps one table, `spot_idempotency_key`, created by
//! [`PgIdempotencyStore::prepare`] from the DDL in [`schema`], with the primary key
//! `(site, credential, key)`.
//!
//! Rules it keeps:
//!
//! - **Scope.** A key belongs to a site and a credential (the router's SHA-256 of the bearer
//!   token, never the token) and to nobody else: one caller can never read another's stored
//!   response. Every statement is bound parameters; the key's length and charset are checked
//!   before it reaches SQL and again by the table.
//! - **`reserve` is one claim statement.** `INSERT … ON CONFLICT (site, credential, key) DO UPDATE
//!   … WHERE (done AND expired) OR (pending AND lease ended) RETURNING 1`: a returned row is
//!   `Fresh` (absent, or the window ended, or a crashed node's lease ran out). No row means the
//!   key is live, and a second read answers by it: a different fingerprint is `Conflict`, a
//!   stored response is `Completed` (status, headers, body byte-exact), a running one is
//!   `Pending`. A refused claim never extends a lease or a window. Two nodes racing for one key
//!   see exactly one `Fresh`: the primary key decides inside the database.
//! - **A concurrent duplicate waits, then gets the stored response** (the contract's definition).
//!   `wait_pending` re-reads the row every 100 ms growing to 500 ms until it is no longer pending,
//!   or the wait budget (default 25 s) is spent and the router answers `503 server.unavailable`
//!   with `Retry-After`. The duplicate is never run a second time and never answered 409. Each
//!   poll holds a pooled connection only for the statement, not between polls, so a waiting
//!   duplicate costs one short indexed read every 100 to 500 ms (2 to 10 per second at first,
//!   settling at 2 per second) and no connection while it sleeps.
//! - **A pending reservation can never be replayed as a response.** `reserve` answers `Pending`
//!   for it and `Completed` only for a row in state `done` with its status, headers and body;
//!   the table's `spot_idempotency_key_state` check makes a half-written done row impossible.
//! - **Lease.** A pending row carries `lease_expires_at = now + lease`; [`PgIdempotencyConfig`]
//!   refuses a lease below 30 s, and the host passes more than its HTTP request timeout
//!   (WP-PLT-05.T9: the larger of twice the timeout and 120 s). A crashed node's key is free again
//!   after the lease, not after 24 h. A handler stuck past its lease can run alongside its
//!   takeover; the first `complete` wins (`UPDATE … WHERE state = 'pending' AND fingerprint = $fp`
//!   touches no row for the second), and the engine-level command id (WP-DOC-04.T8) is the
//!   backstop for the commands that reach the engine.
//! - **Retention is configuration, per caller class, recorded per row.** See [`RetentionPolicy`]:
//!   a default (24 h), a maximum (7 days), class overrides by name. `complete` takes a class name
//!   (the router passes `http_api` or `http_upload`), the row records its own `expires_at` at write
//!   time, so changing the policy never touches rows already written; `prune` deletes by
//!   `expires_at` only.
//! - **Fails closed, leaks nothing.** Any pool or database failure, a statement or pool wait that
//!   outlasts the operation timeout, is [`IdempotencyError::Unavailable`] with a fixed sentence; no
//!   driver text, SQLSTATE, host, pool name, key, fingerprint or body appears in an error or a log
//!   line (this crate logs nothing). The schema migration is the one operation allowed to report a
//!   SQLSTATE, under `idempotency_pg.migration_failed`.
//! - **No secret at rest.** The only credential-derived value is the router's hash. A body is
//!   written only for the routes the router lets persist (document lifecycle responses and the
//!   file-upload record) and only up to the body cap (default 256 KiB, never more than 1 MiB);
//!   a larger one is released, not stored. `headers` holds only `etag`, `location`,
//!   `content-type` and `retry-after`, whatever a caller hands in.
//! - **Time is an argument.** The store reads `clock.now_utc()` for every statement and never the
//!   system clock or `now()` in SQL. Clock skew between nodes is bounded by the lease and the
//!   retention window: a node whose clock runs fast frees a pending key early by its skew.
//! - **`prune` is bounded and the host calls it.** The store runs no timer of its own (ADR-082
//!   open question 4).
//!
//! Tenancy: like the other `*-pg` stores, the table lives in the site's own database and the pool
//! the caller passes is that site's; `site` is still a column and part of the key.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do; with `SPOT_REQUIRE_PG=1` a missing
//! server fails. They run the `spot-http` contract suite against the store, plus restart
//! persistence, crash recovery, two-node concurrency, retention, expiry and prune, and the real
//! router end to end.

#![warn(missing_docs)]

mod config;
mod error;
pub mod schema;
mod sql;
mod store;

pub use config::{
    ConfigError, DEFAULT_MAX_RETENTION, DEFAULT_OPERATION_TIMEOUT, DEFAULT_RETENTION,
    DEFAULT_WAIT_BUDGET, MAX_BODY_LIMIT, MIN_LEASE, PgIdempotencyConfig, RetentionPolicy,
};
pub use error::MIGRATION_FAILED;
pub use store::PgIdempotencyStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-idempotency-pg";
