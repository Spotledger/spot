//! `spot-sync-freshness`: freshness, sync stamps and stale-document findings
//! (WP-SYN-06.T1; ADR-065 sections 4, 5 and 7).
//!
//! A pure library: no clock reads, no I/O. Every time is an argument.
//!
//! * [`FreshnessTracker`]: time since a node was last confirmed current per freshness class,
//!   from **monotonic** instants only, against [`FreshnessSettings`] (`max_staleness`, default
//!   48 hours, tunable).
//! * [`decide`]: what posting does on a stale node under a document type's [`StalePolicy`].
//! * [`SyncStamp`]: what every submitted `record` document carries, pinned by a golden test.
//! * [`hub_offset_estimate`]: the NTP-style hub clock offset from request/response timestamps.
//! * [`find_violations`] and [`list_retroactive`]: the hub's check of a stamp against the
//!   mandatory events that were in force.
//!
//! * [`clamp_offset_checked`] (and the saturating [`clamp_offset`]), [`find_violations_clamped`],
//!   [`stamp_audit`], [`reject_audit`]: the hub bounds a node's
//!   claimed offset by its own receipt time so a stamp cannot be back-dated past a mandatory event.
//!
//! Out of scope: the finding queue, the document engine's stamping, fiscal connectors.

mod clamp;
mod clock;
mod decide;
mod error;
mod stamp;
mod tracker;
mod violation;

pub use clamp::{
    AuditNote, Bounds, ClampReason, Clamped, DEFAULT_TOLERANCE_MS, MAX_AGE_LIMIT_MS, RejectNote,
    clamp_offset, clamp_offset_checked, reject_audit, stamp_audit,
};
pub use clock::{ClockSample, Estimate, hub_offset_estimate};
pub use decide::{PostDecision, StalePolicy, decide};
pub use error::FreshnessError;
pub use stamp::{StampInput, StampOverride, SyncStamp, WarningConfirmed, build_stamp};
pub use tracker::{DEFAULT_MAX_STALENESS_MS, FreshnessSettings, FreshnessTracker};
pub use violation::{
    Finding, FindingKind, HubPostedAt, MandatoryEvent, find_violations, find_violations_clamped,
    list_retroactive,
};
