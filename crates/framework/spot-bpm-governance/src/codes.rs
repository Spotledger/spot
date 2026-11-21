//! The stable error codes this crate raises, each with one entry in
//! `spot-errors/catalogue/bpm_governance.toml`.
//!
//! They are `pub const`s so the catalogue scan finds them and a caller can match on them without
//! a string literal.

/// A governed record type was created outside its running request, with neither a valid case
/// context nor the outside-process permission and a reason.
pub const OUTSIDE_PROCESS_DENIED: &str = "bpm_governance.outside_process_denied";

/// The outside-process permission was held, but the reason was empty, whitespace or shorter than
/// the configured floor.
pub const REASON_REQUIRED: &str = "bpm_governance.reason_required";

/// A stored governance policy names an unknown mode, or an entry that is not `doctype=mode`.
pub const POLICY_INVALID: &str = "bpm_governance.policy_invalid";

/// A governance setting could not be read, or the settings registry could not be built.
pub const SETTINGS: &str = "bpm_governance.settings";

/// The governance audit entry could not be written, so the operation failed closed and nothing
/// was changed.
pub const AUDIT: &str = "bpm_governance.audit";
