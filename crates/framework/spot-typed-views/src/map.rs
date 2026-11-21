//! Name mapping: snake_case storage names into Rust identifiers, with collision checks.
//!
//! Every storage `Name` is `[a-z][a-z0-9_]*`, so splitting at `_` gives PascalCase type names
//! (DocTypes, row types, select enums and variants) and variants for select options. Field
//! members keep their storage snake_case spelling as-is, because snake_case is valid Rust; a
//! member whose name is a keyword becomes a raw identifier (`type` -> `r#type`); a type or
//! variant whose PascalCase form is a keyword is suffixed with `Doc`. Two different source
//! names that still map onto the same identifier are a `GenError`.

use std::collections::BTreeMap;

use crate::error::{GenError, GenErrorKind};
use crate::naming;

/// The suffix for a mapped type name that would collide with a Rust keyword or primitive.
const KEYWORD_SUFFIX: &str = "Doc";

/// PascalCase of `name`: `party` -> `Party`, `sales_invoice` -> `SalesInvoice`.
///
/// # Errors
/// A name that is not snake_case (`InvalidName`).
pub fn pascal(name: &str) -> Result<String, GenError> {
    let mut out = String::with_capacity(name.len());
    for word in name.split('_') {
        if !word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err(GenError::new(
                GenErrorKind::InvalidName,
                format!("`{name}` is not a snake_case name"),
            ));
        }
        if word.is_empty() {
            continue;
        }
        out.push_str(&first_to_ascii_uppercase(word));
    }
    if out.is_empty() || out.parse::<u64>().is_ok() {
        return Err(GenError::new(
            GenErrorKind::InvalidName,
            format!("`{name}` maps onto no Rust type name"),
        ));
    }
    if is_reserved(&out) {
        out.push_str(KEYWORD_SUFFIX);
    }
    Ok(out)
}

/// The first byte upper-cased; `word` is lowercase ASCII, so this is one byte.
fn first_to_ascii_uppercase(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    if let Some(first) = chars.next() {
        out.push(first.to_ascii_uppercase());
    }
    out.push_str(chars.as_str());
    out
}

/// The PascalCase variant of one select option. Options are free-form text, not storage
/// names: every run of letters and digits becomes one capitalised word (`Open` -> `Open`,
/// `cash on delivery` -> `CashOnDelivery`); a leading digit or no letters is refused.
///
/// # Errors
/// An option that maps onto no Rust variant name (`InvalidName`).
pub fn option_variant(text: &str) -> Result<String, GenError> {
    enum Part {
        Letter,
        Digit,
        Other,
    }
    let class = |c: char| {
        if c.is_ascii_alphabetic() {
            Part::Letter
        } else if c.is_ascii_digit() {
            Part::Digit
        } else {
            Part::Other
        }
    };
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    let flush = |out: &mut String, word: &mut String| {
        if !word.is_empty() {
            out.push_str(&first_to_ascii_uppercase(word));
            word.clear();
        }
    };
    let mut prev: Option<Part> = None;
    for c in text.chars() {
        match class(c) {
            Part::Letter => {
                word.push(c.to_ascii_lowercase());
                prev = Some(class(c));
            }
            Part::Digit => {
                // A digit after letters ends the word, so `b2b` splits (`B2B`).
                if matches!(prev, Some(Part::Letter)) {
                    flush(&mut out, &mut word);
                }
                word.push(c);
                prev = Some(class(c));
            }
            Part::Other => flush(&mut out, &mut word),
        }
    }
    flush(&mut out, &mut word);
    if out.is_empty() || !out.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return Err(GenError::new(
            GenErrorKind::InvalidName,
            format!("select option `{text}` maps onto no Rust variant name"),
        ));
    }
    if out == "Self" {
        out.push_str(KEYWORD_SUFFIX);
    }
    Ok(out)
}

/// The member (method) identifier: the storage snake_case name kept verbatim, since snake_case
/// is valid Rust (`posting_date`); a keyword member becomes a raw identifier (`type` ->
/// `r#type`); a name no identifier can spell (`self`, `Self`, the primitives) is refused, so
/// the generated code can never alias Rust's own names.
///
/// # Errors
/// A name that is not snake_case, or no identifier can spell it (`InvalidName`).
pub fn field_ident(name: &str) -> Result<String, GenError> {
    if naming::is_unspellable(name) {
        return Err(GenError::new(
            GenErrorKind::InvalidName,
            format!("`{name}` cannot be a Rust identifier, even as a raw one"),
        ));
    }
    validate_snake(name)?;
    if naming::is_strict_keyword(name) {
        return Ok(format!("r#{name}"));
    }
    Ok(name.to_owned())
}

/// Checks every non-empty run between `_` is lowercase ASCII letters or digits, so the name is
/// a valid Rust identifier (a leading `_`-run would make it start with `_`; storage names
/// cannot have one, and this keeps the mapping honest for any `&str` input).
///
/// # Errors
/// A name that is not snake_case (`InvalidName`).
fn validate_snake(name: &str) -> Result<(), GenError> {
    if name.split('_').any(|word| {
        !word.is_empty()
            && !word
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    }) {
        return Err(GenError::new(
            GenErrorKind::InvalidName,
            format!("`{name}` is not a snake_case name"),
        ));
    }
    Ok(())
}

/// Whether `type_name` would collide with Rust's own names.
fn is_reserved(type_name: &str) -> bool {
    let lower = type_name.to_ascii_lowercase();
    naming::is_strict_keyword(&lower)
        || naming::is_unspellable(&lower)
        || matches!(
            lower.as_str(),
            "f16"
                | "i128"
                | "i16"
                | "i64"
                | "isize"
                | "str"
                | "string"
                | "u128"
                | "u16"
                | "u32"
                | "u64"
                | "usize"
        )
}

/// Reserved names per scope, so two source names mapping onto one identifier are named errors.
#[derive(Debug, Default)]
pub struct Namespace {
    by_ident: BTreeMap<String, String>,
}

impl Namespace {
    /// An empty scope.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `ident` as taken by `source`, described by `kind` (for example `field`).
    ///
    /// # Errors
    /// Another source already took `ident` (`Collision`).
    pub fn reserve(&mut self, source: &str, kind: &str, ident: String) -> Result<String, GenError> {
        match self.by_ident.get(&ident) {
            Some(taken) => Err(GenError::new(
                GenErrorKind::Collision,
                format!(
                    "{kind} `{source}` maps onto `{ident}`, which {kind} `{taken}` already uses"
                ),
            )),
            None => {
                self.by_ident.insert(ident.clone(), source.to_owned());
                Ok(ident)
            }
        }
    }

    /// Whether `ident` is free, without taking it.
    #[must_use]
    pub fn is_free(&self, ident: &str) -> bool {
        !self.by_ident.contains_key(ident)
    }
}
