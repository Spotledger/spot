//! Reading a contribution file: JSON (the contract's reference form) or TOML (what apps write).

use crate::code::{UiExtError, codes};
use crate::model::UiDocument;

/// The two forms the contract is written in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// The reference form of `contracts/ui-ext/examples/`.
    Json,
    /// The form apps write; the same document, one table per item.
    Toml,
}

impl Format {
    /// The format a contribution file name implies: `.toml` is TOML, everything else JSON.
    #[must_use]
    pub fn from_path(file: &str) -> Self {
        if file.to_ascii_lowercase().ends_with(".toml") {
            Self::Toml
        } else {
            Self::Json
        }
    }
}

/// Parses `text` as the format `file`'s extension implies.
///
/// # Errors
///
/// One [`UiExtError`] with code [`codes::SCHEMA`] naming the file and, where the parser knows
/// it, the line and column.
// The finding is a fat value on purpose: it is read once per install, not in a hot loop, and a
// box would only make the common `validate`/`resolve` signatures disagree.
#[allow(clippy::result_large_err)]
pub fn parse_document(file: &str, text: &str) -> Result<UiDocument, UiExtError> {
    parse_document_as(file, text, Format::from_path(file))
}

/// The largest document, in bytes, that is parsed (1 MiB). A larger one is refused before any
/// parser runs, with code [`codes::DOCUMENT_TOO_LARGE`].
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

/// Parses `text` as an explicit [`Format`].
///
/// # Errors
///
/// One [`UiExtError`] with code [`codes::SCHEMA`], or [`codes::DOCUMENT_TOO_LARGE`] when `text`
/// is over [`MAX_DOCUMENT_BYTES`].
// The finding is a fat value on purpose: it is read once per install, not in a hot loop, and a
// box would only make the common `validate`/`resolve` signatures disagree.
#[allow(clippy::result_large_err)]
pub fn parse_document_as(file: &str, text: &str, format: Format) -> Result<UiDocument, UiExtError> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(UiExtError::new(
            codes::DOCUMENT_TOO_LARGE,
            file,
            "",
            format!(
                "the document is {} bytes; the limit is {MAX_DOCUMENT_BYTES}",
                text.len()
            ),
        ));
    }
    match format {
        Format::Json => serde_json::from_str::<UiDocument>(text).map_err(|error| {
            let message = clean(&error.to_string());
            UiExtError::new(codes::SCHEMA, file, "", message)
                .with_position(error.line().max(1), error.column().max(1))
        }),
        Format::Toml => toml::from_str::<UiDocument>(text).map_err(|error| {
            let message = clean(&error.to_string());
            let mut finding = UiExtError::new(codes::SCHEMA, file, "", message);
            if let Some(span) = error.span() {
                let (line, column) = position_of(text, span.start);
                finding = finding.with_position(line, column);
            }
            finding
        }),
    }
}

/// One line of the parser's message, so a finding stays a single sentence.
///
/// `toml` prints the position first and the useful detail last; `serde_json` prints one line.
fn clean(message: &str) -> String {
    let lines: Vec<&str> = message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let first = lines
        .first()
        .copied()
        .unwrap_or("the document does not fit the schema");
    let detail = lines.iter().copied().rev().find(|line| {
        *line != first
            && !line.starts_with('|')
            && !line.starts_with('+')
            && !line.starts_with("TOML parse error")
            && !line
                .chars()
                .all(|c| matches!(c, '^' | '-' | ' ' | '|' | '0'..='9'))
    });
    let mut text = match detail {
        Some(detail) => format!("{first}: {detail}"),
        None => first.to_owned(),
    };
    if !text.ends_with('.') {
        text.push('.');
    }
    text
}

/// The 1-based line and column of a byte offset.
fn position_of(text: &str, offset: usize) -> (usize, usize) {
    let bytes = text.as_bytes();
    let end = offset.min(bytes.len());
    let mut line = 1;
    let mut column = 1;
    for byte in &bytes[..end] {
        if *byte == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line, column)
}

#[cfg(test)]
mod size_tests {
    use super::*;

    #[test]
    fn an_oversized_document_is_refused_before_parsing() {
        for format in [Format::Json, Format::Toml] {
            let text = " ".repeat(MAX_DOCUMENT_BYTES + 1);
            let result = parse_document_as("big", &text, format);
            assert_eq!(
                result.map_err(|error| error.code),
                Err(codes::DOCUMENT_TOO_LARGE)
            );
        }
    }
}
