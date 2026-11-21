//! `spot-sync-catchup` — the catch-up planner (ADR-066 sections 1 to 3; WP-SYN-07.T1).
//!
//! A node that returns after a gap must end up holding the right data without anybody hand-copying
//! files. The hub decides **what** to send and **in which order**; this crate makes that decision
//! over summaries, and reads and sends nothing. It is a pure library: no clock, no randomness, no
//! network, no file, no database, no async runtime. Every number, position and identity is an
//! argument, and every seam (the topic registry, the reference catalog) is a trait.
//!
//! It provides the five pieces ADR-066 section 2 asks for:
//!
//! * [`choose_mode`] — the three modes of the table, chosen by cost, with [`ModeReason`]s returned
//!   as data. A new, rebuilt or `ResyncRequired` node is snapshotted; a small gap still inside hot
//!   retention is replayed; a larger or older gap is coalesced; and when the coalesced plan would
//!   cost more than the configured share of a full scoped snapshot, the snapshot wins. There is no
//!   offline horizon: a node gone for six months is a cost, never a refusal.
//! * [`coalesce`] — only **state** events collapse, to the latest image per record. Domain events
//!   are kept in full unless the topic declares `coalesce: latest_per_subject`. Audit events are
//!   never coalesced; which ones a node gets is the [`AuditDelivery`] option.
//! * [`order_by_dependency`] — coalesced images in dependency order, targets before referrers, a
//!   cycle in one group, so no record is ever applied before a record it references
//!   (ADR-064 section 8).
//! * [`plan`] — the mandatory lane first, individually and in order, never coalesced away, then the
//!   bulk as [`BundleSpec`]s. It takes a [`ScopedGap`], which only [`scope_for_node`] can build:
//!   scope is applied once, before planning, so an audit event addressed to another node never
//!   reaches the incremental bundles or the mandatory lane. `plan` re-checks the gap in every mode
//!   and refuses with [`CatchUpError::UnscopedAudit`] (fail closed); [`coalesce`] keeps its own
//!   filter as the second line.
//! * The `sync_catchup.*` error vocabulary in [`CatchUpError`], catalogued in
//!   `crates/framework/spot-errors/catalogue/sync_catchup.toml`.
//!
//! # Change log: audit delivery (WP-SYN-07.T6)
//!
//! `plan_with_audit` used to deliver an audit event iff it was in scope and not addressed to
//! another node. Its default is now the recommendation of
//! `docs/research/sync-audit-event-delivery.md`: unaddressed audit events are **dropped** (ADR-066
//! section 2, never a broadcast) and events addressed to this node are delivered **whatever their
//! subject** (a `sys_sync_missing` names records the node does not hold yet). Both are an explicit
//! [`AuditDelivery`] given to [`plan_with_audit_options`]; [`AuditDelivery::LEGACY`] is the old
//! rule, so either answer of the architecture owner is a value, not a rewrite.
//!
//! # What this crate does not decide
//!
//! It does not read the journal, produce a snapshot, run the session protocol or serve from the
//! outbox (ADR-066 sections 1 and 3 are other tasks). It also does not read a payload: a payload is
//! the source's canonical JSON **text**, opaque here, so the crate needs no JSON dependency; the
//! reference catalog that does read images is supplied by the host. Where the ADR leaves a value
//! open, it is a [`Settings`] field with the ADR's starting value as its default, never a constant
//! in this code.

#![warn(missing_docs)]

mod audit;
mod coalesce;
mod delivery;
mod error;
mod event;
mod mode;
mod order;
mod plan;
mod scope;
mod settings;
mod summary;
mod topics;

pub use audit::{
    AuditCounts, AuditedPlan, ScopeOracle, VisibleAuditCount, plan_with_audit,
    plan_with_audit_options,
};
pub use coalesce::{Coalesced, coalesce};
pub use delivery::{Addressed, AuditDelivery, Unaddressed};
pub use error::{
    CatchUpError, DUPLICATE_IMAGE, INCONSISTENT_SUMMARY, INVALID_SETTINGS, REFERENCE_UNREADABLE,
    SUBJECT_MISSING, UNKNOWN_TOPIC, UNSCOPED_AUDIT,
};
pub use event::{Event, EventClass, Image, Payload, Position, RecordId};
pub use mode::{Mode, ModeDecision, ModeReason, choose_mode};
pub use order::{References, order_by_dependency};
pub use plan::{BundleKind, BundleSpec, CatchUpPlan, plan};
pub use scope::{ScopedGap, scope_for_node};
pub use settings::{Settings, Share};
pub use summary::{ClassCounts, GapSummary};
pub use topics::{Coalescing, StaticTopics, TopicDeclarations};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-catchup";
