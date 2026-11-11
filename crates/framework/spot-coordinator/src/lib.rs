//! The Coordinator: everything the platform needs that may not lose data (WP-PLT-18, ADR-20).
//!
//! Caching may lose data; coordination may not. Who runs the schedule, who owns a job, how many
//! requests a tenant may make, the next value of a cluster-wide counter: all go through one
//! [`Coordinator`]:
//!
//! - **Leases with fencing tokens** ([`Coordinator::acquire`], `renew`, `release`, `current`,
//!   `check_fence`): tokens are strictly increasing per name for the life of the store and never
//!   reused, the same semantics as `spot-scheduler`'s `LeaseStore`, which this generalises.
//! - **Locks** ([`LockExt::lock`]): a lease with a short ttl and an RAII [`LockGuard`]; a leaked
//!   guard or a crashed node blocks nobody longer than the ttl.
//! - **Counters** ([`Coordinator::next`]): atomic and monotonic, *not* gapless across a crash
//!   (the storage layer's `Sequences` are the gapless ones).
//! - **Rate limits** ([`Coordinator::take`]): never exceeded, however many callers race.
//!
//! [`InProcessCoordinator`] serves one node and refuses to start beside a second. The database
//! backend (WP-PLT-18.T2) and the Redis plugin (WP-PLT-19) serve clusters; all backends pass
//! the same [`contract`] suite (feature `testing`).
//!
//! Not here, by decision: a cluster-wide circuit breaker (ADR-072).

#[cfg(feature = "testing")]
pub mod contract;
pub mod coordinator;
pub mod error;
pub mod in_process;
pub mod presence;
pub mod types;

pub use coordinator::{
    BoxFuture, Coordinator, DEFAULT_LOCK_TTL, LOCK_POLL_INTERVAL, LockExt, LockGuard,
};
pub use error::{Error, Result};
pub use in_process::{InProcessCoordinator, PRESENCE_CHECK_INTERVAL};
pub use presence::{InMemoryPresence, NodePresence, Registration};
pub use types::{
    Admit, FencingToken, Lease, LeaseId, MAX_NAME_BYTES, MAX_TTL, NodeId, RateLimit, validate_name,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-coordinator";
