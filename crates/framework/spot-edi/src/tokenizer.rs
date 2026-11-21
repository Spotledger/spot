//! The tokenizer (WP-INT-06.T1, brief item 1).
//!
//! [`SegmentScanner`] is a byte-at-a-time state machine: feed it the bytes of a document and it
//! yields one [`Segment`] per terminator. It never holds more than the current segment in
//! memory, so a caller can stream a file through it in fixed-size chunks
//! ([`crate::parse_reader`]); it also enforces the [`Limits`]. EDIFACT's release character is
//! honoured throughout, so a released terminator or element separator never ends a segment.
//!
//! Positions: each segment records the byte offset of its first character. Splitting a segment
//! into elements, repetitions and components is done here too, so the model is built from bytes
//! once, in one pass.

use crate::delimiters::Delimiters;
use crate::error::{EdiError, Result};
use crate::limits::Limits;
use crate::model::{Element, Segment};

/// A streaming tokenizer: push bytes, receive whole segments.
#[derive(Debug)]
pub struct SegmentScanner {
    delimiters: Delimiters,
    limits: Limits,
    buf: Vec<u8>,
    started: bool,
    escaped: bool,
    offset: usize,
    pos: usize,
    segments: usize,
}

impl SegmentScanner {
    /// A scanner for `delimiters`, bounded by `limits`.
    pub fn new(delimiters: Delimiters, limits: Limits) -> Self {
        Self {
            delimiters,
            limits,
            buf: Vec::new(),
            started: false,
            escaped: false,
            offset: 0,
            pos: 0,
            segments: 0,
        }
    }

    /// Feeds one byte, returning a segment when the byte terminated one.
    pub fn push(&mut self, byte: u8) -> Result<Option<Segment>> {
        if !self.started {
            if byte == b'\r' || byte == b'\n' {
                self.pos += 1;
                return Ok(None);
            }
            self.started = true;
            self.offset = self.pos;
        }
        self.pos += 1;
        if self.escaped {
            self.buf.push(byte);
            self.escaped = false;
            return Ok(None);
        }
        if self.delimiters.release == Some(byte as char) {
            self.buf.push(byte);
            self.escaped = true;
            return Ok(None);
        }
        if byte as char == self.delimiters.terminator {
            return self.emit();
        }
        self.buf.push(byte);
        Ok(None)
    }

    /// Ends the input, returning a final segment when the document did not end with a
    /// terminator.
    pub fn finish(&mut self) -> Result<Option<Segment>> {
        if self.buf.is_empty() {
            return Ok(None);
        }
        self.emit()
    }

    /// Turns the current buffer into a segment.
    fn emit(&mut self) -> Result<Option<Segment>> {
        let bytes = std::mem::take(&mut self.buf);
        let offset = self.offset;
        self.started = false;
        self.escaped = false;
        let segment = decode_segment(&bytes, self.delimiters, offset, self.limits)?;
        match segment {
            Some(segment) => {
                self.segments += 1;
                if self.segments > self.limits.max_segments {
                    return Err(EdiError::TooManySegments {
                        limit: self.limits.max_segments,
                    });
                }
                Ok(Some(segment))
            }
            None => Ok(None),
        }
    }
}

/// Decodes one raw segment (`tag` plus elements) into a [`Segment`].
///
/// A blank segment (only whitespace, or the `UNA` service string advice) decodes to `None`.
pub fn decode_segment(
    bytes: &[u8],
    delimiters: Delimiters,
    offset: usize,
    limits: Limits,
) -> Result<Option<Segment>> {
    let element_sep = delimiters.element as u8;
    let release = delimiters.release.map(|c| c as u8);
    let (tag_bytes, rest) = match split_first_unescaped(bytes, element_sep, release) {
        Some((tag, rest)) => (tag, Some(rest)),
        None => (bytes, None),
    };
    let tag = ascii(tag_bytes)?;
    if tag == "UNA" || tag.starts_with("UNA:") || tag.trim().is_empty() {
        return Ok(None);
    }
    let mut segment = Segment::new(tag);
    segment.offset = offset;
    if let Some(rest) = rest {
        let tokens = split_unescaped(rest, element_sep, release);
        if tokens.len() > limits.max_elements {
            return Err(EdiError::TooManyElements {
                tag: segment.tag,
                limit: limits.max_elements,
            });
        }
        for token in tokens {
            segment.elements.push(decode_element(token, delimiters)?);
        }
    }
    Ok(Some(segment))
}

/// Decodes one element token into a simple, composite or repeated element.
fn decode_element(token: &[u8], delimiters: Delimiters) -> Result<Element> {
    let release = delimiters.release.map(|c| c as u8);
    if let Some(repetition) = delimiters.repetition {
        let parts = split_unescaped(token, repetition as u8, release);
        if parts.len() > 1 {
            let mut items = Vec::with_capacity(parts.len());
            for part in parts {
                items.push(decode_composite(part, delimiters)?);
            }
            return Ok(Element::Repeated(items));
        }
    }
    decode_composite(token, delimiters)
}

/// Decodes one component-bearing token into a simple or composite element.
fn decode_composite(token: &[u8], delimiters: Delimiters) -> Result<Element> {
    let release = delimiters.release.map(|c| c as u8);
    let parts = split_unescaped(token, delimiters.component as u8, release);
    if parts.len() > 1 {
        let mut components = Vec::with_capacity(parts.len());
        for part in parts {
            components.push(unescape(part, release)?);
        }
        Ok(Element::Composite(components))
    } else {
        Ok(Element::Simple(unescape(token, release)?))
    }
}

/// Splits `bytes` at the first unescaped occurrence of `sep`, returning the part before it and
/// the part after it.
fn split_first_unescaped(bytes: &[u8], sep: u8, release: Option<u8>) -> Option<(&[u8], &[u8])> {
    let mut escaped = false;
    for (i, &byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if release == Some(byte) {
            escaped = true;
            continue;
        }
        if byte == sep {
            return Some((&bytes[..i], &bytes[i + 1..]));
        }
    }
    None
}

/// Splits `bytes` at every unescaped occurrence of `sep`.
fn split_unescaped(bytes: &[u8], sep: u8, release: Option<u8>) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut escaped = false;
    for (i, &byte) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if release == Some(byte) {
            escaped = true;
            continue;
        }
        if byte == sep {
            out.push(&bytes[start..i]);
            start = i + 1;
        }
    }
    out.push(&bytes[start..]);
    out
}

/// Removes release sequences from a token and decodes it as ASCII.
fn unescape(bytes: &[u8], release: Option<u8>) -> Result<String> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut escaped = false;
    for &byte in bytes {
        if escaped {
            out.push(byte);
            escaped = false;
        } else if release == Some(byte) {
            escaped = true;
        } else {
            out.push(byte);
        }
    }
    if escaped {
        // A trailing release character with nothing to release is kept literally.
        out.push(release.unwrap_or(b'?'));
    }
    ascii(&out)
}

/// Decodes bytes as ASCII, failing with the offset of the first offending byte.
pub(crate) fn ascii(bytes: &[u8]) -> Result<String> {
    match std::str::from_utf8(bytes) {
        Ok(text) if text.is_ascii() => Ok(text.to_owned()),
        Ok(text) => Err(EdiError::BadEncoding {
            offset: text
                .char_indices()
                .find(|(_, c)| !c.is_ascii())
                .map(|(i, _)| i)
                .unwrap_or(0),
        }),
        Err(_) => Err(EdiError::BadEncoding { offset: 0 }),
    }
}

/// Tokenizes a whole byte slice into segments under `limits`.
pub fn segments(input: &[u8], delimiters: Delimiters, limits: Limits) -> Result<Vec<Segment>> {
    let mut scanner = SegmentScanner::new(delimiters, limits);
    let mut out = Vec::new();
    for &byte in input {
        if let Some(segment) = scanner.push(byte)? {
            out.push(segment);
        }
    }
    if let Some(segment) = scanner.finish()? {
        out.push(segment);
    }
    Ok(out)
}
