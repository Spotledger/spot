//! The canonical writer (WP-INT-06.T1, brief item 3).
//!
//! [`write`] renders an [`Interchange`] back to text. The output is canonical: trailing empty
//! elements are trimmed ([`WriteOptions::trim_trailing_elements`]), the X12 `ISA` is padded to
//! its fixed widths, EDIFACT values are escaped with the release character, and a `UNA` service
//! string advice is emitted only when the interchange carried one. Parsing a canonical document
//! and writing it again is byte-for-byte identical.

use crate::delimiters::{Delimiters, Syntax};
use crate::error::{EdiError, Result};
use crate::model::{Element, Interchange, Segment};

/// The widths of the sixteen X12 `ISA` elements, in order.
const ISA_WIDTHS: [usize; 16] = [2, 10, 2, 10, 2, 15, 2, 15, 6, 4, 1, 5, 9, 1, 1, 1];

/// How the writer renders a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteOptions {
    /// Whether trailing empty elements are removed from each segment.
    pub trim_trailing_elements: bool,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            trim_trailing_elements: true,
        }
    }
}

/// Renders `interchange` to text.
pub fn write(interchange: &Interchange) -> Result<String> {
    write_with(interchange, WriteOptions::default())
}

/// Renders `interchange` to text under `options`.
pub fn write_with(interchange: &Interchange, options: WriteOptions) -> Result<String> {
    let mut out = String::new();
    if interchange.syntax == Syntax::Edifact && interchange.delimiters.una {
        out.push_str(&una_line(interchange.delimiters));
    }
    for segment in interchange.all_segments() {
        out.push_str(&write_segment(
            segment,
            interchange.syntax,
            interchange.delimiters,
            options,
        )?);
    }
    Ok(out)
}

/// Renders a list of segments to text (no `UNA`), for a message or a fragment.
pub fn write_segments(
    segments: &[Segment],
    syntax: Syntax,
    delimiters: Delimiters,
    options: WriteOptions,
) -> Result<String> {
    let mut out = String::new();
    for segment in segments {
        out.push_str(&write_segment(segment, syntax, delimiters, options)?);
    }
    Ok(out)
}

/// Renders one segment, terminator included.
pub fn write_segment(
    segment: &Segment,
    syntax: Syntax,
    delimiters: Delimiters,
    options: WriteOptions,
) -> Result<String> {
    if syntax == Syntax::X12 && segment.tag == "ISA" {
        return write_isa(segment, delimiters);
    }
    let mut elements = segment.elements.clone();
    if options.trim_trailing_elements {
        while elements.last().is_some_and(Element::is_empty) {
            elements.pop();
        }
    }
    let mut out = String::new();
    out.push_str(&segment.tag);
    for element in &elements {
        out.push(delimiters.element);
        out.push_str(&write_element(element, delimiters)?);
    }
    out.push(delimiters.terminator);
    Ok(out)
}

/// The `UNA` service string advice, exactly nine characters.
fn una_line(delimiters: Delimiters) -> String {
    let mut out = String::from("UNA");
    out.push(delimiters.component);
    out.push(delimiters.element);
    out.push(delimiters.decimal.unwrap_or('.'));
    out.push(delimiters.release.unwrap_or('?'));
    out.push(delimiters.repetition.unwrap_or(' '));
    out.push(delimiters.terminator);
    out
}

/// Renders one element: a simple value, a composite, or a repetition of either.
fn write_element(element: &Element, delimiters: Delimiters) -> Result<String> {
    Ok(match element {
        Element::Simple(value) => escape(value, delimiters),
        Element::Composite(parts) => {
            let mut out = String::new();
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    out.push(delimiters.component);
                }
                out.push_str(&escape(part, delimiters));
            }
            out
        }
        Element::Repeated(items) => {
            let Some(repetition) = delimiters.repetition else {
                return Err(EdiError::BadSegment {
                    reason:
                        "a repeated element is present but the document has no repetition separator"
                            .into(),
                });
            };
            let mut out = String::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(repetition);
                }
                out.push_str(&write_element(item, delimiters)?);
            }
            out
        }
    })
}

/// Escapes the release, element, component, repetition and terminator characters in `value`.
///
/// X12 has no release character, so nothing is escaped there; EDIFACT prefixes each separator
/// with the release character.
fn escape(value: &str, delimiters: Delimiters) -> String {
    let Some(release) = delimiters.release else {
        return value.to_owned();
    };
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch == release
            || ch == delimiters.element
            || ch == delimiters.component
            || ch == delimiters.terminator
            || Some(ch) == delimiters.repetition
        {
            out.push(release);
        }
        out.push(ch);
    }
    out
}

/// Renders the fixed-width X12 `ISA` segment.
fn write_isa(segment: &Segment, delimiters: Delimiters) -> Result<String> {
    if segment.elements.len() > ISA_WIDTHS.len() {
        return Err(EdiError::BadSegment {
            reason: format!(
                "the ISA segment has {} elements, but X12 fixes it at 16",
                segment.elements.len()
            ),
        });
    }
    let mut values: Vec<String> = Vec::with_capacity(ISA_WIDTHS.len());
    for (i, width) in ISA_WIDTHS.iter().enumerate() {
        let text = segment
            .elements
            .get(i)
            .and_then(Element::first)
            .unwrap_or("")
            .to_owned();
        values.push(pad_isa(i, &text, *width, delimiters)?);
    }
    let mut out = String::from("ISA");
    for value in &values {
        out.push(delimiters.element);
        out.push_str(value);
    }
    out.push(delimiters.terminator);
    Ok(out)
}

/// Pads one `ISA` element to its fixed width.
fn pad_isa(index: usize, text: &str, width: usize, delimiters: Delimiters) -> Result<String> {
    let text = if text.is_empty() {
        match index {
            10 => delimiters
                .repetition
                .map(|c| c.to_string())
                .unwrap_or_default(),
            15 => delimiters.component.to_string(),
            _ => String::new(),
        }
    } else {
        text.to_owned()
    };
    if text.chars().count() > width {
        return Err(EdiError::BadSegment {
            reason: format!(
                "ISA element {} is {} characters, over its fixed width of {width}",
                index + 1,
                text.chars().count()
            ),
        });
    }
    let padding = width - text.chars().count();
    if index == 12 {
        // The interchange control number is right-justified with zeros.
        Ok(format!("{}{text}", "0".repeat(padding)))
    } else {
        Ok(format!("{text}{}", " ".repeat(padding)))
    }
}
