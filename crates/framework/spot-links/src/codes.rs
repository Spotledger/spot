//! The stable dotted error codes this crate raises, one per `[[entry]]` in
//! `crates/framework/spot-errors/catalogue/links.toml` (WP-DOC-06.T1).

/// Other documents still refer to the target, so it cannot be deleted or cancelled. The
/// error's details carry the grouped referrers and their count. Class `Linked`.
pub const REFERENCED: &str = "links.referenced";

/// A rename would give a document a name another document already holds. Class `Duplicate`.
pub const NAME_TAKEN: &str = "links.name_taken";

/// A link points at a document that does not exist in the site. Class `Validation`.
pub const TARGET_NOT_FOUND: &str = "links.target_not_found";

/// A link points at a document of the wrong DocType. Class `Validation`.
pub const WRONG_DOCTYPE: &str = "links.wrong_doctype";

/// A dynamic link's type field names no DocType. Class `Validation`.
pub const DYNAMIC_TYPE_UNKNOWN: &str = "links.dynamic_type_unknown";

/// A dynamic link's type names a child row or a reserved kind, which cannot be linked.
/// Class `Validation`.
pub const DYNAMIC_TYPE_NOT_ALLOWED: &str = "links.dynamic_type_not_allowed";

/// A link points at a cancelled document where the field does not allow it. Class `Validation`.
pub const CANCELLED_TARGET: &str = "links.cancelled_target";

/// A posted document points at a draft one. Class `Validation`.
pub const DRAFT_TARGET: &str = "links.draft_target";
