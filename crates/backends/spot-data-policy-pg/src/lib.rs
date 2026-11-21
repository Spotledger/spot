//! The PostgreSQL ledger half of the retention store (WP-SEC-05.T2, ADR-071, ADR-082 class C).
//!
//! `spot-data-policy`'s [`RetentionStore`](spot_data_policy::store::RetentionStore) mixes two
//! things: the **ledger** (per-item outcomes, run checkpoints, deletion proofs) and the **live
//! records**. This crate implements the ledger over PostgreSQL, in the tenant's own schema, and
//! takes the records from a [`RecordSource`] (a document-store adapter, registered as a
//! follow-up). [`PgRetentionStore`] composes the two.
//!
//! # Where this store may be used (ADR-082 section 2)
//!
//! The trait is synchronous, so every call crosses the shared blocking bridge
//! ([`spot_pg_bridge::Bridge`], one per process). That is legitimate only off the request path:
//!
//! - use it from a control-plane process, a CLI command or a job worker, **never from a request
//!   handler**, a hook, a lifecycle step or any task running on an async runtime (the call is
//!   refused with `pg_bridge.wrong_thread`, and building the store there is refused with
//!   `data_policy_pg.unsupported_context`);
//! - never call it inside a transaction an async store holds: the ledger takes its own
//!   connection from the pool, so the two would not share a transaction and could deadlock the
//!   pool;
//! - every call has a stated deadline ([`ledger::POINT_DEADLINE`], [`ledger::SCAN_DEADLINE`],
//!   [`ledger::APPLY_DEADLINE`], [`ledger::PREPARE_DEADLINE`]); a timed-out write may or may not
//!   have been applied, and the item is simply retried (it is idempotent).
//!
//! ## Sizing
//!
//! A retention run applies items one at a time, so the ledger needs one connection per active
//! run. Give the bridge `max_in_flight` equal to the number of retention runs a process may have
//! at once (1 for a CLI or a single job worker) and a pool of at least
//! `max_in_flight + 2` (the bridge reserves one connection and one more stays free);
//! [`PgLedger::new`] refuses a bridge whose `max_in_flight` exceeds
//! `BridgeConfig::largest_in_flight(pool_size)`.
//!
//! # Atomicity and idempotence
//!
//! `apply_item` runs in one PostgreSQL transaction: insert the outcome row (primary key
//! `(run id, record id)`, `ON CONFLICT DO NOTHING`), perform the effect through the
//! [`RecordSource`] on a blocking thread, insert the deletion proof for a deletion, commit. The
//! same item twice, sequentially or concurrently, applies its effect once; a failing effect rolls
//! the ledger rows back. The one window the database cannot close is a crash after the source's
//! effect and before the commit: the retry then calls the source again, so a source must treat a
//! repeated effect as success (see [`RecordSource`]).
//!
//! # Security
//!
//! - Tenant scoping: a store is built for one [`TenantId`](spot_tenancy::TenantId); every
//!   statement runs with that tenant's schema as the only `search_path` entry, set
//!   transaction-locally from a name derived from the id.
//! - Auditability: the full outcome (rule, action, status, reason, attempt, instant, proof digest)
//!   is kept per item, and the deletion proof survives the record. Nothing here deletes or
//!   updates a proof; the audit record of a change is handed to the source with the effect.
//! - No values in errors: failures carry fixed text and a stable code; no SQL, connection string
//!   or row data.
//! - No floats, no wall clock (time is an argument), no `unsafe`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod codec;
pub mod error;
pub mod ledger;
pub mod schema;
pub mod source;
mod store;

pub use error::{PREPARE_FAILED, PgPolicyError, UNSUPPORTED_CONTEXT};
pub use ledger::PgLedger;
pub use source::RecordSource;
pub use store::PgRetentionStore;
