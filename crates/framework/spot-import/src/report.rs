// The import report: up to N full entries per code plus counts, serialisable to CSV.

use std::io::Write;

/// The row-importing policy the caller chose (brief item 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportPolicy {
    /// Import nothing unless the report has no row errors at all.
    AllOrNothing,
    /// Import the parents whose rows had no errors; skip the rest.
    SkipBadRows,
}

/// One row problem, fully described (brief item 5: source row, column name and cell text).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRowError {
    /// The 1-based source row the problem came from.
    pub row: u64,
    /// The column's header text (empty when the row as a whole is at fault).
    pub column: String,
    /// The cell's raw text (empty when no single cell is at fault).
    pub cell: String,
    /// The stable `import.*` or `document.*` code.
    pub code: String,
    /// What the user can do about it.
    pub message: String,
}

/// Whether a cell would be read as a formula by a spreadsheet: its first character is `=`, `+`,
/// `-`, `@`, a tab or a carriage return, or only spaces precede such a character.
///
/// The first character is judged **before** any trimming (a tab or CR is whitespace and would
/// vanish under `trim_start`); only spaces are skipped, because Excel ignores leading spaces
/// before a formula.
fn is_formula_start(value: &str) -> bool {
    matches!(
        value.trim_start_matches(' ').chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r')
    )
}

/// Quotes a cell for a downloadable report.
///
/// Quoting (doubling embedded double quotes and wrapping in double quotes) is always applied.
/// In addition, when the cell starts with `=`, `+`, `-`, `@`, a tab or a carriage return (leading
/// spaces are skipped, the tab and CR are not), a single quote is prefixed so a spreadsheet
/// opening the report treats the value as text instead of a formula. Every writer of
/// a downloadable report must use this function, including any XLSX report writer.
fn csv_quote(value: &str) -> String {
    let escaped = value.replace('"', "\"\"");
    if is_formula_start(value) {
        format!("\"'{escaped}\"")
    } else {
        format!("\"{escaped}\"")
    }
}

/// The report the engine appends every problem to.
///
/// Capacity-bounded: the first [`ImportReport::capacity`] entries per error code are kept in
/// full; later ones only count, so a bad 100k-row file produces a small report.
#[derive(Clone, Debug)]
pub struct ImportReport {
    /// Full entries, in receive order (bounded per code).
    pub entries: Vec<ImportRowError>,
    /// Per error code: how many entries were seen and how many are kept in full.
    pub counts: std::collections::BTreeMap<String, u64>,
    /// Whole-file failures (`import.*` fatal codes), with one full entry each.
    pub fatal: Vec<crate::error::ImportError>,
    /// Full entries kept per code.
    capacity: usize,
}

impl Default for ImportReport {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            counts: std::collections::BTreeMap::new(),
            fatal: Vec::new(),
            capacity: 100,
        }
    }
}

impl ImportReport {
    /// A report keeping up to `capacity` full entries per code.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }

    /// The full entries kept per code.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Records one row problem.
    pub fn push_row_error(&mut self, entry: ImportRowError) {
        let seen = self.counts.entry(entry.code.clone()).or_default();
        if (*seen as usize) < self.capacity {
            self.entries.push(entry);
        }
        *seen += 1;
    }

    /// Records a whole-file failure.
    pub fn push_fatal(&mut self, error: crate::error::ImportError) {
        self.fatal.push(error);
    }

    /// How many problems of `code` were seen in total (kept and counted only).
    pub fn count(&self, code: &str) -> u64 {
        self.counts.get(code).copied().unwrap_or(0)
    }

    /// The first full entry of `code`, for showing the user one concrete example.
    pub fn first(&self, code: &str) -> Option<&ImportRowError> {
        self.entries.iter().find(|e| e.code == code)
    }

    /// Whether an import can proceed under `policy`.
    ///
    /// `all_or_nothing` refuses when any row problem exists; `skip_bad_rows` imports what
    /// passes validation, so only a whole-file failure (unreadable file, row past the limit)
    /// blocks it.
    pub fn is_importable(&self, policy: ImportPolicy) -> bool {
        match policy {
            ImportPolicy::AllOrNothing => self.entries.is_empty() && self.fatal.is_empty(),
            ImportPolicy::SkipBadRows => self.fatal.is_empty(),
        }
    }

    /// Writes the report as CSV: one row per problem, columns fixed.
    ///
    /// The file the user fixes and re-uploads: the original row number, column and cell text
    /// identify the problem's place in *their* file. Fatal entries (row 0, no column: the
    /// file as a whole failed) lead the list so the reader sees them first.
    pub fn to_csv<W: Write>(&self, out: &mut W) -> std::io::Result<()> {
        writeln!(out, "row,column,cell,code,message")?;
        for failure in &self.fatal {
            writeln!(
                out,
                "0,,{},{},{}",
                csv_quote(""),
                csv_quote(failure.code()),
                csv_quote(&failure.to_string())
            )?;
        }
        for entry in &self.entries {
            writeln!(
                out,
                "{},{},{},{},{}",
                entry.row,
                csv_quote(&entry.column),
                csv_quote(&entry.cell),
                csv_quote(&entry.code),
                csv_quote(&entry.message)
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    /// Recorded security finding: every dangerous first character, with and without leading spaces, is
    /// neutralised; ordinary text is only quoted.
    #[test]
    fn csv_quote_neutralises_formula_prefixes() {
        let dangerous = [
            "=1+1",
            "+1",
            "-1",
            "@SUM(A1)",
            "\tcmd",
            "\rcmd",
            "\t=1",
            "\r\n=1",
            " =1",
            "   +1",
            "  \t@x",
            "=cmd|' /C calc'!A0",
        ];
        for value in dangerous {
            let quoted = csv_quote(value);
            assert!(
                quoted.starts_with("\"'"),
                "{value:?} was not neutralised: {quoted:?}"
            );
        }
        let safe = [
            "",
            "plain",
            "a=b",
            "1+1",
            " plain",
            "x\ty",
            "'=already",
            "\"=quoted",
        ];
        for value in safe {
            let quoted = csv_quote(value);
            assert!(
                !quoted.starts_with("\"'") || value.starts_with('\''),
                "{value:?} wrongly prefixed: {quoted:?}"
            );
        }
        assert_eq!(csv_quote("\"=1"), "\"\"\"=1\"");
        assert_eq!(csv_quote("=a\"b"), "\"'=a\"\"b\"");
    }

    /// Every cell of the downloadable report goes through `csv_quote`, fatal entries included.
    #[test]
    fn every_report_column_is_neutralised() {
        let mut report = ImportReport::default();
        report.fatal.push(crate::error::ImportError::NotAWorkbook {
            path: "=evil".to_owned(),
            reason: "r".to_owned(),
        });
        report.entries.push(ImportRowError {
            row: 2,
            column: "=col".to_owned(),
            cell: "\tcell".to_owned(),
            code: "+code".to_owned(),
            message: "\r@msg".to_owned(),
        });
        let mut out = Vec::new();
        report.to_csv(&mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let last = text.lines().last().unwrap();
        assert_eq!(last, "2,\"'=col\",\"'\tcell\",\"'+code\",\"'\r@msg\"");
        assert!(!text.contains(",\"=") && !text.contains(",\"\t"));
    }
}
