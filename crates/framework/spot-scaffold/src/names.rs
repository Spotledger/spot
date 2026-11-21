//! Names: the storage-name rules, the reserved words and prefixes, and the derivation of a
//! storage name from a human label.
//!
//! A storage name is `[a-z][a-z0-9_]*` and at most 63 bytes (`spot_storage_contract::Name`,
//! the PostgreSQL identifier limit). On top of that the platform refuses names it reserves:
//!
//! - the Rust keywords (a name becomes an identifier in generated code);
//! - the SQL words the storage layer keeps for its own use;
//! - the platform prefixes `doc_`, `sys_`, `identity_` and `spot_`;
//! - the reserved system columns (`id`, `version`, `parent`, `parent_field`, `idx`) and the
//!   engine-managed fields (`doc_state`, `amended_from`).
//!
//! A refusal names the rule and suggests a valid name. A label such as `Sales Invoice` is
//! derived to `sales_invoice` and shown to the author, so the rule is visible rather than
//! guessed.

use std::fmt;

use crate::error::{Result, ScaffoldError};

/// The longest storage name in bytes (the PostgreSQL identifier limit).
pub const MAX_NAME_BYTES: usize = 63;

/// The platform prefixes reserved for the platform itself.
pub const RESERVED_PREFIXES: [&str; 4] = ["doc_", "sys_", "identity_", "spot_"];

/// The reserved system columns (the storage layer keeps them).
pub const SYSTEM_COLUMNS: [&str; 5] = ["id", "version", "parent", "parent_field", "idx"];

/// The engine-managed fields on every posted DocType.
pub const ENGINE_FIELDS: [&str; 2] = ["doc_state", "amended_from"];

/// The Rust keywords, refused because a scaffolded name becomes a Rust identifier.
pub const RUST_KEYWORDS: [&str; 51] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use",
    "where", "while", "abstract", "become", "box", "do", "final", "macro", "override", "priv",
    "typeof", "unsized", "virtual", "yield", "try", "union",
];

/// The SQL words the storage layer keeps for its own statements. Storage quotes identifiers,
/// so these are not a correctness requirement; they are refused so a scaffolded name never
/// reads as a keyword to a person or a tool.
pub const SQL_RESERVED: [&str; 68] = [
    "all",
    "alter",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "between",
    "by",
    "case",
    "cast",
    "check",
    "column",
    "constraint",
    "create",
    "cross",
    "current",
    "default",
    "delete",
    "desc",
    "distinct",
    "drop",
    "else",
    "end",
    "except",
    "exists",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "left",
    "like",
    "limit",
    "natural",
    "not",
    "null",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "right",
    "select",
    "table",
    "union",
    "unique",
    "update",
    "user",
    "using",
    "values",
    "when",
    "where",
    "with",
];

/// Which kind of name is being validated, for the refusal message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameKind {
    /// An app name (`[app].name`).
    App,
    /// A DocType storage name (`doctypes/<name>.toml`).
    DocType,
    /// A field name.
    Field,
    /// A script name.
    Script,
}

impl NameKind {
    /// The word used in a refusal message.
    #[must_use]
    pub fn noun(self) -> &'static str {
        match self {
            Self::App => "app name",
            Self::DocType => "DocType name",
            Self::Field => "field name",
            Self::Script => "script name",
        }
    }
}

/// Why a name is not a storage name, or is reserved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NameFault {
    /// It is empty.
    Empty,
    /// It is longer than [`MAX_NAME_BYTES`].
    TooLong(usize),
    /// It starts with something other than a lowercase ASCII letter.
    BadStart,
    /// It holds a character outside `a-z`, `0-9`, `_`.
    BadChar(char),
    /// It is a reserved word (Rust or SQL).
    ReservedWord(&'static str),
    /// It starts with a platform prefix.
    ReservedPrefix(&'static str),
}

impl NameFault {
    /// A plain-language reason.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Empty => "it is empty".to_owned(),
            Self::TooLong(n) => format!("it is {n} bytes; the limit is {MAX_NAME_BYTES}"),
            Self::BadStart => "it must start with a lowercase letter (a-z)".to_owned(),
            Self::BadChar(c) => {
                format!("it holds `{c}`; only lowercase letters, digits and `_` are allowed")
            }
            Self::ReservedWord(w) => format!("`{w}` is a reserved word"),
            Self::ReservedPrefix(p) => {
                format!("the prefix `{p}` is reserved for the platform")
            }
        }
    }
}

/// Checks a raw string against the storage-name rules and the reserved lists.
///
/// # Errors
/// [`NameFault`] naming the first rule the name breaks.
pub fn check_storage_name(raw: &str) -> std::result::Result<(), NameFault> {
    if raw.is_empty() {
        return Err(NameFault::Empty);
    }
    if raw.len() > MAX_NAME_BYTES {
        return Err(NameFault::TooLong(raw.len()));
    }
    let mut chars = raw.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return Err(NameFault::BadStart),
    }
    for c in chars {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return Err(NameFault::BadChar(c));
        }
    }
    for word in RUST_KEYWORDS.iter().chain(SQL_RESERVED.iter()) {
        if raw == *word {
            return Err(NameFault::ReservedWord(word));
        }
    }
    for prefix in RESERVED_PREFIXES {
        if raw.starts_with(prefix) {
            return Err(NameFault::ReservedPrefix(prefix));
        }
    }
    for column in SYSTEM_COLUMNS.iter().chain(ENGINE_FIELDS.iter()) {
        if raw == *column {
            return Err(NameFault::ReservedWord(column));
        }
    }
    Ok(())
}

/// A best-effort valid storage name from a label: lower-cased, every run of non-name
/// characters folded to one `_`, a leading digit prefixed with `n_`, and the result cut to
/// [`MAX_NAME_BYTES`] at a character boundary.
#[must_use]
pub fn suggest_name(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        return "item".to_owned();
    }
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert_str(0, "n_");
    }
    if out.len() > MAX_NAME_BYTES {
        out.truncate(MAX_NAME_BYTES);
        while out.ends_with('_') {
            out.pop();
        }
    }
    out
}

/// Turns a fault into a refusal that names the rule and suggests a valid name.
fn refusal(kind: NameKind, raw: &str, fault: &NameFault) -> ScaffoldError {
    let suggestion = suggest_name(raw);
    ScaffoldError::invalid_name(format!(
        "the {} `{raw}` is not a valid storage name: {}. Use a name like `{suggestion}` \
         (lowercase letters, digits and `_`, starting with a letter, at most {MAX_NAME_BYTES} \
         bytes)",
        kind.noun(),
        fault.reason()
    ))
}

/// Validates a raw string as a name of `kind`.
///
/// # Errors
/// A [`ScaffoldError`] with `scaffold.invalid_name` (or `scaffold.reserved_name`) naming the
/// rule and suggesting a valid name.
pub fn validate(kind: NameKind, raw: &str) -> Result<String> {
    match check_storage_name(raw) {
        Ok(()) => Ok(raw.to_owned()),
        Err(fault @ (NameFault::ReservedWord(_) | NameFault::ReservedPrefix(_))) => {
            let suggestion = suggest_name(raw);
            Err(ScaffoldError::reserved_name(format!(
                "the {} `{raw}` is reserved: {}. Use a name like `{suggestion}`",
                kind.noun(),
                fault.reason()
            )))
        }
        Err(fault) => Err(refusal(kind, raw, &fault)),
    }
}

/// A validated app name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AppName(String);

impl AppName {
    /// Validates an app name.
    ///
    /// # Errors
    /// As [`validate`], for [`NameKind::App`].
    pub fn parse(raw: &str) -> Result<Self> {
        validate(NameKind::App, raw).map(Self)
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated DocType storage name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DocTypeName(String);

impl DocTypeName {
    /// Validates a DocType storage name.
    ///
    /// # Errors
    /// As [`validate`], for [`NameKind::DocType`].
    pub fn parse(raw: &str) -> Result<Self> {
        validate(NameKind::DocType, raw).map(Self)
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DocTypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A validated field name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FieldName(String);

impl FieldName {
    /// Validates a field name.
    ///
    /// # Errors
    /// As [`validate`], for [`NameKind::Field`].
    pub fn parse(raw: &str) -> Result<Self> {
        validate(NameKind::Field, raw).map(Self)
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FieldName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Derives a DocType storage name from a human label such as `Sales Invoice`.
///
/// # Errors
/// `scaffold.invalid_name` when the label holds a character outside letters, digits, spaces,
/// `_` and `-` (a unicode label is refused, not silently folded), or when nothing usable
/// remains.
pub fn derive_doctype_name(label: &str) -> Result<DocTypeName> {
    let trimmed = label.trim();
    if trimmed.is_empty() {
        return Err(ScaffoldError::invalid_name(
            "the label is empty; give a label such as `Sales Invoice`",
        ));
    }
    for c in trimmed.chars() {
        if !(c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-') {
            return Err(ScaffoldError::invalid_name(format!(
                "the label `{label}` holds `{c}`; only ASCII letters, digits, spaces, `_` and \
                 `-` can be derived to a storage name — give an ASCII label or a name with \
                 `--name`"
            )));
        }
    }
    let derived = suggest_name(trimmed);
    DocTypeName::parse(&derived)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn storage_names_validate() {
        assert!(AppName::parse("acme").is_ok());
        assert!(AppName::parse("sales_invoice_2").is_ok());
        assert!(AppName::parse("Sales").is_err());
        assert!(AppName::parse("1abc").is_err());
        assert!(AppName::parse("a-b").is_err());
        assert!(AppName::parse("").is_err());
        assert!(AppName::parse(&"a".repeat(63)).is_ok());
        assert!(AppName::parse(&"a".repeat(64)).is_err());
    }

    #[test]
    fn reserved_words_and_prefixes_are_refused() {
        assert_eq!(
            AppName::parse("order").unwrap_err().code,
            crate::codes::RESERVED_NAME
        );
        assert_eq!(
            AppName::parse("fn").unwrap_err().code,
            crate::codes::RESERVED_NAME
        );
        assert_eq!(
            AppName::parse("doc_thing").unwrap_err().code,
            crate::codes::RESERVED_NAME
        );
        assert_eq!(
            FieldName::parse("parent").unwrap_err().code,
            crate::codes::RESERVED_NAME
        );
    }

    #[test]
    fn labels_derive() {
        assert_eq!(
            derive_doctype_name("Sales Invoice").map(|n| n.as_str().to_owned()),
            Ok("sales_invoice".to_owned())
        );
        assert!(derive_doctype_name("Café").is_err());
        assert_eq!(suggest_name("  -- x --"), "x");
    }
}
