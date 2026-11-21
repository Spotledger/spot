//! The quarantine queue for invalid received items (WP-EDG-06.T2, ADR-062, ADR-064, ADR-067).
//!
//! Sync is a consumer of the stream (ADR-062 section 2). The apply path ([`spot_replica_apply`],
//! WP-EDG-06.T1) stores a received record version or **refuses** it with a typed reason. This
//! crate is what happens to a refusal: the item is quarantined, and **the stream is not blocked**.
//! One bad document never stalls a branch.
//!
//! # What it is
//!
//! - [`QuarantineItem`]: the queue's metadata for a refused (or parked) record — its sender,
//!   stream sequence, apply key, rejection [`ReasonInfo`](reason::ReasonInfo), first-seen time,
//!   attempt count and [`State`]. The record image itself is held by the [`store`], never edited.
//! - [`QuarantineStore`](store::QuarantineStore): where items and their immutable payloads live;
//!   [`InMemoryStore`](store::InMemoryStore) is the reference. The store assigns item ids, so the
//!   queue keeps its order across a restart.
//! - [`Intake`]: the adapter around the apply path. On a refusal the item is stored and the
//!   checkpoint **advances past it**; later records of *other* documents keep flowing. A later
//!   version of a quarantined document is parked behind it ([`Dependents`]).
//! - Operations: [`Intake::retry`], [`Intake::retry_all`] (after a metadata upgrade, for
//!   example), [`Intake::release`] (re-apply) and [`Intake::discard`] (needs a permission and a
//!   reason; audited; never deletes the sender's data). The automatic schedule
//!   ([`Intake::run_due`], [`RetryPolicy`]) retries with backoff and a cap, after which the item
//!   stays `Held` and raises a [`Condition`].
//! - The admin API ([`admin`]): plain functions returning serialisable view structs, with every
//!   decision checked through `spot-permissions` ([`QuarantineGuard`](admin::QuarantineGuard)).
//!
//! # What it never does
//!
//! It never edits or deletes a quarantined item's payload, never re-runs business logic or emits
//! an event (the apply path's rules, unchanged), never reads the wall clock (time is an argument),
//! and never logs or returns a secret.
//!
//! # Policy is elsewhere
//!
//! The later sync ADRs (063 to 067) are still proposed, so every policy decision is behind a seam:
//! [`CatalogSource`] (an upgrade swaps the metadata snapshot, ADR-066), [`Dependents`] (what waits
//! behind what, ADR-064's closure), [`RetryPolicy`] (the backoff and cap, ADR-067), and
//! [`QuarantineGuard`](admin::QuarantineGuard) (who may do what, ADR-070). Where a later ADR will
//! matter is listed in the PR's open questions.

#![warn(missing_docs)]

pub mod admin;
pub mod audit;
pub mod codes;
pub mod error;
pub mod intake;
pub mod item;
pub mod reason;
pub mod retry;
pub mod store;

pub use admin::{
    Access, CloseAuthority, ConditionView, EngineGuard, ItemView, OperationResult, OperationView,
    QuarantineGuard, authorize_close,
};
#[cfg(any(test, feature = "testing"))]
pub use admin::{AllowAll, DenyAll};
pub use audit::{AuditAction, AuditEntry, ChainAudit, QuarantineAudit, RecordingAudit};
pub use error::QuarantineError;
pub use intake::{
    CatalogSource, Dependents, FixedCatalog, Intake, IntakeOutcome, SameDocument, SharedCatalog,
};
pub use item::{
    Closure, ClosureOutcome, ClosureRecord, ItemFilter, ItemId, MAX_DETAIL_BYTES,
    MAX_PAYLOAD_BYTES, MAX_REASON_CODE_BYTES, MAX_SOURCE_BYTES, NewItem, QuarantineItem, RecordRef,
    State,
};
pub use reason::ReasonInfo;
pub use retry::{Condition, RetryPolicy, Severity};
pub use store::{InMemoryStore, QuarantineStore, StoreFuture};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-quarantine";
