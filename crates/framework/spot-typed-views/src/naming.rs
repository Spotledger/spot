//! The Rust keywords.
//!
//! A field named like a keyword keeps its spelled name through a raw identifier (`type` ->
//! `r#type`). The few names a raw identifier cannot spell (`self`, `Self`, `crate`, the
//! primitive types and the literals) are refused, so generated names can never alias Rust's
//! own types or hide a literal.

/// Strict (reserved) keywords usable as raw identifiers.
pub const STRICT: [&str; 35] = [
    "abstract", "as", "async", "await", "become", "box", "break", "const", "continue", "do",
    "else", "enum", "extern", "final", "fn", "gen", "if", "impl", "in", "let", "loop", "macro",
    "override", "priv", "pub", "ref", "return", "static", "struct", "trait", "try", "type",
    "union", "unsafe", "where",
];

/// Names a raw identifier cannot spell: the contextual keyword forms plus `self`, `Self`,
/// `super` and the primitive type and literal keywords. Refused, not mangled.
pub const UNSAFE_RAW: [&str; 16] = [
    "crate", "self", "Self", "super", "dyn", "true", "false", "bool", "char", "f32", "f64", "i8",
    "i16", "i32", "i64", "u8",
];

/// Whether `word` cannot be an identifier in any form.
#[must_use]
pub fn is_unspellable(word: &str) -> bool {
    UNSAFE_RAW.contains(&word)
}

/// Whether `word` is a strict keyword that works as a raw identifier.
#[must_use]
pub fn is_strict_keyword(word: &str) -> bool {
    STRICT.contains(&word)
}
