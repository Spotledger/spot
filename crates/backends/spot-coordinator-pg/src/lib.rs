//! The PostgreSQL [`Coordinator`](spot_coordinator::Coordinator) backend (WP-PLT-18.T2, ADR-20).
//!
//! A multi-node cluster with no Redis coordinates through its own database: fenced leases,
//! cluster-wide counters, rate limits and node presence all live in rows of a small set of
//! tables this crate owns ([`PgCoordinator::prepare`]). The in-process backend serves one node;
//! this one serves every node that shares the database.
//!
//! # Time is the database's
//!
//! Lease expiry is decided by the **database clock** (`now()`), never a node's, so a node with a
//! skewed clock cannot create two live leases for one name (ADR-053, ADR-46). A returned
//! [`Lease::expires_at`](spot_coordinator::Lease) is the database's instant, so a node should
//! treat it as a deadline, not compare it to its own clock.
//!
//! # Connections
//!
//! [`PgCoordinator::new`] takes a pool the caller built (and a [`PgCoordinatorConfig`]). Every
//! coordinator call takes its own pooled connection and runs a single auto-committed statement:
//! it never joins, and can never be rolled back with, a caller's business transaction. A store
//! layer that must fence a write inside its own transaction uses [`assert_fence`] (or
//! [`fence_sql`]) on that transaction's client instead.
//!
//! # Tables
//!
//! | Table | Holds |
//! | --- | --- |
//! | `spot_coord_lease` | the live lease per name: holder, token, `expires_at`, `lease_id` |
//! | `spot_coord_token` | the last token issued per name, so tokens survive a lease row's deletion |
//! | `spot_coord_counter` | the last value issued per counter (`numeric(20,0)`: the contract's counters are `u64`) |
//! | `spot_coord_rate` | one bucket per key: its theoretical arrival time and the capacity it was built for |
//! | `spot_coord_node` | one heartbeat row per running node |
//!
//! # The database clock and the contract suite
//!
//! `spot-coordinator`'s `contract::run_all` drives time through a `ManualClock` the backend is
//! built on. This backend reads the *database* clock, by design, so it cannot run that suite
//! unchanged: `tests/contract.rs` runs the same cases against a real server, advancing time by
//! waiting short ttls instead. That gap is recorded under `## Open questions` in the task report.

mod cleanup;
mod coordinator;
mod counters;
mod error;
mod leases;
mod presence;
mod rate;
mod schema;

pub use cleanup::{CleanupReport, cleanup};
pub use coordinator::{PgCoordinator, PgCoordinatorConfig};
pub use error::UNAVAILABLE;
pub use leases::{FENCE_SQL, FenceCheck, assert_fence, fence_sql};
pub use presence::PgPresence;
pub use schema::DDL;

/// Every table this backend owns, in creation order.
pub const TABLES: [&str; 5] = [
    "spot_coord_lease",
    "spot_coord_token",
    "spot_coord_counter",
    "spot_coord_rate",
    "spot_coord_node",
];

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-coordinator-pg";
