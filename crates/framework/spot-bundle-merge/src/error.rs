//! The crate's error type: a stable `bundle_merge.*` code with a plain-language message.
//!
//! Every code here has one entry in `crates/framework/spot-errors/catalogue/bundle_merge.toml`
//! with a summary, the parameters its message names and a remedy.

use std::fmt;

/// The stable `bundle_merge.*` codes this crate raises and attaches to conflicts.
///
/// The item- and semantic-conflict codes are the stable identity of a
/// [`ConflictKind`](crate::ConflictKind); the `unresolved`/`unknown`/`duplicate` codes are the
/// ways [`resolve`](crate::resolve) refuses a resolution set.
pub mod codes {
    /// Both sides changed one item, differently.
    pub const CONFLICT_BOTH_MODIFIED: &str = "bundle_merge.conflict_both_modified";
    /// One side changed an item the other side removed.
    pub const CONFLICT_MODIFIED_VS_REMOVED: &str = "bundle_merge.conflict_modified_vs_removed";
    /// Both sides added an item with the same key, differently.
    pub const CONFLICT_BOTH_ADDED_DIFFERENTLY: &str =
        "bundle_merge.conflict_both_added_differently";
    /// An override survives a merge that removed the custom field it targets.
    pub const SEMANTIC_OVERRIDE_ON_REMOVED_FIELD: &str =
        "bundle_merge.semantic_override_on_removed_field";
    /// A custom field anchors after a field the merge removed.
    pub const SEMANTIC_INSERT_AFTER_REMOVED_FIELD: &str =
        "bundle_merge.semantic_insert_after_removed_field";
    /// Two sides anchor different custom fields to the same predecessor.
    pub const SEMANTIC_INSERT_AFTER_COLLISION: &str =
        "bundle_merge.semantic_insert_after_collision";
    /// Two sides changed different scripts bound to the same hook point.
    pub const SEMANTIC_SCRIPT_HOOK_COLLISION: &str = "bundle_merge.semantic_script_hook_collision";
    /// The merged metadata fails `spot_meta::Registry::build`.
    pub const SEMANTIC_INVALID_META: &str = "bundle_merge.semantic_invalid_meta";
    /// A platform-upgrade conflict, carried through from `spot-migration-conflicts`.
    pub const UPGRADE_CONFLICT: &str = "bundle_merge.upgrade_conflict";
    /// An item's canonical bytes could not be parsed back into its typed value.
    pub const INVALID_ITEM: &str = "bundle_merge.invalid_item";
    /// The merged sections could not be packed into a bundle.
    pub const PACK_FAILED: &str = "bundle_merge.pack_failed";
    /// A conflict has no resolution in the choices supplied to `resolve`.
    pub const UNRESOLVED_CONFLICT: &str = "bundle_merge.unresolved_conflict";
    /// A resolution names a conflict id the result does not carry.
    pub const UNKNOWN_CONFLICT: &str = "bundle_merge.unknown_conflict";
    /// Two resolutions name the same conflict.
    pub const DUPLICATE_RESOLUTION: &str = "bundle_merge.duplicate_resolution";
}

/// The crate's error: a stable code, the offending item, and one plain-language sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BundleMergeError {
    /// The stable code, for example `bundle_merge.unresolved_conflict`.
    code: String,
    /// The item the error is about (a section, an item key, a conflict id), when there is one.
    item: String,
    /// One plain-language sentence; never a secret.
    message: String,
}

impl BundleMergeError {
    /// An error with a code and a message, naming no item.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            item: String::new(),
            message: message.into(),
        }
    }

    /// An error that names the offending item (a section, an item key, a conflict id).
    pub fn about(
        code: impl Into<String>,
        item: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            item: item.into(),
            message: message.into(),
        }
    }

    /// The stable code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The item the error is about, if any.
    pub fn item(&self) -> &str {
        &self.item
    }

    /// The plain-language message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for BundleMergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.item.is_empty() {
            write!(f, "{}: {}", self.code, self.message)
        } else {
            write!(f, "{}: {}: {}", self.code, self.item, self.message)
        }
    }
}

impl std::error::Error for BundleMergeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_code_and_item() {
        let with_item = BundleMergeError::about(codes::UNKNOWN_CONFLICT, "meta:a.b", "no such id");
        assert_eq!(
            with_item.to_string(),
            "bundle_merge.unknown_conflict: meta:a.b: no such id"
        );
        let without = BundleMergeError::new(codes::PACK_FAILED, "too large");
        assert_eq!(without.to_string(), "bundle_merge.pack_failed: too large");
    }
}
