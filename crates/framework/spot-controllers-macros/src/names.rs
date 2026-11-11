//! Name rules shared by the macros: the storage-name rule and Rust keywords.

use proc_macro2::Span;
use syn::Error;

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "try", "typeof", "unsized", "virtual", "yield", "gen",
];

/// Whether `name` is a Rust keyword (strict or reserved).
pub fn is_keyword(name: &str) -> bool {
    KEYWORDS.contains(&name)
}

/// Checks a storage-style name: lowercase letters, digits and underscores, starting with a
/// letter, at most 63 bytes (the rule `Name::new` enforces at run time), and not a Rust keyword.
pub fn check_name(name: &str, what: &str, span: Span) -> Result<(), Error> {
    if is_keyword(name) {
        return Err(Error::new(
            span,
            format!("`{name}` is a Rust keyword and cannot be a {what}; pick another name"),
        ));
    }
    let mut chars = name.chars();
    let ok = name.len() <= 63
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::new(
            span,
            format!(
                "`{name}` is not a valid {what}: use snake_case (lowercase letters, digits and \
                 underscores, starting with a letter, at most 63 bytes)"
            ),
        ))
    }
}
