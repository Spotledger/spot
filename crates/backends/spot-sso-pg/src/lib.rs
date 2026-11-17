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

//! # The pending-authorisation and provider stores (WP-IAM-05.T9)
//!
//! [`PgPendingStore`] keeps the sign-in flows in progress (`spot_sso_pending`): the browser
//! binding as a keyed digest, the record sealed ([`Sealer`], [`AesGcmSealer`]), the tenant in
//! every predicate, `take` one atomic `DELETE … RETURNING`. [`PgProviderStore`] keeps the
//! tenant's provider configurations (`spot_sso_provider`), the client secret only as the name of a
//! `spot-secrets` entry. Each has its own idempotent `prepare` and its own DDL constant
//! ([`PENDING_DDL`], [`PROVIDER_DDL`]); neither touches the replay table.
//!
//! Their tests run the `spot-sso` contract suites ([`spot_sso::contract_stores`]).
//!
//! # The host's sign-in tables (WP-IAM-05.T18)
//!
//! The SQL the host used to carry moved here (the G5 lint allows no SQL and no database driver
//! outside the storage crates): [`PgSamlRows`] (`spot_sso_saml_provider`, versioned, the
//! configuration as opaque text), [`PgPolicyStore`] (`spot_sso_policy`, one row per tenant,
//! compare-and-set), [`PgHandoverTables`] (`spot_sso_handover`, redeemed by one
//! `DELETE ... RETURNING`, and `spot_sso_session_index`). Each names the tenant in every statement and
//! has its own idempotent `prepare`. The host keeps only the adaptation to its ports.

#![warn(missing_docs)]

mod error;
mod handover;
mod pending;
mod policy;
mod provider;
mod saml;
pub mod schema;
mod seal;
mod sql;
mod store;

pub use deadpool_postgres::Pool;
pub use error::MIGRATION_FAILED;
pub use handover::{HANDOVER_DDL, HandoverUnavailable, PgHandoverTables};
pub use pending::{PENDING_DDL, PgPendingStore};
pub use policy::{POLICY_DDL, PgPolicyStore, PolicyStoreError};
pub use provider::{PROVIDER_DDL, PgProviderStore};
pub use saml::{MAX_SAML_CONFIG, PgSamlRows, SAML_DDL, SamlRow, SamlRowError};
pub use seal::{AesGcmSealer, KEY_LEN, NonceSource, Sealer};
pub use store::PgReplayStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sso-pg";
