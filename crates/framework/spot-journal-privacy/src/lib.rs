//! Personal-field protection and erasure, hosted (WP-RTB-01.T8, ADR-061 sections 3 and 6).
//!
//! A field flagged `personal` (with a purpose and a retention class) is encrypted in the journal
//! under a **per-subject key**; erasing a subject destroys the key; replays and snapshots then
//! yield a documented placeholder for those fields. The mechanism lives in `spot-journal-pii`,
//! `-pii-secrets`, `-pii-secrets-pg` and `-protect`; this crate is what a host needs on top:
//!
//! - [`FieldDecl`] and [`CatalogueFields`]: which fields are personal, with the subject that
//!   identifies the data subject. A personal field without a subject is refused
//!   (`journal_privacy.subject_missing`): it could not be sealed, so it would reach the journal
//!   in clear. The set can be replaced at runtime.
//! - [`Privacy`]: the transform, snapshot parts and keys over one declared set; [`RefusingProtector`]
//!   for a site that declares fields but cannot protect them (fail closed).
//! - [`Eraser`]: `erase_subject(subject, requester)` with the permission check
//!   ([`ERASE_PERMISSION`]), retention classes that forbid erasing (the field stays readable and
//!   the outcome says so), an audit record that carries a salted hash, and no second key ever.
//!
//! The documented placeholder in live reads is the data-policy engine's; this crate proves the
//! journal, snapshot and rebuild side.
#![forbid(unsafe_code)]

mod decl;
mod erase;
mod error;
mod protect;
mod source;

pub use decl::{Built, Declared, FieldDecl, build};
pub use erase::{
    AdminAction, AdminAudit, ERASE_PERMISSION, ErasePermission, Eraser, ErasureOutcome, Retained,
    SubjectHasher,
};
pub use error::PrivacyError;
pub use protect::{Privacy, RefusingProtector};
pub use source::CatalogueFields;
