//! The PostgreSQL partition registry and range allocator (WP-EDG-03.T8, ADR-082 class C).
//!
//! [`PgPartitionRegistry`] implements `spot_partition::PartitionRegistry` and
//! [`PgRangeAllocator`] implements `spot_partition::RangeAllocator` over the shared blocking
//! bridge ([`spot_pg_bridge::Bridge`], one per process). The in-memory implementations in
//! `spot-partition` are the contract; the test table runs the same cases against both.
//!
//! # Where this store may be used (ADR-082 section 2)
//!
//! The traits are synchronous, so every call crosses the bridge. That is legitimate only off the
//! request path:
//!
//! - use it from a control-plane process (the hub's lease granting), a CLI command or a job
//!   worker, **never from a request handler**, a hook, a lifecycle step or any task running on
//!   an async runtime (the call is refused with `pg_bridge.wrong_thread`, and building the store
//!   there is refused with `partition_pg.unsupported_context`);
//! - never call it inside a transaction an async store holds: it takes its own connection from
//!   the pool, so the two would not share a transaction and could deadlock the pool;
//! - every call has a stated deadline ([`registry::ASSIGN_DEADLINE`], [`registry::MAP_DEADLINE`],
//!   [`registry::HISTORY_DEADLINE`], [`allocator::LEASE_DEADLINE`],
//!   [`allocator::GRANTS_DEADLINE`], [`PREPARE_DEADLINE`]). A timed-out write may or may not have
//!   been applied; both writes are safe to inspect and retry (an assignment that did commit
//!   makes the retry an `ownership_conflict` only for a different node; a grant that did commit
//!   is in `grants()`).
//!
//! ## Sizing
//!
//! Grants and assignments are rare, short control-plane writes. Give the bridge `max_in_flight`
//! of 2 to 4 (the hub grants a range to one branch at a time and an administrator assigns one
//! scope at a time) and a pool of at least `max_in_flight + 2` (the bridge reserves one
//! connection and one more stays free); the constructors refuse a bridge whose `max_in_flight`
//! exceeds `BridgeConfig::largest_in_flight(pool_size)`.
//!
//! # The allocator is the part to review hardest
//!
//! A reissued range duplicates document numbers. The grant locks the series row, advances the
//! cursor and inserts the grant row in one transaction (see [`allocator`]). The cursor is a
//! `numeric(39,0)` because the in-memory allocator counts in `u128`.
//!
//! # Reads cannot fail in the trait
//!
//! `PartitionRegistry::map_at`, `PartitionRegistry::history` and `RangeAllocator::grants` return
//! no error. A failed read returns an empty answer (an empty map denies every write, so the
//! failure is fail-closed) and is recorded for [`PgPartitionRegistry::take_read_failure`]; the
//! fallible `try_map_at`, `try_history` and `try_grants` return the error itself. A database
//! that is down is never presented as a map with owners.
//!
//! # Security
//!
//! - No values in errors: failures carry fixed text and a stable code; no SQL, connection string
//!   or row data.
//! - Instants are arguments (no wall clock); they are stored as exact UTC nanoseconds, so an
//!   instant read back is the same moment, in UTC.
//! - The history and grant tables are append-only (a trigger refuses UPDATE, DELETE, TRUNCATE).
//! - No floats, no `unsafe`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod allocator;
mod codec;
pub mod error;
pub mod registry;
pub mod schema;

use std::sync::Arc;
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig};

pub use allocator::PgRangeAllocator;
pub use error::{PREPARE_FAILED, PgPartitionError, UNSUPPORTED_CONTEXT};
pub use registry::PgPartitionRegistry;

/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The checks a constructor makes: not inside an async runtime, and a bridge that fits the pool.
pub(crate) fn check_context(bridge: &Bridge, pool_size: usize) -> Result<(), PgPartitionError> {
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(PgPartitionError::UnsupportedContext {
            reason: "the store is built inside an async runtime; build it on a plain thread",
        });
    }
    if bridge.config().max_in_flight > BridgeConfig::largest_in_flight(pool_size) {
        return Err(PgPartitionError::UnsupportedContext {
            reason: "the bridge allows more calls in flight than the pool can serve",
        });
    }
    Ok(())
}

/// Creates every table (idempotent).
pub(crate) fn prepare_all(bridge: &Arc<Bridge>) -> Result<(), PgPartitionError> {
    bridge
        .run(PREPARE_DEADLINE, |mut conn| {
            Box::pin(async move { schema::prepare(&mut conn).await })
        })
        .map_err(|_| PgPartitionError::PrepareFailed)
}
