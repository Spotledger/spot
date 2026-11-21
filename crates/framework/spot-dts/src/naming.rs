//! TypeScript declaration naming: DocType and field storage names (`Name`, always
//! `[a-z][a-z0-9_]*`) into valid TypeScript, with collision checks.
//!
//! Interface names are PascalCase by the same rule as `spot_typed_views::map::pascal`
//! (`sales_invoice` -> `SalesInvoice`), copied here so this crate does not depend on a
//! proc-macro crate. TypeScript treats interfaces and properties differently from Rust:
//!
//! - A property keeps its storage spelling and is **quoted when it is not a valid TypeScript
//!   identifier or is a reserved word** (`"type"`); TypeScript allows reserved words as
//!   unquoted member names, but quoting every risky name makes the generated file unambiguous
//!   and the proptest simple: a name either spells an identifier or is quoted.
//! - An interface name that would collide with a TypeScript keyword or global (`any`, `string`)
//!   is not suffixed: a DocType named `any` is a named error instead of a silent rename.
//! - Two source names mapping onto the same interface or the same property are a named error,
//!   never a silent rename. (One property of a DocType against one interface of another
//!   DocType is fine: different scoping.)

/// The TypeScript reserved words plus the words every scope binds (`string`, `number`, ...).
/// Quoting a property named so is always safe; refusing one as an interface name is too.
pub const RESERVED: [&str; 76] = [
    "abstract",
    "any",
    "arguments",
    "as",
    "asserts",
    "async",
    "await",
    "bigint",
    "boolean",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "constructor",
    "continue",
    "debugger",
    "declare",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "implements",
    "import",
    "in",
    "infer",
    "instanceof",
    "interface",
    "is",
    "keyof",
    "let",
    "module",
    "namespace",
    "never",
    "new",
    "null",
    "number",
    "object",
    "of",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "return",
    "satisfies",
    "set",
    "static",
    "string",
    "super",
    "switch",
    "symbol",
    "this",
    "throw",
    "true",
    "try",
    "type",
    "typeof",
    "undefined",
    "unique",
    "unknown",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// Whether `word` cannot be spelled as a TypeScript identifier even quoted is never needed:
/// this is the list behind [`is_reserved_word`].
#[must_use]
pub fn is_reserved_word(word: &str) -> bool {
    RESERVED.contains(&word)
}

/// Whether `text` is a valid TypeScript identifier: starts with a letter, `_` or `$`, continues
/// with letters, digits, `_` or `$`. (ASCII only; a non-ASCII letter is quoted like any other
/// character, which keeps the proptest and the grammar one rule.)
#[must_use]
pub fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// The PascalCase interface name of a snake_case DocType name: `sales_invoice` ->
/// `SalesInvoice`. Words split at `_`; digits are kept inside their word. The rule is
/// `spot_typed_views::map::pascal` copied, minus the keyword suffix: here a collision with a
/// reserved word is an error, because TypeScript has no raw identifiers to fall back on.
///
/// # Errors
/// A name that is not lowercase snake_case ASCII, maps onto nothing, or is a reserved word
/// ([`DtsErrorKind::InvalidName`]).
pub fn interface_name(name: &str) -> Result<String, DtsError> {
    let mut out = String::with_capacity(name.len());
    for word in name.split('_') {
        if !word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err(DtsError::new(
                DtsErrorKind::InvalidName,
                format!("`{name}` is not a snake_case DocType name"),
            )
            .about_name(name));
        }
        if word.is_empty() {
            continue;
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
        }
        out.push_str(chars.as_str());
    }
    if out.is_empty() {
        return Err(DtsError::new(
            DtsErrorKind::InvalidName,
            format!("`{name}` maps onto no TypeScript interface name"),
        )
        .about_name(name));
    }
    if is_reserved_word(&out.to_ascii_lowercase()) {
        return Err(DtsError::new(
            DtsErrorKind::InvalidName,
            format!("`{name}` maps onto the TypeScript reserved word `{out}`; rename the DocType"),
        )
        .about_name(name));
    }
    Ok(out)
}

/// The property spelling of a field name: the storage name verbatim when it is a valid
/// identifier and not a reserved word, else the quoted form. Quoting keeps digits-leading,
/// spaced and reserved names representable without renaming anything.
#[must_use]
pub fn property_name(name: &str) -> String {
    if is_identifier(name) && !is_reserved_word(name) {
        name.to_owned()
    } else {
        // The storage `Name` grammar allows only letters, digits and `_`, but a future format or
        // a quoted edge never hurts: escape what JSON would need escaping too.
        let mut out = String::with_capacity(name.len() + 2);
        out.push('"');
        for c in name.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                other => out.push(other),
            }
        }
        out.push('"');
        out
    }
}

use crate::error::{DtsError, DtsErrorKind};
