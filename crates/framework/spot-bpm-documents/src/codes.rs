//! The stable `bpm_documents.*` codes this crate reports.
//!
//! A Document node reports a failure as [`NodeOutcome::Fail`](spot_bpm_engine::NodeOutcome::Fail),
//! so these codes ride the engine's failure vocabulary rather than an `AppError`; the catalogue
//! (`crates/framework/spot-errors/catalogue/bpm_documents.toml`) explains each one.

/// The node's config is malformed: an unknown operation, a missing DocType, a mapping that is
/// not an object of expressions.
pub const CONFIG_INVALID: &str = "bpm_documents.config_invalid";

/// An `update`, `submit` or `cancel` node names no document to change, or the document it names
/// has no version.
pub const DOCUMENT_MISSING: &str = "bpm_documents.document_missing";

/// A mapping expression, an expected-version expression or a branch condition could not be
/// evaluated, or produced a value the target field cannot hold.
pub const EXPRESSION_INVALID: &str = "bpm_documents.expression_invalid";

/// Recording the Case file failed: the business-graph edge for a touched document was not
/// written.
pub const CASE_FILE: &str = "bpm_documents.case_file";

/// The Case's acting principal is not a user id the document engine can act as. The node refuses
/// to run rather than fall back to a system actor, which would skip every permission check.
pub const PRINCIPAL_INVALID: &str = "bpm_documents.principal_invalid";

/// A node names a `principal` that is not a registered service principal of the tenant (a plain
/// user id never is), or the registry could not be read, or no registry is configured.
pub const PRINCIPAL_NOT_REGISTERED: &str = "bpm_documents.principal_not_registered";

/// The version's author may not act as the node's service principal: it lacks the publish
/// permission, or the principal's grant list does not include it, or the author is unknown.
pub const PRINCIPAL_NOT_PERMITTED: &str = "bpm_documents.principal_not_permitted";

/// The use of a service principal could not be written to the audit trail, so the node did not
/// act.
pub const PRINCIPAL_AUDIT_FAILED: &str = "bpm_documents.principal_audit_failed";
