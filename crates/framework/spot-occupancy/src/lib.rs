//! Booking holds on range exclusion (WP-STO-14.T1, ADR-087 section 6).
//!
//! A hold is a document in the `occupancy` entity with a tentative state, not a new storage
//! primitive: one table, one exclusion constraint over `(unit, during)`, and a service on the
//! storage contract that owns the lifecycle around it.
//!
//! - [`declaration`]: the entity, declared through `spot-meta` ([`declaration::definition`]).
//! - [`Occupancy`]: claim (`hold`, `book_now`), `confirm`, `release`, `availability` and the
//!   expiry sweeper (`sweep_once`, `sweep_all`).
//! - [`events`]: the `occupancy_*` outbox events, captured in the business transaction.
//! - [`owner`]: one owning node per bookable resource (hub by default, or one named home edge).
//! - [`audit`]: the audit entry reported after every operation, refusals included.
//!
//! The database arbitrates: the exclusion constraint is the only thing that decides a double
//! booking, and every race case of ADR-087 section 6.3 has a test on real PostgreSQL
//! (`tests/races.rs`). The in-memory fake answers `Unsupported` for holds by capability.
//!
//! No transaction in this crate calls anything outside the database: events are captured, the
//! audit sink is called after the transaction ended, and the wall clock is never read (the one
//! injected [`spot_core::Clock`] supplies `expires_at` and `$now`).

pub mod audit;
pub mod declaration;
pub mod error;
pub mod events;
pub mod model;
pub mod owner;
mod service;

pub use audit::{Action, AuditEntry, AuditSink, MemoryAudit, NoAudit, Outcome};
pub use declaration::{Span, definition};
pub use error::OccupancyError;
pub use model::{Busy, Caller, ClaimRequest, Hold, Holder, Limits, Released, State, SweepReport};
pub use owner::{HomeEdges, HubOwnsAll, Owner, OwnerRoutes};
pub use service::Occupancy;
