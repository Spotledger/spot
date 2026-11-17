//! The edge's durable receiving state on PostgreSQL (WP-RTB-02.T2, ADR-062 sections 2 to 4,
//! ADR-064 sections 6 to 9, ADR-065 sections 1 and 4, ADR-082 class C).
//!
//! `spot-sync-node`'s intake works over traits whose only implementations were in memory. This
//! crate keeps what a node must not forget, per tenant and transactionally:
//!
//! | Type | Implements | Holds |
//! | --- | --- | --- |
//! | [`PgBundleApply`] | `BundleApply` | the **real apply**: all records of a bundle through `spot_replica_apply` in one transaction, with the applied ledger and an optional checkpoint in that same transaction |
//! | [`PgApplyRouter`] | `BundleApply` | routes the intake's per-tenant calls to each tenant's database |
//! | [`PgLedger`] | (reader) | the applied-record ledger `(tenant, source, name, version, position, applied_at)`; the applied prefix is **recomputed** from it |
//! | [`PgParkingStore`] | `ParkingStore` | parked bundles, per tenant, in arrival order, bounded by the intake's count and byte limits |
//! | [`PgRepairList`] | (list) | bundles that left parking unapplied, for the closure repair loop |
//! | [`PgCheckpoints`] | `CheckpointStore` | the applied journal prefix per source, forward only, refused when ahead of the ledger |
//! | [`PgNodeState`] | (state) | the hold set in its WP-SYN-08.T4 golden form and the confirmed freshness horizons |
//! | [`PgNodeCatalog`] | `NodeCatalog` | the running metadata generation, swapped atomically |
//!
//! # Where this store may be used (ADR-082 section 2)
//!
//! The traits are synchronous, so every call crosses the shared blocking bridge
//! ([`spot_pg_bridge::Bridge`], one per process). That is legitimate on the edge's control thread
//! (a blocking thread that runs the intake), never on a request handler or any task running on an
//! async runtime: such a call is refused (`pg_bridge.wrong_thread`) and building a store there is
//! refused with `sync_node_pg.unsupported_context`. Every call has a stated deadline; a timed-out
//! write may or may not have been applied, and every write here is idempotent or safe to inspect
//! and retry (a redelivered bundle is a replay of `(name, version)`).
//!
//! # The guarantees, and where each is proved
//!
//! * **G1, ownership is checked, not trusted:** the apply path asks the node's own ownership view
//!   before the first write of every record; a bundle with one non-owned record is wholly refused,
//!   rolled back and the record quarantined.
//! * **G3, all or nothing:** one transaction per bundle; a rejected record rolls it back.
//! * **G4, idempotent by `(name, version)`:** a replay is a no-op, a lower version is `Stale`; a
//!   500-bundle history replayed in a random order ends in the in-order state.
//! * **The checkpoint is never ahead:** it is written in the apply's own transaction, so a crash
//!   leaves both or neither; written alone it is refused above the ledger.
//! * **No event:** the apply path enqueues nothing; the wrapper refuses and counts any attempt.
//!
//! # Tenancy
//!
//! Every table's key starts with the tenant and **every statement** carries the tenant predicate:
//! [`STATEMENTS`] lists them and a test proves each one has it. A bundle for another tenant than
//! the apply's is refused before any I/O. No failure carries SQL, a record image or a row value.
//!
//! # Not here
//!
//! The intake itself keeps its prefix trackers, hold set and freshness tracker in memory; this
//! crate persists the state behind them and recomputes the prefix, but a `NodeIntake` cannot yet
//! be seeded from it (see the task report's open questions).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod apply;
mod bundle_tx;
mod catalog;
mod checkpoints;
mod db;
pub mod error;
mod ledger;
mod parking;
mod repair;
pub mod schema;
mod state;

pub use apply::{
    ApplyClock, ApplyEnv, ApplyLimits, CatalogSource, Failpoint, Failpoints, PgApplyRouter,
    PgBundleApply, StaticCatalog,
};
pub use catalog::{EntityInfo, Generation, PgNodeCatalog};
pub use checkpoints::{CHECKPOINT_DEADLINE, PgCheckpoints};
pub use db::{Db, PREPARE_DEADLINE};
pub use error::{FailureLog, PgNodeError};
pub use ledger::{LEDGER_DEADLINE, PgLedger};
pub use parking::{PARKING_DEADLINE, PgParkingStore};
pub use repair::{MAX_LIST, PgRepairList, REPAIR_DEADLINE, StoredRepair};
pub use state::{MAX_STATE_BYTES, PgNodeState, STATE_DEADLINE};

/// Every statement this crate runs on a node table, so a test can prove none lacks the tenant
/// predicate. DDL and the one statement that finds the tenants with parked bundles (for the
/// expiry sweep, which then visits each tenant under its own predicate) are not listed.
pub const STATEMENTS: &[&str] = &[
    ledger::HAS,
    ledger::HAS_TOMBSTONE,
    ledger::MAX_POSITION,
    ledger::PREFIX,
    ledger::COUNT,
    ledger::INSERT,
    ledger::MAX_PUT,
    checkpoints::LOAD,
    checkpoints::UPSERT,
    parking::PARK,
    parking::MISSING_LIST,
    parking::SET_MISSING,
    parking::REMOVE_ONE,
    parking::TAKE_OLDEST,
    parking::SIZE,
    parking::EXPIRE,
    repair::PUSH,
    repair::LIST,
    repair::REMOVE,
    repair::LEN,
    state::LOAD,
    state::SAVE,
];

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-node-pg";
