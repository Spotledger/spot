//! The document activity model (WP-DOC-07.T1): what a document's side panel shows, as data and
//! pure functions.
//!
//! Everything here is a function of its inputs; nothing persists, nothing reads a clock, no
//! float, no `unsafe` (the project coding rules, ADR-12). The engine (WP-DOC-04's storage) persists these
//! records as ordinary documents and calls the builders when it renders or stores a history;
//! the UI renders the entries and localises their keys.
//!
//! - [`version::Version`] and [`diff::diff_documents`]: one save of a document and the whole
//!   field-level change history, built from before/after images (the journal, ADR-061, already
//!   records state changes — this is the user-facing view of them, not a second audit).
//! - [`timeline::Timeline::build`]: every record on one deterministic, time-ordered list of
//!   localisable entries.
//! - [`comment::Comment`] and [`comment::parse_mentions`]: comments with a backtracking-free
//!   mention scan and rules for who may edit until when.
//! - [`assignment::Assignment`]: the assignment state machine (`Open → Done | Cancelled`),
//!   one open assignment per (document, assignee).
//! - [`share::Share`] and [`share::effective_share_rights`]: grants and the rights a user
//!   effectively holds.
//! - [`error::ActivityError`]: the one error type, with stable `activity.*` codes.
//!
//! Every public type carries serde with stable names, in the ADR-055 value encoding (values as
//! the native API writes them); the engine can store a record as JSON and read it back
//! unchanged.
//!
//! # Not here
//!
//! Persistence, permissions (WP-PLT-03 owns the checks), lifecycle operations (WP-DOC-04),
//! the journal (ADR-061).

pub mod assignment;
pub mod comment;
pub mod diff;
pub mod error;
pub mod share;
pub mod timeline;
pub mod version;
pub mod wire;

pub use assignment::{Assignment, AssignmentAction, AssignmentStatus};
pub use comment::{Comment, EditCheck};
pub use diff::{FieldChange, REDACTED};
pub use error::ActivityError;
pub use share::{Share, ShareRight, ShareRights};
pub use timeline::{CrateEvent, EntryKind, Timeline, TimelineEntry};
pub use version::Version;

/// The crate's re-exports from the contracts, so callers never touch lower layers for the
/// types these records use (the same pattern `spot-document` follows).
pub mod prelude {
    pub use spot_app_api::Actor;
    pub use spot_app_api::prelude::{
        CommandId, Name, OffsetDateTime, RecordId, Value, Version as DocVersion,
    };
}
