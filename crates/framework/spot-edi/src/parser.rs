//! The parsers (WP-INT-06.T1, brief items 1 and 2).
//!
//! [`parse`] discovers the delimiters, tokenizes the bytes and assembles an [`Interchange`]
//! whose header and trailer are the first `ISA`/`UNB` and the last `IEA`/`UNZ` segments and
//! whose body is everything between them. [`parse_reader`] does the same from any
//! [`std::io::Read`], reading the header once for discovery and then streaming the rest through
//! the [`SegmentScanner`], so memory stays bounded by the limits and never by the file.

use std::io::Read;

use crate::delimiters::{Delimiters, Syntax, discover};
use crate::error::{EdiError, Result};
use crate::limits::Limits;
use crate::model::{Interchange, Segment};
use crate::tokenizer::SegmentScanner;

/// Parses a whole document under `limits`.
///
/// Fails with [`EdiError::NotEdi`] when the input is not an interchange, [`EdiError::TooLarge`]
/// when it exceeds the byte limit, and [`EdiError::MissingEnvelope`] when a header or trailer is
/// absent. Arbitrary bytes never panic.
pub fn parse(input: &[u8], limits: Limits) -> Result<Interchange> {
    if input.len() > limits.max_bytes {
        return Err(EdiError::TooLarge {
            found: input.len(),
            limit: limits.max_bytes,
        });
    }
    let (syntax, delimiters) = discover(input)?;
    let segments = crate::tokenizer::segments(input, delimiters, limits)?;
    assemble(syntax, delimiters, segments)
}

/// Parses a document from text.
pub fn parse_str(input: &str, limits: Limits) -> Result<Interchange> {
    parse(input.as_bytes(), limits)
}

/// Parses a document from any reader, streaming it through the tokenizer.
///
/// The header is read once so the delimiters can be discovered; the rest of the stream is
/// scanned in fixed-size chunks. The reader is never read past `limits.max_bytes`.
pub fn parse_reader<R: Read>(reader: &mut R, limits: Limits) -> Result<Interchange> {
    let mut prefix = Vec::with_capacity(106);
    let mut chunk = [0u8; 4096];
    while prefix.len() < 106 {
        let want = 106 - prefix.len();
        let n = reader
            .read(&mut chunk[..want])
            .map_err(|e| EdiError::BadSegment {
                reason: format!("the input could not be read: {e}"),
            })?;
        if n == 0 {
            break;
        }
        prefix.extend_from_slice(&chunk[..n]);
    }
    let (syntax, delimiters) = discover(&prefix)?;
    let mut total = prefix.len();
    if total > limits.max_bytes {
        return Err(EdiError::TooLarge {
            found: total,
            limit: limits.max_bytes,
        });
    }
    let mut scanner = SegmentScanner::new(delimiters, limits);
    let mut segments = Vec::new();
    for &byte in &prefix {
        if let Some(segment) = scanner.push(byte)? {
            segments.push(segment);
        }
    }
    loop {
        let n = reader.read(&mut chunk).map_err(|e| EdiError::BadSegment {
            reason: format!("the input could not be read: {e}"),
        })?;
        if n == 0 {
            break;
        }
        total += n;
        if total > limits.max_bytes {
            return Err(EdiError::TooLarge {
                found: total,
                limit: limits.max_bytes,
            });
        }
        for &byte in &chunk[..n] {
            if let Some(segment) = scanner.push(byte)? {
                segments.push(segment);
            }
        }
    }
    if let Some(segment) = scanner.finish()? {
        segments.push(segment);
    }
    assemble(syntax, delimiters, segments)
}

/// Finds the header and trailer and builds the interchange.
fn assemble(syntax: Syntax, delimiters: Delimiters, segments: Vec<Segment>) -> Result<Interchange> {
    let header_tag = syntax.interchange_header();
    let trailer_tag = syntax.interchange_trailer();
    let header_index = segments.iter().position(|s| s.tag == header_tag);
    let trailer_index = segments.iter().rposition(|s| s.tag == trailer_tag);
    let (Some(header_index), Some(trailer_index)) = (header_index, trailer_index) else {
        return Err(EdiError::MissingEnvelope {
            syntax: syntax.name(),
            what: if header_index.is_none() {
                "header"
            } else {
                "trailer"
            },
        });
    };
    if trailer_index < header_index {
        return Err(EdiError::EnvelopeOrder {
            reason: format!("the {trailer_tag} segment precedes the {header_tag} segment"),
        });
    }
    let mut header = segments[header_index].clone();
    let trailer = segments[trailer_index].clone();
    let body = segments[header_index + 1..trailer_index].to_vec();
    // The header carries the source offset of the interchange; keep it on the header.
    header.offset = segments[header_index].offset;
    Ok(Interchange::new(syntax, delimiters, header, trailer, body))
}
