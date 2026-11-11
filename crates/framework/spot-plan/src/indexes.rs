//! The single-column unique indexes a plan asks the storage for (WP-DOC-05.T4).
//!
//! A DocType with a `naming` pattern has **unique names**: the document engine refuses a
//! second document whose name is taken, under a per-name transaction lock
//! (`spot-document`'s `refuse_taken_name`). That refusal is only as good as every writer
//! going through the engine, so the plan also asks for a **storage-level backstop**: a unique
//! index on the DocType's title column, which the database refuses a duplicate row on whatever
//! wrote it — a migration, an import, a future bug.
//!
//! The index names follow the storage catalogue's own rule exactly — `uq_<doctype>_<column>`,
//! cut to [`Name::MAX_LEN`] bytes and given an FNV-1a hash of the full name when it is longer
//! (`spot-meta`'s `compile::fit_name`) — so a title column that is already `unique` yields the
//! *same* name as the backstop would, and therefore one index, never two.
//!
//! Only single-column indexes are planned here. The multi-field indexes a DocType declares in
//! its definition stay the storage catalogue's business: repeating them in the plan would put
//! two owners on one name. Where a declared unique index covers exactly the title column its
//! name is the same `uq_<doctype>_<column>`, so the catalogue still creates a single index.

use spot_app_api::prelude::Name;
use spot_meta::Definition;

/// One single-column unique index the plan asks the storage for.
///
/// The name is a `String` rather than a [`Name`] because it is derived from names the metadata
/// has already validated; [`PlanIndex::name`] documents the construction and
/// `every_planned_index_name_is_a_valid_storage_name` (tests) proves it for a name at the
/// limit. A consumer building a storage definition re-validates it with `Name::new` and
/// reports its own compile error, rather than this crate growing a `plan.*` code the error
/// catalogue would have to know about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanIndex {
    name: String,
    column: Name,
}

impl PlanIndex {
    /// The index name: `uq_<doctype>_<column>`, or that cut to [`Name::MAX_LEN`] bytes with
    /// `_<8 hex digits>` appended. Lowercase ASCII, starting with `u`, so it is a valid
    /// storage name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The indexed column.
    #[must_use]
    pub fn column(&self) -> &Name {
        &self.column
    }

    /// The unique index over `column` of `doctype`'s table.
    pub(crate) fn over(doctype: &str, column: &Name) -> Self {
        Self {
            name: unique_index_name(doctype, column.as_str()),
            column: column.clone(),
        }
    }
}

/// The title field whose name the storage must keep unique, when the DocType needs the naming
/// backstop: it declares a `naming` pattern, is not a child table (rows of a parent's table
/// carry no name of their own) and has a title field for the engine to write that name into.
///
/// A DocType with `naming` and no `title_field` gets no index: the engine refuses to name such
/// a document at all (`document.naming_title_field_missing`), so there is no name to keep
/// unique.
pub(crate) fn naming_title_field(doctype: &Definition) -> Option<&str> {
    if doctype.is_child {
        return None;
    }
    doctype.naming.as_ref()?;
    doctype.title_field.as_deref()
}

/// The name of the unique index over one column: `uq_<doctype>_<column>`.
///
/// Over [`Name::MAX_LEN`] bytes the name is cut and `_<hash>` is appended, the hash being
/// FNV-1a over the full name — byte for byte what `spot-meta`'s `compile::fit_name` does for a
/// unique single-column index, so the two never name one index two ways. Metadata names are
/// lowercase ASCII, so the cut is on a byte boundary; a hand-built definition with a non-ASCII
/// name (which the registry refuses before a plan exists) is cut on a character boundary
/// instead of panicking.
fn unique_index_name(doctype: &str, column: &str) -> String {
    let full = format!("uq_{doctype}_{column}");
    if full.len() <= Name::MAX_LEN {
        return full;
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in full.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    // `keep` is a byte budget: for the ASCII names metadata allows this is exactly
    // `&full[..keep]`; a character that would cross the budget ends the head instead of
    // splitting it, so no input can panic here.
    let keep = Name::MAX_LEN - 9;
    let mut head = String::new();
    for ch in full.chars() {
        if head.len() + ch.len_utf8() > keep {
            break;
        }
        head.push(ch);
    }
    format!("{head}_{hash:08x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_name_is_the_catalogue_form() {
        assert_eq!(unique_index_name("invoice", "number"), "uq_invoice_number");
    }

    #[test]
    fn a_long_name_is_cut_and_hashed_within_the_limit() {
        let doctype = "a".repeat(60);
        let name = unique_index_name(&doctype, "title");
        assert_eq!(name.len(), Name::MAX_LEN);
        // The head is as much of `uq_<doctype>_<column>` as fits, then the hash suffix.
        assert!(name.starts_with("uq_"), "{name}");
        assert!(format!("uq_{doctype}_title").starts_with(&name[..name.len() - 9]));
        assert_eq!(name.len() - 9, Name::MAX_LEN - 9);
        assert!(name[name.len() - 9..].starts_with('_'));
        // Stable for the same input, and distinct for a different one.
        assert_eq!(name, unique_index_name(&doctype, "title"));
        assert_ne!(name, unique_index_name(&"a".repeat(59), "title"));
    }

    #[test]
    fn a_non_ascii_name_is_cut_without_panicking() {
        let name = unique_index_name(&"é".repeat(40), "título");
        assert!(name.len() <= Name::MAX_LEN);
    }

    #[test]
    fn the_backstop_needs_naming_a_title_field_and_a_document_table() {
        let mut d = Definition::new("invoice");
        d.naming = Some("INV-.#####".into());
        d.title_field = Some("number".into());
        assert_eq!(naming_title_field(&d), Some("number"));
        d.is_child = true;
        assert_eq!(
            naming_title_field(&d),
            None,
            "a child table carries no name"
        );
        d.is_child = false;
        d.naming = None;
        assert_eq!(naming_title_field(&d), None, "no naming, no unique name");
        d.naming = Some("INV-.#####".into());
        d.title_field = None;
        assert_eq!(naming_title_field(&d), None, "no title field to name");
    }
}
