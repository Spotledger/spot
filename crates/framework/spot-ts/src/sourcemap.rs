//! Source maps (version 3), enough to map a position in the emitted JavaScript back to the
//! TypeScript the author wrote. The transpiler (WP-EXT-08.T2) produces the maps; this reads them.

use crate::error::SourcePosition;

/// One mapping: a generated position and the original position it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    generated_line: u32,
    generated_column: u32,
    source: usize,
    line: u32,
    column: u32,
}

/// A parsed source map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceMap {
    sources: Vec<String>,
    /// Sorted by generated position.
    segments: Vec<Segment>,
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn digit(c: u8) -> Option<i64> {
    BASE64
        .iter()
        .position(|b| *b == c)
        .and_then(|p| i64::try_from(p).ok())
}

/// Decodes one base64 VLQ number from `bytes`, advancing `at`.
fn vlq(bytes: &[u8], at: &mut usize) -> Option<i64> {
    let (mut value, mut shift) = (0_i64, 0_u32);
    loop {
        let d = digit(*bytes.get(*at)?)?;
        *at += 1;
        value |= (d & 31).checked_shl(shift)?;
        if d & 32 == 0 {
            break;
        }
        shift += 5;
        if shift > 60 {
            return None;
        }
    }
    let magnitude = value >> 1;
    Some(if value & 1 == 1 {
        -magnitude
    } else {
        magnitude
    })
}

impl SourceMap {
    /// Parses a source map from its JSON text. `Err` names what is wrong.
    pub fn parse(json: &str) -> Result<SourceMap, String> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|e| format!("not JSON: {e}"))?;
        let sources: Vec<String> = value
            .get("sources")
            .and_then(|s| s.as_array())
            .ok_or("no `sources` array")?
            .iter()
            .map(|s| s.as_str().unwrap_or("").to_owned())
            .collect();
        let mappings = value
            .get("mappings")
            .and_then(|m| m.as_str())
            .ok_or("no `mappings` string")?;
        let mut segments = Vec::new();
        let (mut source, mut line, mut column) = (0_i64, 0_i64, 0_i64);
        for (generated_line, group) in mappings.split(';').enumerate() {
            let mut generated_column = 0_i64;
            for segment in group.split(',').filter(|s| !s.is_empty()) {
                let bytes = segment.as_bytes();
                let mut at = 0;
                let bad = || format!("bad mapping segment `{segment}`");
                generated_column += vlq(bytes, &mut at).ok_or_else(bad)?;
                if at >= bytes.len() {
                    continue; // a segment with no source: nothing to map to
                }
                source += vlq(bytes, &mut at).ok_or_else(bad)?;
                line += vlq(bytes, &mut at).ok_or_else(bad)?;
                column += vlq(bytes, &mut at).ok_or_else(bad)?;
                segments.push(Segment {
                    generated_line: u32::try_from(generated_line).map_err(|_| bad())?,
                    generated_column: u32::try_from(generated_column).map_err(|_| bad())?,
                    source: usize::try_from(source).map_err(|_| bad())?,
                    line: u32::try_from(line).map_err(|_| bad())?,
                    column: u32::try_from(column).map_err(|_| bad())?,
                });
            }
        }
        segments.sort_by_key(|s| (s.generated_line, s.generated_column));
        Ok(SourceMap { sources, segments })
    }

    /// The original position of the generated position `line`:`column` (both counted from 1): the
    /// segment that starts at or before it on that line.
    pub fn lookup(&self, line: u32, column: u32) -> Option<SourcePosition> {
        let (line, column) = (line.checked_sub(1)?, column.saturating_sub(1));
        let after = self
            .segments
            .partition_point(|s| (s.generated_line, s.generated_column) <= (line, column));
        let segment = self.segments.get(after.checked_sub(1)?)?;
        if segment.generated_line != line {
            return None;
        }
        Some(SourcePosition {
            source: self.sources.get(segment.source)?.clone(),
            line: segment.line + 1,
            column: segment.column + 1,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_standard_example() {
        // From the source map specification's examples: `AAAA` is (0,0,0,0); `CAAC` moves one
        // column in both; `;AACA` is the next generated line, one source line down.
        let map =
            SourceMap::parse(r#"{"version":3,"sources":["a.ts"],"mappings":"AAAA,CAAC;AACA"}"#)
                .unwrap();
        let at = |l, c| map.lookup(l, c);
        assert_eq!(
            at(1, 1).unwrap(),
            SourcePosition {
                source: "a.ts".into(),
                line: 1,
                column: 1
            }
        );
        assert_eq!(
            at(1, 2).unwrap(),
            SourcePosition {
                source: "a.ts".into(),
                line: 1,
                column: 2
            }
        );
        // Inside the second segment: it still applies.
        assert_eq!(at(1, 9).unwrap().column, 2);
        assert_eq!(
            at(2, 1).unwrap(),
            SourcePosition {
                source: "a.ts".into(),
                line: 2,
                column: 2
            }
        );
        // Nothing on a line with no mappings, and nothing before the first segment.
        assert!(at(3, 1).is_none());
        assert!(at(0, 1).is_none());
    }

    #[test]
    fn negative_deltas_and_bad_input() {
        // `CCAA` moves the generated column and the source index by one: the second source file.
        let map =
            SourceMap::parse(r#"{"sources":["a.ts","b.ts"],"mappings":"AAAA,CCAA"}"#).unwrap();
        assert_eq!(map.lookup(1, 2).unwrap().source, "b.ts");
        // A negative delta (`D` is minus one) back to the first source.
        let back =
            SourceMap::parse(r#"{"sources":["a.ts","b.ts"],"mappings":"AAAA,CCAA,CDAA"}"#).unwrap();
        assert_eq!(back.lookup(1, 3).unwrap().source, "a.ts");
        assert!(SourceMap::parse("nope").is_err());
        assert!(SourceMap::parse(r#"{"sources":[],"mappings":"!!!"}"#).is_err());
        assert!(SourceMap::parse(r#"{"mappings":""}"#).is_err());
    }
}
