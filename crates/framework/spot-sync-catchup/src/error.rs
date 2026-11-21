//! The `sync_catchup.*` error vocabulary (WP-SYN-07.T1).
//!
//! Every code here has one entry in `crates/framework/spot-errors/catalogue/sync_catchup.toml`.
//! Callers match on [`CatchUpError::code`], never on a message. Messages name topics, entities and
//! ids; they never carry a payload, a secret or a value read from a record image.

use thiserror::Error;

/// A setting cannot work: a limit of zero, or a share above the whole.
pub const INVALID_SETTINGS: &str = "sync_catchup.invalid_settings";
/// The gap summary contradicts itself (its positions are not a range, or its class counts exceed
/// its event count).
pub const INCONSISTENT_SUMMARY: &str = "sync_catchup.inconsistent_summary";
/// A topic has no registered class, so the planner cannot tell how to coalesce its events.
pub const UNKNOWN_TOPIC: &str = "sync_catchup.unknown_topic";
/// A state event carries no subject, so it cannot be an image of a record.
pub const SUBJECT_MISSING: &str = "sync_catchup.subject_missing";
/// The image set holds the same record twice; the caller should have coalesced it first.
pub const DUPLICATE_IMAGE: &str = "sync_catchup.duplicate_image";
/// The catalog could not read a record image's references.
pub const REFERENCE_UNREADABLE: &str = "sync_catchup.reference_unreadable";
/// The gap holds an audit event that is not addressed to the node being served (it was not scoped).
pub const UNSCOPED_AUDIT: &str = "sync_catchup.unscoped_audit";

/// Why the planner refused to produce a plan.
///
/// It is a library vocabulary: the sync runtime maps these onto its own conditions and none of
/// them rides an `AppError`, which is why every catalogue entry uses `class = "none"`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CatchUpError {
    /// A tuning value cannot work.
    #[error("The catch-up settings are not usable: {reason}.")]
    InvalidSettings {
        /// Which rule the settings break.
        reason: &'static str,
    },
    /// The gap summary contradicts itself.
    #[error("The gap summary is not consistent: {reason}.")]
    InconsistentSummary {
        /// Which rule the summary breaks.
        reason: &'static str,
    },
    /// A topic has no registered class.
    #[error("The topic `{topic}` has no registered event class, so it cannot be planned.")]
    UnknownTopic {
        /// The topic name.
        topic: String,
    },
    /// A state event carries no subject.
    #[error("The state event at position {position} on topic `{topic}` names no record.")]
    SubjectMissing {
        /// The topic the event is on.
        topic: String,
        /// The event's position in the source stream.
        position: u64,
    },
    /// The image set holds one record twice.
    #[error("The image set holds the {entity} record {id} twice.")]
    DuplicateImage {
        /// The record's entity.
        entity: String,
        /// The record's id.
        id: String,
    },
    /// The gap holds an audit event addressed to another node.
    #[error(
        "The gap holds an audit event at position {position} that is not addressed to this node."
    )]
    UnscopedAudit {
        /// The event's position in the source stream.
        position: u64,
    },
    /// A record image's references could not be read.
    #[error("The references of the {entity} record {id} cannot be read from the catalog.")]
    ReferenceUnreadable {
        /// The record's entity.
        entity: String,
        /// The record's id.
        id: String,
    },
}

impl CatchUpError {
    /// The stable dotted catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSettings { .. } => INVALID_SETTINGS,
            Self::InconsistentSummary { .. } => INCONSISTENT_SUMMARY,
            Self::UnknownTopic { .. } => UNKNOWN_TOPIC,
            Self::SubjectMissing { .. } => SUBJECT_MISSING,
            Self::DuplicateImage { .. } => DUPLICATE_IMAGE,
            Self::ReferenceUnreadable { .. } => REFERENCE_UNREADABLE,
            Self::UnscopedAudit { .. } => UNSCOPED_AUDIT,
        }
    }
}
