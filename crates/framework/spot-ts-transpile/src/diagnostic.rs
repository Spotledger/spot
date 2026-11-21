/// One reported failure. Every diagnostic carries a stable `code` and a
/// 1-based position ([`Diagnostic::line`]..[`Diagnostic::end_line`])
/// so the author sees the problem on save, at the editor position.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Diagnostic {
    /// Stable identifier, e.g. `ts.syntax`, `ts.import_not_allowed`.
    pub code: String,
    /// Human explanation of what is wrong and what to do instead.
    pub message: String,
    /// 1-based line of the start.
    pub line: u32,
    /// 1-based column of the start (characters, so tabs count once).
    pub column: u32,
    /// 1-based line just past the end.
    pub end_line: u32,
    /// 1-based column just past the end.
    pub end_column: u32,
}

impl Diagnostic {
    /// Build a diagnostic covering the span `start..end` (byte offsets).
    pub fn spanned(code: &str, message: String, index: &LineIndex, start: u32, end: u32) -> Self {
        let (line, column) = index.position(start);
        let (end_line, end_column) = index.position(end);
        Self {
            code: code.to_string(),
            message,
            line,
            column,
            end_line,
            end_column,
        }
    }

    /// Build a diagnostic at `0:0..0:0` — for problems with the whole input
    /// (for example too large) rather than one position in it.
    pub fn whole_input(code: &str, message: String) -> Self {
        Self {
            code: code.to_string(),
            message,
            line: 0,
            column: 0,
            end_line: 0,
            end_column: 0,
        }
    }
}

/// Positions of the line starts in one source text; built once per transpile.
///
/// Lines are separated by `\n`; a `\r` before a `\n` stays on its line, so
/// positions count `\r` as part of the line ending, like most editors.
#[derive(Debug, Clone)]
pub struct LineIndex {
    line_starts: Vec<u32>,
    /// Total length of the source, to clamp positions past the end.
    source_end: u32,
}

impl LineIndex {
    /// Index one source text.
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0_u32];
        for (offset, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(offset as u32 + 1);
            }
        }
        Self {
            line_starts,
            source_end: source.len() as u32,
        }
    }

    /// 1-based (`line`, `column`) for a byte offset, clamped into the source.
    ///
    /// An end offset may sit at the very end of the source; clamping keeps it
    /// on the final line, at the column just past the last line's characters
    /// (or, after a trailing newline, on the empty final line).
    pub fn position(&self, offset: u32) -> (u32, u32) {
        let offset = offset.min(self.source_end);
        // Index of the last line start <= offset.
        let index = match self.line_starts.binary_search(&offset) {
            Ok(found) => found,
            Err(insert) => insert - 1,
        };
        let line = index as u32 + 1;
        let column = offset - self.line_starts[index] + 1;
        (line, column)
    }
}
