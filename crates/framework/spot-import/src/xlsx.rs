//! The XLSX source ([`XlsxSource`]): a streaming reader over one worksheet.
//!
//! The workbook (its ZIP index, shared strings and styles) is opened once; row streaming runs
//! through calamine's cell reader without materialising the sheet, so memory holds one row
//! plus calamine's own buffers. Number cells keep the exact digits the workbook stored: the
//! read layer is text only, and ADR-12 lives in the conversion (no float arithmetic here —
//! [`float_text`] formats, it never computes).
//!
//! - Every row has the header's width; unused columns are [`Cell::Missing`].
//! - A formula cell yields its cached value; the formula text is never read.
//! - An error cell (`#N/A` and friends) is an absent cell: a spreadsheet error is never a
//!   document value, and validation reports the field instead.
//! - Date cells are converted through integer components (`ExcelDateTime::to_ymd_hms_milli`),
//!   which honours the workbook's own date system (1900 or 1904) — no float math, no
//!   platform features enabled.
//! - Rows that hold no cell are invisible: a spreadsheet row exists only where cells exist,
//!   so Excel's used-range padding never becomes phantom rows.

use std::fs::File;
use std::path::{Path, PathBuf};

use calamine::{DataRef, ExcelDateTime, Reader, Xlsx};
use zip::ZipArchive;

use crate::error::ImportError;
use crate::source::{BoxedRows, Cell, Limits, Row, RowSource};

/// The sheet an XLSX import reads: the workbook's first, or one by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sheet {
    /// The workbook's first sheet in workbook order.
    First,
    /// The sheet with exactly this name.
    Named(String),
}

/// A floating-point cell as its text: the shortest decimal string that reads back as the
/// same `f64`.
///
/// Calamine hands numeric cells out as `f64`, so the raw digits the XML stored (`1.50`,
/// `0.00`) are already gone — `f64` has no scale. `{f}` gives Rust's shortest round-trip
/// form (`1.5`, `0`), which is the closest faithful text an `f64` can carry. No arithmetic
/// ever runs on the float (ADR-12): this is text derivation, and the conversion layer re-parses
/// the text with `Decimal::from_str_exact`, keeping the value exact (`1.5` and `1.50` are one
/// number at different scales; the field's scale check decides whether the form the file chose
/// is acceptable). A non-finite value cannot come from an XLSX numeric cell (the XML holds
/// decimal text); if one ever did, the text simply fails the decimal parse later.
fn float_text(f: f64) -> String {
    format!("{f}")
}

/// Counts shared-string entries (`<si>` elements) across read chunks without holding the table.
#[derive(Default)]
struct SiCounter {
    /// The last bytes of the previous chunk, so a tag split across chunks is still seen.
    tail: Vec<u8>,
    count: usize,
}

impl SiCounter {
    fn feed(&mut self, chunk: &[u8]) {
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(chunk);
        let mut i = 0;
        while i + 3 < window.len() {
            if &window[i..i + 3] == b"<si"
                && matches!(window[i + 3], b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n')
            {
                self.count += 1;
                i += 3;
            } else {
                i += 1;
            }
        }
        let keep = window.len().min(3);
        self.tail = window[window.len() - keep..].to_vec();
    }
}

/// Inflates the workbook's zip archive into a sink, counting the bytes **actually produced**.
///
/// Declared sizes in the central directory are attacker-controlled and are never trusted: every
/// entry is read through `take(remaining + 1)`, so inflation stops one byte past the cap. The
/// total decompressed bytes, the shared-string table's bytes and entry count, the entry count
/// and the compression ratio (counted bytes over the real file size) are all judged on counted
/// bytes. Memory stays one fixed buffer; time is bounded by the caps.
pub(crate) fn check_xlsx_zip(path: &Path, limits: &Limits) -> Result<(), ImportError> {
    use std::io::Read;
    let not_a_workbook = |reason: String| ImportError::NotAWorkbook {
        path: path.display().to_string(),
        reason,
    };
    let file = File::open(path).map_err(|e| ImportError::FileNotFound {
        path: path.display().to_string(),
        io: e.to_string(),
    })?;
    let file_len = file.metadata().map(|m| m.len()).unwrap_or(0).max(1);
    let mut archive = ZipArchive::new(file).map_err(|e| not_a_workbook(e.to_string()))?;
    if archive.len() > limits.max_zip_entries {
        return Err(not_a_workbook(format!(
            "the workbook holds {} zip entries, over the {} limit",
            archive.len(),
            limits.max_zip_entries
        )));
    }
    // The ratio ceiling on the whole archive: counted bytes against the real file size.
    let ratio_ceiling = file_len.saturating_mul(limits.max_compression_ratio);
    let total_cap = limits.max_decompressed_bytes.min(ratio_ceiling);
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| not_a_workbook(e.to_string()))?;
        let is_shared_strings = entry
            .name()
            .is_ok_and(|name| name == "xl/sharedStrings.xml");
        let mut counter = SiCounter::default();
        let mut table_bytes: u64 = 0;
        let mut reader = entry.take(total_cap.saturating_sub(total).saturating_add(1));
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| not_a_workbook(e.to_string()))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > total_cap {
                let ratio_hit =
                    total_cap == ratio_ceiling && ratio_ceiling < limits.max_decompressed_bytes;
                return Err(ImportError::FileTooLarge {
                    found: total,
                    limit: if ratio_hit {
                        ratio_ceiling
                    } else {
                        limits.max_decompressed_bytes
                    },
                });
            }
            if is_shared_strings {
                table_bytes += n as u64;
                if table_bytes > limits.max_shared_string_bytes {
                    return Err(ImportError::FileTooLarge {
                        found: table_bytes,
                        limit: limits.max_shared_string_bytes,
                    });
                }
                counter.feed(&buf[..n]);
                if counter.count > limits.max_shared_strings {
                    return Err(not_a_workbook(format!(
                        "the shared-string table holds more than {} entries",
                        limits.max_shared_strings
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Opens an XLSX workbook from a file, mapping calamine's failures onto [`ImportError`].
fn open_xlsx(file: File, context: &str) -> Result<Xlsx<File>, ImportError> {
    Xlsx::new(file).map_err(|e| ImportError::NotAWorkbook {
        path: context.to_owned(),
        reason: e.to_string(),
    })
}

/// A fresh workbook over `path`: a new file handle and a new zip archive, so the sheet's XML
/// stream starts at its beginning no matter what any earlier reader left behind.
fn reopen_workbook(path: &Path) -> Result<Xlsx<File>, ImportError> {
    let file = File::open(path).map_err(|e| ImportError::FileNotFound {
        path: path.display().to_string(),
        io: e.to_string(),
    })?;
    open_xlsx(file, &path.display().to_string())
}

/// A streaming XLSX reader ([`RowSource`]).
///
/// The source stores its **path** and the resolved sheet name. Every pass over the sheet —
/// the header at open, the data-row stream — opens a *fresh* workbook over a *fresh file
/// handle* ([`reopen_workbook`]): calamine's cell reader continues the workbook's XML stream,
/// so a reader dropped mid-stream (the header pass stops after one row) leaves the open
/// workbook's stream position unusable for a second reader. Reopening from the path starts
/// the XML at the sheet's beginning every time, and no workbook state is ever shared between
/// passes. The workbook field exists only so the stream's reader has something to borrow;
/// each `rows()` replaces it before borrowing.
pub struct XlsxSource {
    path: PathBuf,
    workbook: Xlsx<File>,
    sheet: String,
    headers: Vec<String>,
    width: usize,
    size: u64,
    limits: Limits,
}

impl std::fmt::Debug for XlsxSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XlsxSource")
            .field("sheet", &self.sheet)
            .field("headers", &self.headers)
            .field("width", &self.width)
            .field("size", &self.size)
            .finish()
    }
}

impl XlsxSource {
    /// Opens the workbook and reads the header from `sheet`.
    pub fn open_sheet(path: &Path, sheet: &Sheet, limits: &Limits) -> Result<Self, ImportError> {
        let size = crate::error::file_size(path)?;
        crate::error::check_file_size(size, limits.max_file_size)?;
        // Guard against zip bombs and oversized shared-string tables before calamine reads the
        // workbook: inflate every entry through a counting, capped reader (declared sizes are
        // never trusted).
        check_xlsx_zip(path, limits)?;
        let file = File::open(path).map_err(|e| ImportError::FileNotFound {
            path: path.display().to_string(),
            io: e.to_string(),
        })?;
        let workbook = open_xlsx(file, &path.display().to_string())?;
        let names = workbook.sheet_names();
        if names.is_empty() {
            return Err(ImportError::NotAWorkbook {
                path: path.display().to_string(),
                reason: "the workbook holds no sheets".to_owned(),
            });
        }
        let sheet_name = match sheet {
            Sheet::First => names[0].clone(),
            Sheet::Named(name) => match names.iter().find(|n| n.as_str() == name) {
                Some(found) => found.clone(),
                None => {
                    return Err(ImportError::UnknownSheet {
                        name: name.clone(),
                        available: names.join(", "),
                    });
                }
            },
        };
        let mut source = Self {
            path: path.to_owned(),
            workbook,
            sheet: sheet_name,
            headers: Vec::new(),
            width: 0,
            size,
            limits: limits.clone(),
        };
        // The header row: one collection pass over a fresh workbook, then the reader drops (its
        // borrow ends with the pass). A data-stream opened later reopens the workbook from the
        // path, so it never touches this pass's mid-stream XML position.
        source.headers = source.header_row()?;
        source.width = source.headers.len();
        Ok(source)
    }

    /// Reads the first row that holds any cell of the sheet; `Err(NoHeader)` when none does.
    fn header_row(&mut self) -> Result<Vec<String>, ImportError> {
        match self.open_and_pull()? {
            Some(row) => {
                let texts: Vec<String> = row
                    .cells
                    .iter()
                    .map(Cell::as_str)
                    .map(str::to_owned)
                    .collect();
                Ok(texts)
            }
            None => Err(ImportError::NoHeader),
        }
    }

    /// One full row from a fresh workbook over the source's path ([`reopen_workbook`]): the
    /// header pass only, stopped after one row. Nothing is shared with any other pass, which
    /// is why the pass can drop its reader mid-stream without breaking the next one.
    fn open_and_pull(&mut self) -> Result<Option<Row>, ImportError> {
        let mut workbook = reopen_workbook(&self.path)?;
        let limits = self.limits.clone();
        let reader = cell_reader(&mut workbook, &self.sheet)?;
        let mut collector = Collector::new(reader);
        Ok(collector
            .next_row(&limits)?
            .map(|(number, cells)| Row { number, cells }))
    }
}

/// A fresh cell reader over `sheet` of `workbook` (stream order, cached formula values).
fn cell_reader<'a>(
    workbook: &'a mut Xlsx<File>,
    sheet: &str,
) -> Result<calamine::XlsxCellReader<'a, File>, ImportError> {
    workbook
        .worksheet_cells_reader(sheet)
        .map_err(|e| ImportError::NotAWorkbook {
            path: String::new(),
            reason: e.to_string(),
        })
}

/// One calamine cell's text: `None` for an empty or error cell.
fn cell_text(value: &DataRef<'_>) -> Option<String> {
    match value {
        DataRef::Empty | DataRef::Error(_) => None,
        DataRef::String(s) => Some(s.clone()),
        DataRef::SharedString(s) => Some((*s).to_owned()),
        DataRef::Bool(b) => Some(if *b { "1" } else { "0" }.to_owned()),
        DataRef::Int(i) => Some(i.to_string()),
        // Numbers and date serials keep every digit the workbook stored; a date's meaning is
        // decided per field later, from the components ExcelDateTime already resolved.
        DataRef::Float(f) => Some(float_text(*f)),
        DataRef::DateTime(dt) => Some(serial_text(*dt)),
        DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => Some(s.clone()),
    }
}

/// A date serial as text: `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM:SS(.mmm)` with a time of day —
/// every digit the workbook stored, through integer components, never a float rounding.
fn serial_text(dt: ExcelDateTime) -> String {
    let (year, month, day, hour, minute, second, milli) = dt.to_ymd_hms_milli();
    if hour == 0 && minute == 0 && second == 0 && milli == 0 {
        format!("{year:04}-{month:02}-{day:02}")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milli:03}")
    }
}

/// One row collection pass over a borrowed cell reader: a row is complete when the first cell
/// of a later row arrives — that cell is pushed back, and the next call starts from it.
///
/// The reader **must not be touched after it returned `Ok(None)`**: calamine's
/// `XlsxCellReader::next_cell` fails with `Unexpected end of xml` on every call past the end
/// (probed on calamine 0.36). [`Collector::next_row`] therefore stops pulling the moment the
/// reader says the sheet is over, and a later call sees [`Collector::drained`] and returns
/// `None` directly.
struct Collector<'a> {
    reader: calamine::XlsxCellReader<'a, File>,
    /// The cell of the *next* row, read while the last complete row was collected.
    pushed: Option<calamine::Cell<DataRef<'a>>>,
    /// The reader reached the sheet's end; every later call returns `None` without it.
    drained: bool,
}

impl<'a> Collector<'a> {
    /// A collector over a fresh cell reader.
    fn new(reader: calamine::XlsxCellReader<'a, File>) -> Self {
        Self {
            reader,
            pushed: None,
            drained: false,
        }
    }

    /// Pulls the next cell, honouring the one pushed back by the previous row.
    fn next_cell(&mut self) -> Option<Result<calamine::Cell<DataRef<'a>>, ImportError>> {
        if let Some(cell) = self.pushed.take() {
            return Some(Ok(cell));
        }
        if self.drained {
            return None;
        }
        self.next_fresh_cell()
    }

    /// Reads one cell straight from the XML stream (no pushback involved).
    fn next_fresh_cell(&mut self) -> Option<Result<calamine::Cell<DataRef<'a>>, ImportError>> {
        match self.reader.next_cell() {
            Ok(Some(c)) => Some(Ok(c)),
            Ok(None) => {
                self.drained = true;
                None
            }
            Err(e) => Some(Err(ImportError::NotAWorkbook {
                path: String::new(),
                reason: e.to_string(),
            })),
        }
    }

    /// The next row with any cell as `(1-based number, cells)`, or `None` at the sheet's end.
    ///
    /// Cells are padded with [`Cell::Missing`] between used columns. A row is complete when the
    /// first cell of a later row arrives — that cell is pushed back for the next call, which is
    /// why one reader serves the whole stream and memory holds one row.
    fn next_row(&mut self, limits: &Limits) -> Result<Option<(u64, Vec<Cell>)>, ImportError> {
        let mut current: u32 = 0;
        let mut cells: Vec<Cell> = Vec::new();
        while let Some(cell) = self.next_cell().transpose()? {
            let (row_0, col_0) = cell.get_position();
            let number = u64::from(row_0) + 1;
            if current == 0 {
                current = row_0 + 1;
            } else if row_0 + 1 != current {
                // The first cell of a later row: the previous row is complete.
                self.pushed = Some(cell);
                return Ok(Some((u64::from(current), cells)));
            }
            let col = col_0 as usize;
            if cells.len() <= col {
                cells.resize(col + 1, Cell::Missing);
            }
            if let Some(text) = cell_text(cell.get_value()) {
                let length = text.chars().count();
                if length > limits.max_cell_length {
                    return Err(ImportError::CellTooLong {
                        row: number,
                        column: (col + 1).to_string(),
                        found: length,
                        limit: limits.max_cell_length,
                    });
                }
                cells[col] = Cell::Text(text);
            }
        }
        Ok((current != 0).then(|| (u64::from(current), cells)))
    }
}

/// The XLSX row stream: one cell reader borrows the workbook for the whole stream, the header
/// row is skipped, and every row is padded to the header's width.
struct XlsxRows<'a> {
    /// `None` after the header row was skipped or the stream failed.
    collector: Option<Collector<'a>>,
    /// A reading failure to yield before anything else (a closed or corrupt workbook).
    failure: Option<ImportError>,
    width: usize,
    limits: Limits,
    rows_yielded: u64,
    /// Whether the header (the stream's first row) still needs skipping.
    skip_header: bool,
}

impl Iterator for XlsxRows<'_> {
    type Item = Result<Row, ImportError>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(e) = self.failure.take() {
            return Some(Err(e));
        }
        loop {
            let collector = self.collector.as_mut()?;
            match collector.next_row(&self.limits) {
                // End of sheet: the stream is done.
                Err(e) => {
                    self.collector = None;
                    return Some(Err(e));
                }
                Ok(None) => return None,
                Ok(Some((number, mut cells))) => {
                    if self.skip_header {
                        self.skip_header = false;
                        continue;
                    }
                    self.rows_yielded += 1;
                    if let Some(max_rows) = self.limits.max_rows
                        && self.rows_yielded > max_rows
                    {
                        self.collector = None;
                        return Some(Err(ImportError::TooManyRows { limit: max_rows }));
                    }
                    while cells.len() < self.width {
                        cells.push(Cell::Missing);
                    }
                    return Some(Ok(Row { number, cells }));
                }
            }
        }
    }
}

impl RowSource for XlsxSource {
    fn open(path: &Path, limits: &Limits) -> Result<Self, ImportError> {
        Self::open_sheet(path, &Sheet::First, limits)
    }

    fn file_size(&self) -> u64 {
        self.size
    }

    fn headers(&self) -> &[String] {
        &self.headers
    }

    fn limits(&self) -> &Limits {
        &self.limits
    }

    fn rows(&mut self) -> BoxedRows<'_> {
        // The cell reader borrows this source's *freshly reopened* workbook for the whole
        // stream: the header pass at open had its own workbook, dropped mid-stream, so a
        // reader over that one would resume a used-up XML stream ("Unexpected end of xml").
        // Reopening from the path starts the sheet's XML at the beginning; the stale field
        // workbook is replaced (never borrowed) and only serves as the struct's sink.
        // The header row was already collected at open, so the stream skips its first row.
        self.workbook = match reopen_workbook(&self.path) {
            Ok(workbook) => workbook,
            Err(e) => {
                return Box::new(std::iter::once(Err(e)).chain(std::iter::empty()));
            }
        };
        let mut rows = {
            let (limits, width) = (self.limits.clone(), self.width);
            match cell_reader(&mut self.workbook, &self.sheet) {
                Ok(reader) => XlsxRows {
                    collector: Some(Collector::new(reader)),
                    failure: None,
                    width,
                    limits,
                    rows_yielded: 0,
                    skip_header: true,
                },
                Err(e) => XlsxRows {
                    collector: None,
                    failure: Some(e),
                    width: 0,
                    limits,
                    rows_yielded: 0,
                    skip_header: false,
                },
            }
        };
        Box::new(std::iter::from_fn(move || match rows.next() {
            // A reading failure ends the stream: once the cell reader has failed (or the row or
            // cell limit fired), the XML stream position is unknown and no further row is
            // trusted. The boxed loop keeps the iterator alive but stops it here.
            Some(Err(e)) => {
                rows.collector = None;
                Some(Err(e))
            }
            other => other,
        }))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::source::open_err;

    /// A fixture from `tests/fixtures/` (generated by `make_xlsx_fixtures.py`).
    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// Collects a source's rows, failing on any reading error (tests read well-formed files).
    fn all_rows(source: &mut XlsxSource) -> Vec<Row> {
        source.rows().map(|r| r.expect("row")).collect()
    }

    /// Basic fixture: headers, strings, numbers, a formula's cached value, a date serial,
    /// an error cell (absent), sparse rows, padding to the header's width.
    #[test]
    fn xlsx_basic_fixture() {
        let path = fixture("basic.xlsx");
        let mut source =
            XlsxSource::open_sheet(&path, &Sheet::First, &Limits::default()).expect("open");
        assert_eq!(
            source.headers(),
            &[
                "name".to_owned(),
                "total".to_owned(),
                "other".to_owned(),
                "<q> & more".to_owned(),
            ]
        );
        let rows = all_rows(&mut source);
        // 4 data rows (rows 2..=5 of the sheet), each padded to 4 columns.
        assert_eq!(rows.len(), 4);
        assert_eq!(
            rows[0],
            Row {
                number: 2,
                cells: vec![
                    Cell::Text("Acme Ltd".into()),
                    Cell::Text("1.5".into()),
                    Cell::Text("42".into()),
                    // Serial 46000 on the 1900 system: 2025-12-09 (verified: the date
                    // components come through the workbook's own date system).
                    Cell::Text("2025-12-09".into()),
                ],
            }
        );
        // Row 3: formula's cached value 3 (never the formula text), the error cell missing.
        assert_eq!(
            rows[1],
            Row {
                number: 3,
                cells: vec![
                    Cell::Text("3".into()),
                    Cell::Text("7.25".into()),
                    Cell::Missing,
                    Cell::Missing,
                ],
            }
        );
        // Row 4: only column C used; padding on both sides.
        assert_eq!(rows[2].number, 4);
        assert_eq!(rows[2].cells[2].as_str(), "name");
        assert_eq!(rows[2].cells[0], Cell::Missing);
        // Row 5: multiline shared string survives; a float cell reads as its shortest
        // round-trip text (calamine gives the cell as f64, so the XML's `0.00` digits are
        // gone; f64 0.0 formats as `0`).
        assert_eq!(rows[3].number, 5);
        assert_eq!(rows[3].cells[0].as_str(), "line1\nline2");
        assert_eq!(rows[3].cells[1].as_str(), "0");
    }

    /// A named sheet is read; an unknown one reports `import.unknown_sheet` with the list.
    #[test]
    fn xlsx_sheet_selection() {
        let path = fixture("basic.xlsx");
        let mut source =
            XlsxSource::open_sheet(&path, &Sheet::Named("Other".into()), &Limits::default())
                .expect("open");
        assert_eq!(source.headers(), &["other".to_owned()]);
        assert_eq!(all_rows(&mut source)[0].cells[0].as_str(), "Acme Ltd");
    }

    /// An unknown sheet reports `import.unknown_sheet` naming the available sheets.
    #[test]
    fn xlsx_unknown_sheet() {
        let path = fixture("basic.xlsx");
        let error = XlsxSource::open_sheet(&path, &Sheet::Named("Nope".into()), &Limits::default())
            .expect_err("unknown sheet");
        assert_eq!(error.code(), crate::error::UNKNOWN_SHEET);
        assert!(error.to_string().contains("Data"));
        assert!(error.to_string().contains("Other"));
    }

    /// A file that is not a zip reports `import.not_a_workbook`.
    #[test]
    fn xlsx_corrupt_file() {
        let path = fixture("corrupt.xlsx");
        let error = open_err::<XlsxSource>(&path, &Limits::default());
        assert_eq!(error.code(), crate::error::NOT_A_WORKBOOK);
    }

    /// A workbook with no header row reports `import.no_header`.
    #[test]
    fn xlsx_empty_sheet() {
        let path = fixture("empty.xlsx");
        let error = open_err::<XlsxSource>(&path, &Limits::default());
        assert_eq!(error.code(), crate::error::NO_HEADER);
    }

    /// Over the size limit fails at open; the row limit fails mid-stream.
    #[test]
    fn xlsx_limits() {
        let path = fixture("basic.xlsx");
        let error = open_err::<XlsxSource>(&path, &Limits::platform(Some(0), None));
        assert_eq!(error.code(), crate::error::FILE_TOO_LARGE);

        let mut source =
            XlsxSource::open_sheet(&path, &Sheet::First, &Limits::platform(None, Some(2)))
                .expect("open");
        let mut stream = source.rows();
        stream.next().expect("row").expect("ok");
        stream.next().expect("row").expect("ok");
        let error = stream.next().expect("row").expect_err("too many rows");
        assert_eq!(error.code(), crate::error::TOO_MANY_ROWS);
    }

    /// A missing workbook is `import.file_not_found`.
    #[test]
    fn xlsx_missing_file() {
        let path = fixture("no-such-file.xlsx");
        let error = open_err::<XlsxSource>(&path, &Limits::default());
        assert_eq!(error.code(), crate::error::FILE_NOT_FOUND);
    }

    // Recorded security finding: zip-bomb fixtures are generated here (a few KiB on disk, huge when inflated).

    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// A zip with the given `(name, content)` entries, deflated.
    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(&mut out);
        for (name, content) in entries {
            writer
                .start_file(
                    *name,
                    SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated),
                )
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap();
        out.into_inner()
    }

    /// Writes `bytes` to a fresh temp file and returns its path (removed by the caller's dir).
    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spot-import-bomb-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bomb.xlsx");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// Overwrites the declared uncompressed size of every entry (central directory and local
    /// header) with `declared`, so the archive lies about its size.
    fn lie_about_sizes(bytes: &mut [u8], declared: u32) {
        for i in 0..bytes.len().saturating_sub(30) {
            if bytes[i..i + 4] == [0x50, 0x4b, 0x01, 0x02] {
                bytes[i + 24..i + 28].copy_from_slice(&declared.to_le_bytes());
            } else if bytes[i..i + 4] == [0x50, 0x4b, 0x03, 0x04] {
                bytes[i + 22..i + 26].copy_from_slice(&declared.to_le_bytes());
            }
        }
    }

    fn small_caps() -> Limits {
        Limits {
            max_decompressed_bytes: 1024 * 1024,
            max_shared_string_bytes: 256 * 1024,
            max_shared_strings: 1000,
            ..Limits::default()
        }
    }

    #[test]
    fn a_huge_sheet_is_refused_by_counted_bytes() {
        let sheet = vec![b' '; 64 * 1024 * 1024];
        let bytes = zip_bytes(&[("xl/worksheets/sheet1.xml", &sheet)]);
        assert!(bytes.len() < 200 * 1024, "the bomb stays small on disk");
        let path = temp_file("sheet", &bytes);
        let started = std::time::Instant::now();
        let error = open_err::<XlsxSource>(&path, &small_caps());
        assert_eq!(error.code(), crate::error::FILE_TOO_LARGE);
        assert!(
            started.elapsed().as_secs() < 10,
            "refusal is bounded in time"
        );
    }

    #[test]
    fn a_huge_shared_string_table_is_refused_by_counted_bytes() {
        let table = vec![b'x'; 8 * 1024 * 1024];
        let bytes = zip_bytes(&[("xl/sharedStrings.xml", &table)]);
        let path = temp_file("sst-bytes", &bytes);
        let limits = Limits {
            max_decompressed_bytes: 64 * 1024 * 1024,
            max_shared_string_bytes: 256 * 1024,
            ..small_caps()
        };
        let error = open_err::<XlsxSource>(&path, &limits);
        assert_eq!(error.code(), crate::error::FILE_TOO_LARGE);
    }

    #[test]
    fn too_many_shared_strings_are_refused() {
        let table = "<si><t>a</t></si>".repeat(5000);
        let bytes = zip_bytes(&[("xl/sharedStrings.xml", table.as_bytes())]);
        let path = temp_file("sst-count", &bytes);
        let error = open_err::<XlsxSource>(&path, &small_caps());
        assert_eq!(error.code(), crate::error::NOT_A_WORKBOOK);
        assert!(error.to_string().contains("shared-string"));
    }

    #[test]
    fn declared_sizes_that_lie_do_not_hide_a_bomb() {
        let sheet = vec![b' '; 64 * 1024 * 1024];
        let mut bytes = zip_bytes(&[("xl/worksheets/sheet1.xml", &sheet)]);
        lie_about_sizes(&mut bytes, 10);
        let path = temp_file("lying", &bytes);
        let started = std::time::Instant::now();
        let error = open_err::<XlsxSource>(&path, &small_caps());
        assert!(
            matches!(
                error.code(),
                crate::error::FILE_TOO_LARGE | crate::error::NOT_A_WORKBOOK
            ),
            "got {error}"
        );
        assert!(started.elapsed().as_secs() < 10);
    }

    #[test]
    fn the_ratio_is_judged_on_counted_bytes() {
        // 2 MiB of zeros deflates to ~2 KiB: far over a ratio of 100, under the byte cap.
        let sheet = vec![0u8; 2 * 1024 * 1024];
        let bytes = zip_bytes(&[("xl/worksheets/sheet1.xml", &sheet)]);
        let path = temp_file("ratio", &bytes);
        let limits = Limits {
            max_decompressed_bytes: 64 * 1024 * 1024,
            ..small_caps()
        };
        let error = open_err::<XlsxSource>(&path, &limits);
        assert_eq!(error.code(), crate::error::FILE_TOO_LARGE);
    }

    #[test]
    fn too_many_zip_entries_are_refused() {
        let entries: Vec<(String, Vec<u8>)> = (0..20)
            .map(|i| (format!("e{i}.xml"), vec![b'a'; 10]))
            .collect();
        let refs: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(n, c)| (n.as_str(), c.as_slice()))
            .collect();
        let path = temp_file("entries", &zip_bytes(&refs));
        let limits = Limits {
            max_zip_entries: 5,
            ..small_caps()
        };
        let error = open_err::<XlsxSource>(&path, &limits);
        assert_eq!(error.code(), crate::error::NOT_A_WORKBOOK);
    }

    #[test]
    fn the_shared_string_counter_sees_tags_split_across_chunks() {
        let mut counter = SiCounter::default();
        for chunk in [
            &b"<sst><"[..],
            b"s",
            b"i>x</si><s",
            b"i/><si a=\"1\">",
            b"</si><sst>",
        ] {
            counter.feed(chunk);
        }
        assert_eq!(counter.count, 3);
    }
}
