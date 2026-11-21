//! The model the access questions run on, built from the live permission model of the site
//! (WP-IAM-08.T13).
//!
//! WP-IAM-08.T9 hosts the four questions over `spot_iam_questions::AccessModel`, but the model it
//! built had no duty definitions, no history, no portal assignments and no shares, so every answer
//! was "nothing". This module is the real model. It is a helper module, not a service of its own:
//! the access-questions service asks a [`live::LiveModelSource`] for a model and answers from it.
//!
//! # What the model contains
//!
//! - **Roles**: every stored `role` document with its parents, kind and duty names, and one
//!   reference role for every role name an installed app's `permissions/grants.toml` names that has
//!   no stored record. The shipped grant entries are the **direct grants** of the role they name, so
//!   "System Manager may read `user`" is an answer whose reason is "granted directly by role System
//!   Manager from app platform" ([`assemble`]).
//! - **Duties**: the definitions the installed apps ship (`duties.toml`), by name. The engine of the
//!   site decides by role-named grants only and never reads a duty's rights, so the duties of the
//!   model keep their names and lose their rights; an answer never says "allowed" where the engine
//!   says "no" (note [`CODE_DUTIES_NOT_ENFORCED`]).
//! - **Assignments** of every user type, active or not, with source, validity and scope
//!   ([`directory`]), and the portal users, so Barrier 3 applies in every answer.
//! - **Changes** ([`events`]): a role, assignment or user document that changes leaves the database
//!   as an outbox event after commit; the model applies it by reading just the records it names,
//!   and the access-change journal gets one event per change (ids only). A missed event is
//!   repaired by the periodic full read.
//! - **History**: the permission-change events, read from the tenant's access-change journal
//!   ([`history`]); an event the reader cannot interpret is skipped and counted, never invented.
//!
//! # The one invariant
//!
//! For every (user, DocType, right) the model's answer equals the decision of the permission engine
//! built from the same grants and the same assignments. `tests/access_questions_model.rs` pins it
//! with a randomised comparison over a seeded matrix.
//!
//! # Failure
//!
//! A part that cannot be read never turns into an empty answer. The questions that need the
//! missing part refuse with a stable code ([`ModelError::code`]); the others keep answering, and
//! readiness says which part is degraded.

#[path = "access_questions_model/assemble.rs"]
pub mod assemble;
#[path = "access_questions_model/directory.rs"]
pub mod directory;
#[path = "access_questions_model/events.rs"]
pub mod events;
#[path = "access_questions_model/feed.rs"]
pub mod feed;
#[path = "access_questions_model/grants.rs"]
pub mod grants;
#[path = "access_questions_model/history.rs"]
pub mod history;
#[path = "access_questions_model/live.rs"]
pub mod live;

pub use assemble::{
    AssembleInput, DEFAULT_PACK, GrantInputs, ModelCore, RawAssignment, RawDirectory, RawRole,
    RawUser, assemble, field_facts_of, groups_of,
};

/// The reference roles, assignments or pins the assembly skipped because they were malformed.
pub const CODE_RECORDS_SKIPPED: &str = "access_questions_model.records_skipped";
/// A stored role names a duty no installed app defines: the duty exists in the model, empty.
pub const CODE_DUTIES_UNDEFINED: &str = "access_questions_model.duties_undefined";
/// A stored role names a duty whose shipped rights the site's engine does not enforce.
pub const CODE_DUTIES_NOT_ENFORCED: &str = "access_questions_model.duties_not_enforced";
/// The stored roles do not form a registry (a cycle, a role too deep): the questions refuse.
pub const CODE_REGISTRY_INVALID: &str = "access_questions_model.registry_invalid";
/// The stored directory could not be read: the questions refuse.
pub const CODE_DIRECTORY_UNAVAILABLE: &str = "access_questions_model.directory_unavailable";
/// The installed apps' grant, duty or SoD files could not be read or do not parse.
pub const CODE_GRANTS_UNAVAILABLE: &str = "access_questions_model.grants_unavailable";
/// The role snapshot the engine side publishes is missing: the questions refuse.
pub const CODE_SNAPSHOT_MISSING: &str = "access_questions_model.snapshot_missing";
/// The permission-change history could not be read: only the history question refuses.
pub const CODE_HISTORY_UNAVAILABLE: &str = "access_questions_model.history_unavailable";
/// The requested history window holds more entries than one question reads.
pub const CODE_HISTORY_TOO_LARGE: &str = "access_questions_model.history_too_large";
/// The history window is malformed (the end before the start).
pub const CODE_RANGE_INVALID: &str = "access_questions_model.range_invalid";
/// Journal entries the reader could not interpret were skipped (a count, never a content).
pub const CODE_HISTORY_SKIPPED: &str = "access_questions_model.history_skipped";
/// No role snapshot port exists on this site: the model is rebuilt on a timer alone.
pub const CODE_SNAPSHOT_UNVERSIONED: &str = "access_questions_model.snapshot_unversioned";

/// Why a model, or a part of it, cannot be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelError {
    /// The stored roles do not form a registry.
    RegistryInvalid,
    /// The stored directory could not be read.
    DirectoryUnavailable,
    /// The apps' grant, duty or rule files could not be read.
    GrantsUnavailable,
    /// The role snapshot is missing.
    SnapshotMissing,
    /// The history journal could not be read.
    HistoryUnavailable,
    /// The history window holds too many entries.
    HistoryTooLarge,
    /// The history window is malformed.
    RangeInvalid,
}

impl ModelError {
    /// The stable code, for readiness text, logs and the service's own answer.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::RegistryInvalid => CODE_REGISTRY_INVALID,
            Self::DirectoryUnavailable => CODE_DIRECTORY_UNAVAILABLE,
            Self::GrantsUnavailable => CODE_GRANTS_UNAVAILABLE,
            Self::SnapshotMissing => CODE_SNAPSHOT_MISSING,
            Self::HistoryUnavailable => CODE_HISTORY_UNAVAILABLE,
            Self::HistoryTooLarge => CODE_HISTORY_TOO_LARGE,
            Self::RangeInvalid => CODE_RANGE_INVALID,
        }
    }

    /// Whether only the history question is affected: the other three keep answering.
    #[must_use]
    pub fn history_only(self) -> bool {
        matches!(
            self,
            Self::HistoryUnavailable | Self::HistoryTooLarge | Self::RangeInvalid
        )
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ModelError {}

/// Something the assembly noticed and did not fail on: a code and how many records it concerns.
/// Never a record's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Note {
    /// The catalogued code.
    pub code: &'static str,
    /// How many records.
    pub count: u64,
}
