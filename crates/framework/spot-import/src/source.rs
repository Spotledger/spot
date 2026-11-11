//! Streaming row sources: the [`RowSource`] trait, [`AnySource`], and [`Limit`] plumbing.
//!
//! The engine never holds a file in memory: a source yields one [`Row`] at a time and the
//! caller drops it before asking for the next. The concrete readers are [`crate::csv::CsvSource`]
//! and [`crate::xlsx::XlsxSource`]; both
//!
//! - number rows from 1, the real line or spreadsheet row, so an error points where the user
//!   looks ([`Row::number`]);
//! - yield [`Cell::Missing`] for absent columns of a short row or a sparse spreadsheet row, so
//!   every column keeps its header;
//! - enforce the caller's [`Limits`] and refuse an impossible file with a typed
//!   [`ImportError`] **before** work is wasted on it: the file-size limit at open, the column
//!   and cell-length limits while reading, the row limit as soon as the next row passes it.
//!
//! Sources are lending iterators ([`RowSource::rows`] borrows the source): the XLSX cell reader
//! borrows the open workbook, so ownership is expressed with lifetimes instead of a
//! self-referential struct. `crate::stream::import_stream` drives exactly that loop.

use std::path::Path;

use crate::csv::CsvSource;
use crate::error::ImportError;
use crate::xlsx::XlsxSource;

pub use crate::csv::Delimiter;
pub use crate::xlsx::Sheet;

/// Import limits, checked while reading so an impossible file is refused before work is wasted.
///
/// The platform caps (decompressed bytes, shared-string table bytes and entries, rows, columns,
/// cell length) are the default; there is no constructor that leaves them unset, so a limit
/// set always carries every cap.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Largest (compressed) file in bytes; over the limit is [`ImportError::FileTooLarge`] at open.
    pub max_file_size: Option<u64>,
    /// Largest total decompressed bytes across the workbook's zip entries (zip-bomb guard).
    pub max_decompressed_bytes: u64,
    /// Largest shared-string table in bytes.
    pub max_shared_string_bytes: u64,
    /// Most entries in the shared-string table.
    pub max_shared_strings: usize,
    /// Most columns in one row (the platform's cap: 500).
    pub max_columns: usize,
    /// Longest cell in characters (the platform's cap: 1 MiB).
    pub max_cell_length: usize,
    /// Most data rows; reaching a row past the limit fails with [`ImportError::TooManyRows`]
    /// instead of silently importing a prefix.
    pub max_rows: Option<u64>,
    /// Most entries in the workbook's zip archive (nested-entry guard).
    pub max_zip_entries: usize,
    /// Maximum allowed ratio of decompressed to compressed bytes for any single entry.
    pub max_compression_ratio: u64,
    /// The CSV delimiter; XLSX ignores it.
    pub delimiter: Delimiter,
}

impl Default for Limits {
    fn default() -> Self {
        Self::platform(None, None)
    }
}

impl Limits {
    /// The platform's caps. `max_file_size` and `max_rows` default to the platform cap when
    /// `None` is passed.
    pub fn platform(max_file_size: Option<u64>, max_rows: Option<u64>) -> Self {
        const PLATFORM_FILE_SIZE: u64 = 50 * 1024 * 1024;
        const PLATFORM_DECOMPRESSED_BYTES: u64 = 100 * 1024 * 1024;
        const PLATFORM_SHARED_STRING_BYTES: u64 = 64 * 1024 * 1024;
        const PLATFORM_SHARED_STRINGS: usize = 1_000_000;
        const PLATFORM_ROWS: u64 = 1_000_000;
        const PLATFORM_ZIP_ENTRIES: usize = 10_000;
        const PLATFORM_COMPRESSION_RATIO: u64 = 100;
        Self {
            max_file_size: max_file_size.or(Some(PLATFORM_FILE_SIZE)),
            max_decompressed_bytes: PLATFORM_DECOMPRESSED_BYTES,
            max_shared_string_bytes: PLATFORM_SHARED_STRING_BYTES,
            max_shared_strings: PLATFORM_SHARED_STRINGS,
            max_columns: 500,
            max_cell_length: 1024 * 1024,
            max_rows: max_rows.or(Some(PLATFORM_ROWS)),
            max_zip_entries: PLATFORM_ZIP_ENTRIES,
            max_compression_ratio: PLATFORM_COMPRESSION_RATIO,
            delimiter: Delimiter::Detect,
        }
    }

    /// Limits for tests: small caps so the checks themselves are exercised cheaply.
    #[cfg(test)]
    pub fn tiny() -> Self {
        Self {
            max_columns: 3,
            max_cell_length: 8,
            max_rows: Some(4),
            ..Self::default()
        }
    }
}

/// One cell of a source row: its text, or an absent column.
///
/// A CSV row is as wide as its header (short rows carry [`Cell::Missing`]); an XLSX row's
/// unused columns before, between and after used ones are [`Cell::Missing`] too. Missing is
/// distinct from [`Cell::Text("")`]: an empty string is a present-but-empty cell, which the
/// conversion turns into null or an error per the field's `empty_to_null` option.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    /// The column has no value in this row.
    Missing,
    /// The cell's text as it appears in the file.
    Text(String),
}

impl Cell {
    /// The cell's text, or `""` when the column is absent.
    pub fn as_str(&self) -> &str {
        match self {
            Cell::Missing => "",
            Cell::Text(t) => t,
        }
    }

    /// Whether the column is absent (distinct from an empty string).
    pub fn is_missing(&self) -> bool {
        matches!(self, Cell::Missing)
    }
}

/// One source row: its 1-based number in the file and its cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The 1-based row number as the user sees it in the file: the CSV line the record starts
    /// on, or the XLSX row index. The header row is row 1; data rows start at 2.
    pub number: u64,
    /// The row's cells, one per column of the row's width.
    pub cells: Vec<Cell>,
}

/// A boxed, borrowed stream of rows: [`RowSource::rows`]'s return type.
///
/// The rows borrow their source (the XLSX cell reader borrows the open workbook), so one
/// stream exists per source at a time and the source stays the stream's owner.
pub type BoxedRows<'a> = Box<dyn Iterator<Item = Result<Row, ImportError>> + 'a>;

/// A streaming source of rows for one file.
///
/// [`RowSource::open`] checks the file-size limit, reads the header row (row 1) and returns the
/// source ready to stream data rows from row 2.
pub trait RowSource: Sized {
    /// Opens `path`, checks the limits, reads the header row and returns the source.
    fn open(path: &Path, limits: &Limits) -> Result<Self, ImportError>;

    /// The file's size in bytes.
    fn file_size(&self) -> u64;

    /// The header row's cell texts.
    fn headers(&self) -> &[String];

    /// The configured limits.
    fn limits(&self) -> &Limits;

    /// Streams every data row, one at a time.
    ///
    /// The returned iterator borrows the source: an XLSX cell reader borrows the open workbook,
    /// so one stream exists at a time and the header row is never yielded. Reading failures
    /// (encoding, limits) surface through the iterator; end of file is `None`.
    fn rows(&mut self) -> BoxedRows<'_>;
}

/// A CSV or XLSX import source, for callers that accept either.
pub enum AnySource {
    /// A CSV file.
    Csv(Box<CsvSource>),
    /// An XLSX file.
    Xlsx(Box<XlsxSource>),
}

impl std::fmt::Debug for AnySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnySource::Csv(s) => f
                .debug_struct("AnySource::Csv")
                .field("headers", &s.headers())
                .finish(),
            AnySource::Xlsx(s) => f
                .debug_struct("AnySource::Xlsx")
                .field("headers", &s.headers())
                .finish(),
        }
    }
}

impl AnySource {
    /// Opens the source by the file's extension: `.csv` reads CSV, `.xlsx` XLSX (first
    /// sheet). Anything else is refused before the file is opened, so a mislabelled file
    /// never reaches the wrong reader.
    pub fn open(path: &Path, limits: &Limits) -> Result<Self, ImportError> {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("csv") => {
                Ok(Self::Csv(Box::new(CsvSource::open(path, limits)?)))
            }
            Some(ext) if ext.eq_ignore_ascii_case("xlsx") => Ok(Self::Xlsx(Box::new(
                XlsxSource::open_sheet(path, &Sheet::First, limits)?,
            ))),
            _ => Err(ImportError::NotAWorkbook {
                path: path.display().to_string(),
                reason: "the file's extension is neither .csv nor .xlsx".to_owned(),
            }),
        }
    }

    /// The kind of file this is.
    pub fn kind(&self) -> &'static str {
        match self {
            AnySource::Csv(_) => "csv",
            AnySource::Xlsx(_) => "xlsx",
        }
    }
}

impl RowSource for AnySource {
    fn open(path: &Path, limits: &Limits) -> Result<Self, ImportError> {
        Self::open(path, limits)
    }

    fn file_size(&self) -> u64 {
        match self {
            AnySource::Csv(s) => s.file_size(),
            AnySource::Xlsx(s) => s.file_size(),
        }
    }

    fn headers(&self) -> &[String] {
        match self {
            AnySource::Csv(s) => s.headers(),
            AnySource::Xlsx(s) => s.headers(),
        }
    }

    fn limits(&self) -> &Limits {
        match self {
            AnySource::Csv(s) => s.limits(),
            AnySource::Xlsx(s) => s.limits(),
        }
    }

    fn rows(&mut self) -> BoxedRows<'_> {
        match self {
            AnySource::Csv(s) => s.rows(),
            AnySource::Xlsx(s) => s.rows(),
        }
    }
}

/// Opens a source, expecting the typed failure (tests assert codes, not values).
#[cfg(test)]
pub(crate) fn open_err<S: RowSource + std::fmt::Debug>(
    path: &std::path::Path,
    limits: &Limits,
) -> ImportError {
    match S::open(path, limits) {
        Ok(source) => panic!("open must fail, got {source:?}"),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::super::csv::temp_file;
    use super::*;

    /// `AnySource` opens by the extension and reports its kind.
    #[test]
    fn any_source_dispatches_by_extension() {
        let csv = temp_file("any_source.csv", b"a\n1\n");
        let source = AnySource::open(&csv, &Limits::default()).expect("csv");
        assert_eq!(source.kind(), "csv");
        assert_eq!(source.headers(), &["a".to_owned()]);

        let xlsx =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/basic.xlsx");
        let source = AnySource::open(&xlsx, &Limits::default()).expect("xlsx");
        assert_eq!(source.kind(), "xlsx");
        assert!(source.headers().iter().any(|h| h == "name"));

        let neither = temp_file("any_source.txt", b"a\n1\n");
        let error = AnySource::open(&neither, &Limits::default()).expect_err("not a workbook");
        assert_eq!(error.code(), crate::error::NOT_A_WORKBOOK);
    }
}
