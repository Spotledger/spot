//! The reverse direction of a v3 source map: from the TypeScript a breakpoint names to the
//! generated JavaScript lines that can host it, and back for a paused frame.
//!
//! `spot-ts` reads maps generated → original (for an exception position). A debugger needs the
//! other direction: `setBreakpoints` names line 7 of `on_validate.ts`, and the injected hooks live
//! on lines of the generated file. This module decodes the map once and indexes both ways.

/// One mapping segment.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    generated_line: u32,
    generated_column: u32,
    source: usize,
    line: u32,
    column: u32,
}

/// A position in the original TypeScript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OriginalPosition {
    /// The source the map names.
    pub source: String,
    /// The line, counted from 1.
    pub line: u32,
    /// The column, counted from 1.
    pub column: u32,
}

/// A parsed source map, indexed generated → original and original line → generated lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remap {
    sources: Vec<String>,
    segments: Vec<Segment>,
    /// No map was supplied: line `N` maps to line `N`.
    identity: bool,
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn digit(byte: u8) -> Option<i64> {
    BASE64
        .iter()
        .position(|b| *b == byte)
        .and_then(|position| i64::try_from(position).ok())
}

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

impl Remap {
    /// The identity mapping, for a script with no source map: generated lines are source lines.
    #[must_use]
    pub fn identity(source: &str) -> Self {
        Self {
            sources: vec![source.to_owned()],
            segments: Vec::new(),
            identity: true,
        }
    }

    /// Parses a v3 map.
    ///
    /// # Errors
    ///
    /// A message naming what is wrong with the map (not JSON, no `sources`, no `mappings`, a bad
    /// segment).
    pub fn parse(text: &str) -> std::result::Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let sources: Vec<String> = value
            .get("sources")
            .and_then(|sources| sources.as_array())
            .ok_or("no `sources` array")?
            .iter()
            .map(|source| source.as_str().unwrap_or("").to_owned())
            .collect();
        let mappings = value
            .get("mappings")
            .and_then(|mappings| mappings.as_str())
            .ok_or("no `mappings` string")?;
        let mut segments = Vec::new();
        let (mut source, mut line, mut column) = (0_i64, 0_i64, 0_i64);
        for (generated_line, group) in mappings.split(';').enumerate() {
            let mut generated_column = 0_i64;
            for part in group.split(',').filter(|part| !part.is_empty()) {
                let bytes = part.as_bytes();
                let mut at = 0;
                let bad = || format!("bad mapping segment `{part}`");
                generated_column += vlq(bytes, &mut at).ok_or_else(bad)?;
                if at >= bytes.len() {
                    continue;
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
        segments.sort_by_key(|segment| (segment.generated_line, segment.generated_column));
        Ok(Self {
            sources,
            segments,
            identity: false,
        })
    }

    /// The map's sources.
    #[must_use]
    pub fn sources(&self) -> &[String] {
        &self.sources
    }

    /// The original position of a generated line: the first segment on that line (its column is
    /// the line's first mapped HIT; a breakpoint on the line means the line).
    #[must_use]
    pub fn original(&self, generated_line: u32) -> Option<OriginalPosition> {
        if self.identity {
            return Some(OriginalPosition {
                source: self.sources.first().cloned().unwrap_or_default(),
                line: generated_line,
                column: 1,
            });
        }
        let zero = generated_line.checked_sub(1)?;
        let first = self
            .segments
            .iter()
            .find(|segment| segment.generated_line == zero)?;
        Some(OriginalPosition {
            source: self.sources.get(first.source)?.clone(),
            line: first.line + 1,
            column: first.column + 1,
        })
    }

    /// The generated lines a breakpoint on `line` (1-based) of `source_path` can be set on.
    ///
    /// `source_path` is matched by file name (the map carries relative names); `None` matches any
    /// source. With the identity mapping, the line itself. The result is sorted and deduplicated.
    #[must_use]
    pub fn generated_lines(&self, source_path: Option<&str>, line: u32) -> Vec<u32> {
        let zero = match line.checked_sub(1) {
            Some(zero) => zero,
            None => return Vec::new(),
        };
        if self.identity {
            return vec![line];
        }
        let wanted = source_path.and_then(file_name);
        let mut lines: Vec<u32> = self
            .segments
            .iter()
            .filter(|segment| segment.line == zero)
            .filter(
                |segment| match (&wanted, self.sources.get(segment.source)) {
                    (Some(wanted), Some(source)) => {
                        file_name(source).as_deref() == Some(wanted.as_str())
                    }
                    _ => true,
                },
            )
            .filter_map(|segment| segment.generated_line.checked_add(1))
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }
}

fn file_name(path: &str) -> Option<String> {
    path.rsplit(['/', '\\']).next().map(str::to_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Remap;

    /// A map where generated line 1 columns 0 and 4 are TS line 1, generated line 2 is TS line 2,
    /// and generated line 3 has no mapping.
    const MAP: &str =
        r#"{"version":3,"sources":["scripts/on_validate.ts"],"mappings":"AAAA,EAAE;AACA"}"#;

    #[test]
    fn generated_positions_map_back_to_the_typescript() {
        let map = Remap::parse(MAP).expect("map");
        assert_eq!(map.original(1).expect("line 1").line, 1);
        assert_eq!(map.original(2).expect("line 2").line, 2);
        assert!(map.original(3).is_none());
        assert!(map.original(0).is_none());
        assert_eq!(map.sources(), ["scripts/on_validate.ts"]);
    }

    #[test]
    fn a_breakpoint_finds_every_generated_line_of_its_source_line() {
        let map = Remap::parse(MAP).expect("map");
        assert_eq!(
            map.generated_lines(Some("scripts/on_validate.ts"), 1),
            vec![1]
        );
        assert_eq!(
            map.generated_lines(Some("another/name.ts"), 2),
            Vec::<u32>::new()
        );
        assert_eq!(map.generated_lines(None, 2), vec![2]);
        assert_eq!(map.generated_lines(None, 0), Vec::<u32>::new());
        assert_eq!(map.generated_lines(None, 99), Vec::<u32>::new());
    }

    #[test]
    fn an_identity_map_keeps_line_numbers() {
        let map = Remap::identity("scripts/a.ts");
        assert_eq!(map.generated_lines(Some("scripts/a.ts"), 7), vec![7]);
        assert_eq!(map.original(7).expect("line").line, 7);
        assert!(Remap::parse("nope").is_err());
        assert!(Remap::parse(r#"{"sources":[],"mappings":"!!!"}"#).is_err());
    }
}
