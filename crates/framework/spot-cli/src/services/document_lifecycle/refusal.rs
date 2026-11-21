//! The fixed refusal texts of the document lifecycle, and how an engine error becomes one. No
//! text carries a value, a field's content, a message from the engine or a document name: the
//! engine's own error text may name data, so only its class is read.

use spot_app_api::{AppError, ErrorClass};

/// No document engine is attached to this site right now.
pub const NOT_ATTACHED: &str = "the document engine is not attached";
/// No such document on this site (or the reference is not `<doctype>/<id>`).
pub const NO_DOCUMENT: &str = "no such document";
/// The actor is not a user record id (with or without an `agent:` or `user:` prefix).
pub const UNKNOWN_ACTOR: &str = "the actor is not a known user";
/// The operation has no engine call to run on.
pub const NOT_SUPPORTED: &str = "not supported by the document lifecycle";
/// A submitted financial record is changed only through cancel, amend or a journal entry. The
/// text is the world part's own, so the two refusals read alike.
pub const HARD_FLOOR: &str =
    "a submitted financial record changes only through cancel, amend or a journal entry";
/// A financial record is never reposted.
pub const REPOST_FINANCIAL: &str = "a financial record is never reposted";
/// The field is not on the document type.
pub const UNKNOWN_FIELD: &str = "the document type has no such field";
/// A protected field is never recomputed by a remedy.
pub const PROTECTED_FIELD: &str = "a protected field is not recomputed by a remedy";
/// The engine refused: the actor may not do this.
pub const DENIED: &str = "the actor may not change this document";
/// The engine refused: the document changed since it was read.
pub const STALE: &str = "the document changed meanwhile";
/// The engine refused: the state does not allow the operation.
pub const BAD_STATE: &str = "the document is not in a state that allows this";
/// The engine refused: validation.
pub const INVALID: &str = "the change was refused by validation";
/// The engine refused: other documents refer to this one.
pub const LINKED: &str = "other documents still refer to this document";
/// Anything else the engine could not do.
pub const FAILED: &str = "the document engine could not do it";

/// The fixed refusal for an engine error, read from its class only.
#[must_use]
pub fn refusal_of(error: &AppError) -> &'static str {
    match error.class {
        ErrorClass::PermissionDenied => DENIED,
        ErrorClass::NotFound | ErrorClass::UnknownDocType => NO_DOCUMENT,
        ErrorClass::StaleVersion => STALE,
        ErrorClass::InvalidState => BAD_STATE,
        ErrorClass::Validation => INVALID,
        ErrorClass::Linked => LINKED,
        ErrorClass::Duplicate
        | ErrorClass::MethodNotFound
        | ErrorClass::Unavailable
        | ErrorClass::Internal => FAILED,
    }
}
