//! The journal capture invariant (WP-JRN-02.T1, ADR-061 s3 and s8): *every entry in the change
//! feed has exactly one `doc_state` event of the same document and version.*
//!
//! The storage change feed is storage's own account of what changed, so it is an independent
//! witness against a missed capture. [`Checker`] compares two position-ordered streams of plain
//! summaries ([`FeedEntry`], [`StateEvent`]) in a single pass with bounded memory; reading the
//! real feed and journal is the document engine's job. Time and randomness are never read.
//!
//! # The window
//!
//! The streams are not in lockstep, so the checker tracks up to `window` document versions
//! (unmatched entries and recently matched ones, which is how a late duplicate is still caught).
//! The default is [`DEFAULT_WINDOW`]. When a new version arrives and the window is full, the
//! oldest tracked version is dropped; if it was unmatched it becomes an `OutOfWindow` finding,
//! never a silent drop. At [`Checker::finish`] the streams are over, so what is still unmatched
//! is definitive: `MissingStateEvent`, `OrphanStateEvent` or `VersionMismatch`. A continuous run
//! therefore sees a bypassed write as `OutOfWindow` once the window passes it; a window larger
//! than the longest commit-to-journal lag makes every `OutOfWindow` a real miss.
//!
//! # Exclusions
//!
//! Derived and local-only entities ([`Exclusions`]) are skipped on both sides and listed in the
//! report with their owner, reason and counts. A document created and deleted in one transaction
//! produces neither a feed entry nor an event (ADR-053, ADR-061), so it is simply absent.
//!
//! No `unsafe`, no float, no clock, no I/O.

mod checker;
mod error;
mod exclusions;
mod model;
mod report;

pub use checker::{Checker, DEFAULT_WINDOW};
pub use error::{
    CaptureInvariantError, EXCLUSION_INVALID, FEED_DUPLICATE, REPORT_ENCODING, STREAM_OUT_OF_ORDER,
    WINDOW_INVALID,
};
pub use exclusions::{Exclusion, ExclusionKind, Exclusions};
pub use model::{FeedEntry, Op, Position, StateEvent};
pub use report::{
    ALL_KINDS, AppliedExclusion, DERIVED_WITHOUT_PROJECTOR, DUPLICATE_STATE_EVENT, Finding,
    FindingKind, MISSING_STATE_EVENT, OP_MISMATCH, ORPHAN_STATE_EVENT, OUT_OF_WINDOW, Report,
    VERSION_MISMATCH,
};
