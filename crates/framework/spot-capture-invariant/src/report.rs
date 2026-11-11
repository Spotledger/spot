//! Findings and the report.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::error::CaptureInvariantError;
use crate::exclusions::ExclusionKind;
use crate::model::{Op, Position};

/// What kind of inconsistency a finding is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A feed entry with no `doc_state` event: a write that bypassed capture.
    MissingStateEvent,
    /// More than one `doc_state` event for the same document and version.
    DuplicateStateEvent,
    /// An event with no feed entry.
    OrphanStateEvent,
    /// Kept for the stable report form; no longer produced, because the checker keys a slot by
    /// operation too (WP-JRN-02.T4): an operation that differs on the same version is reported
    /// as `MissingStateEvent` plus `OrphanStateEvent`.
    OpMismatch,
    /// Feed and event name the same document with different versions.
    VersionMismatch,
    /// A derived entity flagged in the catalog with no registered projector.
    DerivedWithoutProjector,
    /// An entry found no counterpart inside the window: missing, or the window is too small.
    OutOfWindow,
}

/// Every kind, in report order.
pub const ALL_KINDS: [FindingKind; 7] = [
    FindingKind::MissingStateEvent,
    FindingKind::DuplicateStateEvent,
    FindingKind::OrphanStateEvent,
    FindingKind::OpMismatch,
    FindingKind::VersionMismatch,
    FindingKind::DerivedWithoutProjector,
    FindingKind::OutOfWindow,
];

/// A feed entry has no `doc_state` event.
pub const MISSING_STATE_EVENT: &str = "capture_invariant.missing_state_event";
/// A document version has more than one `doc_state` event.
pub const DUPLICATE_STATE_EVENT: &str = "capture_invariant.duplicate_state_event";
/// A `doc_state` event has no feed entry.
pub const ORPHAN_STATE_EVENT: &str = "capture_invariant.orphan_state_event";
/// Feed and event disagree on the operation.
pub const OP_MISMATCH: &str = "capture_invariant.op_mismatch";
/// Feed and event disagree on the version.
pub const VERSION_MISMATCH: &str = "capture_invariant.version_mismatch";
/// A derived entity has no registered projector.
pub const DERIVED_WITHOUT_PROJECTOR: &str = "capture_invariant.derived_without_projector";
/// An entry left the window unmatched.
pub const OUT_OF_WINDOW: &str = "capture_invariant.out_of_window";

impl FindingKind {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::MissingStateEvent => MISSING_STATE_EVENT,
            Self::DuplicateStateEvent => DUPLICATE_STATE_EVENT,
            Self::OrphanStateEvent => ORPHAN_STATE_EVENT,
            Self::OpMismatch => OP_MISMATCH,
            Self::VersionMismatch => VERSION_MISMATCH,
            Self::DerivedWithoutProjector => DERIVED_WITHOUT_PROJECTOR,
            Self::OutOfWindow => OUT_OF_WINDOW,
        }
    }
}

/// One inconsistency, naming entity, id, version and the positions on each side.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Finding {
    /// The kind.
    pub kind: FindingKind,
    /// The entity (for `DerivedWithoutProjector`, the derived entity; the id is empty).
    pub entity: String,
    /// The document id.
    pub id: String,
    /// The version the finding is about (the feed's, when both sides are present).
    pub version: u64,
    /// For `VersionMismatch`, the event's version.
    pub event_version: Option<u64>,
    /// The feed position, when a feed entry is involved.
    pub feed_position: Option<Position>,
    /// The event position (for a duplicate, the extra event).
    pub event_position: Option<Position>,
    /// For `DuplicateStateEvent`, the position of the first event.
    pub first_event_position: Option<Position>,
    /// The feed's operation, when known.
    pub feed_op: Option<Op>,
    /// The event's operation, when known.
    pub event_op: Option<Op>,
}

/// One exclusion as applied, with how many entries it skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AppliedExclusion {
    /// The excluded entity.
    pub entity: String,
    /// Derived or local-only.
    pub kind: ExclusionKind,
    /// Who excluded it.
    pub owner: String,
    /// Why.
    pub reason: String,
    /// The registered projector, for a derived entity.
    pub projector: Option<String>,
    /// Feed entries skipped.
    pub feed_entries_skipped: u64,
    /// Events skipped.
    pub events_skipped: u64,
}

/// The result of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The window the run used (tracked document versions).
    pub window: usize,
    /// Feed entries pushed, excluded ones included.
    pub feed_entries: u64,
    /// Events pushed, excluded ones included.
    pub state_events: u64,
    /// Feed entries paired with an event (agreeing or not).
    pub matched: u64,
    /// The most document versions tracked at once; never above `window`.
    pub peak_tracked: usize,
    /// The most unmatched entries held at once; never above `window`.
    pub peak_pending: usize,
    /// Findings per kind code; every kind is present, zero included.
    pub counts: BTreeMap<&'static str, u64>,
    /// The findings, sorted.
    pub findings: Vec<Finding>,
    /// Every declared exclusion, for the tenant's recovery report.
    pub exclusions: Vec<AppliedExclusion>,
}

impl Report {
    /// No finding of any kind.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }

    /// How many findings of a kind.
    #[must_use]
    pub fn count(&self, kind: FindingKind) -> u64 {
        self.counts.get(kind.code()).copied().unwrap_or(0)
    }

    /// The report as JSON. The field order is fixed and the maps are sorted, so equal reports
    /// give equal text.
    ///
    /// # Errors
    ///
    /// `capture_invariant.report_encoding` if encoding fails (it cannot for these types).
    pub fn to_json(&self) -> Result<String, CaptureInvariantError> {
        serde_json::to_string(self).map_err(|_| CaptureInvariantError::ReportEncoding)
    }
}
