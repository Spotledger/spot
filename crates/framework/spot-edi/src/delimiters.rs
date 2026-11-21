//! Delimiters and their discovery (WP-INT-06.T1, brief item 1).
//!
//! EDI's delimiters are not fixed by the standard: a document carries its own. X12's `ISA`
//! segment is a fixed-width 106-character record, so the element, repetition, component and
//! segment separators sit at known offsets (3, 82, 104 and 105). EDIFACT either opens with a
//! nine-character `UNA` service string advice that names them, or uses the syntax defaults and
//! reveals the element separator as the fourth character of `UNB` and the component separator
//! inside `UNB01` (for example `UNOA:1`).
//!
//! Everything here works on bytes: the adapter is ASCII-only by design, and discovery happens
//! before any decoding so a malformed header is an error and never a panic.

use serde::{Deserialize, Serialize};

use crate::error::{EdiError, Result};

/// Which of the two syntaxes a document is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Syntax {
    /// ASC X12 (the `ISA`/`GS`/`ST` family).
    X12,
    /// UN/EDIFACT (the `UNB`/`UNG`/`UNH` family).
    Edifact,
}

impl Syntax {
    /// The name used in messages and schema ids (`X12`, `EDIFACT`).
    pub fn name(self) -> &'static str {
        match self {
            Self::X12 => "X12",
            Self::Edifact => "EDIFACT",
        }
    }

    /// The interchange header segment's tag (`ISA` or `UNB`).
    pub fn interchange_header(self) -> &'static str {
        match self {
            Self::X12 => "ISA",
            Self::Edifact => "UNB",
        }
    }

    /// The interchange trailer segment's tag (`IEA` or `UNZ`).
    pub fn interchange_trailer(self) -> &'static str {
        match self {
            Self::X12 => "IEA",
            Self::Edifact => "UNZ",
        }
    }

    /// The functional-group header segment's tag (`GS` or `UNG`).
    pub fn group_header(self) -> &'static str {
        match self {
            Self::X12 => "GS",
            Self::Edifact => "UNG",
        }
    }

    /// The functional-group trailer segment's tag (`GE` or `UNE`).
    pub fn group_trailer(self) -> &'static str {
        match self {
            Self::X12 => "GE",
            Self::Edifact => "UNE",
        }
    }

    /// The transaction-set/message header segment's tag (`ST` or `UNH`).
    pub fn message_header(self) -> &'static str {
        match self {
            Self::X12 => "ST",
            Self::Edifact => "UNH",
        }
    }

    /// The transaction-set/message trailer segment's tag (`SE` or `UNT`).
    pub fn message_trailer(self) -> &'static str {
        match self {
            Self::X12 => "SE",
            Self::Edifact => "UNT",
        }
    }
}

/// The separators a document uses.
///
/// `repetition` and `release` are optional: X12 always has a repetition separator (ISA11, which
/// some old documents leave as a space) and never has a release character; EDIFACT has a
/// release character (default `?`) and only syntax version 4 defines a repetition separator (the
/// reserved sixth character of `UNA`). `una` records whether the document carried a `UNA`
/// service string advice, so the writer reproduces it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Delimiters {
    /// The element separator (`*` in X12, `+` in EDIFACT).
    pub element: char,
    /// The component separator (`>` in X12, `:` in EDIFACT).
    pub component: char,
    /// The repetition separator, when the syntax defines one.
    pub repetition: Option<char>,
    /// The release (escape) character, EDIFACT only.
    pub release: Option<char>,
    /// The segment terminator (`~` in X12, `'` in EDIFACT).
    pub terminator: char,
    /// The decimal notation character from `UNA`, EDIFACT only.
    pub decimal: Option<char>,
    /// Whether the document opened with a `UNA` service string advice.
    pub una: bool,
}

impl Delimiters {
    /// The X12 defaults, as a freshly discovered `ISA` would give them.
    pub fn x12(element: char, component: char, repetition: Option<char>, terminator: char) -> Self {
        Self {
            element,
            component,
            repetition,
            release: None,
            terminator,
            decimal: None,
            una: false,
        }
    }

    /// The EDIFACT syntax defaults (`:` `+` `'`, release `?`, no repetition).
    pub fn edifact_defaults() -> Self {
        Self {
            element: '+',
            component: ':',
            repetition: None,
            release: Some('?'),
            terminator: '\'',
            decimal: Some('.'),
            una: false,
        }
    }

    /// Every separator that must be distinct for the document to be unambiguous.
    pub(crate) fn separators(self) -> Vec<char> {
        let mut out = vec![self.element, self.component, self.terminator];
        if let Some(c) = self.repetition {
            out.push(c);
        }
        if let Some(c) = self.release {
            out.push(c);
        }
        out
    }
}

/// Discovers the syntax and delimiters of `input`.
///
/// Fails with [`EdiError::BadDelimiters`] when the input is shorter than a header or when two
/// separators collide. Never panics on arbitrary bytes.
pub fn discover(input: &[u8]) -> Result<(Syntax, Delimiters)> {
    if input.starts_with(b"UNA") {
        let una = discover_una(input)?;
        return Ok((Syntax::Edifact, una));
    }
    if input.starts_with(b"ISA") {
        return Ok((Syntax::X12, discover_isa(input)?));
    }
    if input.starts_with(b"UNB") {
        return Ok((Syntax::Edifact, discover_unb(input)?));
    }
    Err(EdiError::NotEdi)
}

/// The `UNA` service string advice: nine characters naming every separator.
fn discover_una(input: &[u8]) -> Result<Delimiters> {
    if input.len() < 9 {
        return Err(EdiError::BadDelimiters {
            reason: "the UNA service string advice is shorter than 9 characters".into(),
        });
    }
    let component = input[3] as char;
    let element = input[4] as char;
    let decimal = input[5] as char;
    let release = input[6] as char;
    let reserved = input[7] as char;
    let terminator = input[8] as char;
    let repetition = (reserved != ' ').then_some(reserved);
    let delimiters = Delimiters {
        element,
        component,
        repetition,
        release: Some(release),
        terminator,
        decimal: Some(decimal),
        una: true,
    };
    check_distinct(delimiters)?;
    Ok(delimiters)
}

/// The X12 `ISA`: a fixed-width record whose separators sit at known offsets.
fn discover_isa(input: &[u8]) -> Result<Delimiters> {
    if input.len() < 106 {
        return Err(EdiError::BadDelimiters {
            reason: format!(
                "the ISA segment is {} bytes, but X12 fixes it at 106",
                input.len()
            ),
        });
    }
    let element = input[3] as char;
    let repetition = match input[82] as char {
        ' ' => None,
        c => Some(c),
    };
    let component = input[104] as char;
    let terminator = input[105] as char;
    let delimiters = Delimiters::x12(element, component, repetition, terminator);
    check_distinct(delimiters)?;
    Ok(delimiters)
}

/// EDIFACT without a `UNA`: the element separator is the fourth byte of `UNB`, the component
/// separator is the first non-alphanumeric character of `UNB01` (for example the `:` in
/// `UNOA:1`).
fn discover_unb(input: &[u8]) -> Result<Delimiters> {
    if input.len() < 4 {
        return Err(EdiError::BadDelimiters {
            reason: "the UNB segment is shorter than 4 characters".into(),
        });
    }
    let element = input[3] as char;
    let mut defaults = Delimiters::edifact_defaults();
    defaults.element = element;
    let rest = &input[4..];
    let end = rest
        .iter()
        .position(|&b| b as char == element || b as char == defaults.terminator)
        .unwrap_or(rest.len());
    let component = rest[..end]
        .iter()
        .map(|&b| b as char)
        .find(|c| !c.is_ascii_alphanumeric())
        .unwrap_or(defaults.component);
    defaults.component = component;
    check_distinct(defaults)?;
    Ok(defaults)
}

/// Refuses delimiters where two separators are the same character (an unreadable document).
fn check_distinct(delimiters: Delimiters) -> Result<()> {
    let seps = delimiters.separators();
    for (i, a) in seps.iter().enumerate() {
        if seps[i + 1..].contains(a) {
            return Err(EdiError::BadDelimiters {
                reason: format!("the separator {a:?} is used for more than one purpose"),
            });
        }
    }
    Ok(())
}
