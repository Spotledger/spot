//! Naming rules: a label a business user types becomes a safe storage identifier, and two
//! labels that collide get distinct names.
//!
//! The rules are pure text, so the same label always yields the same name (the client can show
//! the name it is about to use). A name is a storage name: lower-case letters, digits and `_`,
//! starting with a letter, at most 63 bytes — the same shape `spot-meta` validates.
//!
//! A field added to an **existing** DocType must be a custom field (its name starts with
//! `custom_`, the rule `spot-meta`'s registry enforces); a field of a brand-new DocType is a
//! definition field and carries no prefix. [`field_name`] takes the choice, so the designer
//! applies it at edit time rather than discovering it at publish time.

/// The prefix every custom field's name must have (`spot-meta::CUSTOM_PREFIX`).
pub use spot_meta::CUSTOM_PREFIX;

/// Longest storage name, in bytes (the `Name` contract).
pub const MAX_NAME_BYTES: usize = 63;

/// Whether `name` is a storage name: `[a-z][a-z0-9_]*`, at most [`MAX_NAME_BYTES`] bytes.
#[must_use]
pub fn is_storage_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    name.len() <= MAX_NAME_BYTES
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Turns a human label into a safe storage identifier.
///
/// Every run of characters that is not a lower-case letter or a digit becomes one `_`; leading
/// and trailing `_` are dropped; a name that would start with a digit or be empty is prefixed
/// with `f`; the result is cut to [`MAX_NAME_BYTES`] bytes. The transformation is pure and
/// deterministic.
#[must_use]
pub fn safe_identifier(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut pending_sep = false;
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('_');
            }
            pending_sep = false;
            out.push(c.to_ascii_lowercase());
        } else {
            pending_sep = true;
        }
    }
    if out.is_empty() {
        out.push('f');
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, 'f');
    }
    truncate_name(&out)
}

/// Turns a label into a field name for a DocType: a `custom_`-prefixed name when the DocType
/// already exists, a plain storage name when the DocType is being created.
#[must_use]
pub fn field_name(label: &str, existing_doctype: bool) -> String {
    let base = safe_identifier(label);
    if existing_doctype {
        if base.starts_with(CUSTOM_PREFIX) {
            truncate_name(&base)
        } else {
            truncate_name(&format!("{CUSTOM_PREFIX}{base}"))
        }
    } else {
        base
    }
}

/// Turns a label into a DocType name (never prefixed; a custom DocType carries its own name).
#[must_use]
pub fn doctype_name(label: &str) -> String {
    safe_identifier(label)
}

/// A name that is not among `taken`: `base`, else `base_2`, `base_3`, ... Each candidate is cut
/// to [`MAX_NAME_BYTES`] bytes so the suffix always fits.
#[must_use]
pub fn unique_name(taken: &[String], base: &str) -> String {
    if !taken.iter().any(|t| t == base) {
        return truncate_name(base);
    }
    let mut n = 2u32;
    loop {
        let candidate = truncate_name(&format!("{base}_{n}"));
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Cuts `name` to [`MAX_NAME_BYTES`] bytes on a character boundary, then trims a trailing `_`
/// so a truncation never leaves a name ending in the separator.
fn truncate_name(name: &str) -> String {
    if name.len() <= MAX_NAME_BYTES {
        return name.to_owned();
    }
    let mut cut = MAX_NAME_BYTES;
    while cut > 0 && !name.is_char_boundary(cut) {
        cut -= 1;
    }
    name[..cut].trim_end_matches('_').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_becomes_a_storage_name() {
        assert_eq!(
            safe_identifier("Vehicle Registration"),
            "vehicle_registration"
        );
        assert_eq!(safe_identifier("  Cost (£)  "), "cost");
        assert_eq!(safe_identifier("Qty-2"), "qty_2");
        assert_eq!(safe_identifier("2nd owner"), "f2nd_owner");
        assert_eq!(safe_identifier("!!!"), "f");
    }

    #[test]
    fn custom_fields_are_prefixed_only_on_existing_doctypes() {
        assert_eq!(field_name("Service Date", false), "service_date");
        assert_eq!(field_name("Service Date", true), "custom_service_date");
        assert_eq!(field_name("custom_already", true), "custom_already");
    }

    #[test]
    fn collisions_get_a_suffix() {
        let taken = vec!["notes".to_owned(), "notes_2".to_owned()];
        assert_eq!(unique_name(&taken, "notes"), "notes_3");
        assert_eq!(unique_name(&taken, "other"), "other");
    }

    #[test]
    fn names_are_cut_to_the_limit() {
        let long = "a".repeat(80);
        assert_eq!(safe_identifier(&long).len(), MAX_NAME_BYTES);
        assert!(is_storage_name(&safe_identifier(&long)));
    }

    #[test]
    fn storage_name_shape() {
        assert!(is_storage_name("service_date"));
        assert!(is_storage_name("a1"));
        assert!(!is_storage_name("Service"));
        assert!(!is_storage_name("1abc"));
        assert!(!is_storage_name("a-b"));
        assert!(!is_storage_name(""));
    }
}
