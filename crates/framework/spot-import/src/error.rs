//! The typed errors of the import engine (WP-DOC-09.T1, ADR-26).
//!
//! Every failure carries a stable dotted `import.*` code ([`ImportError::code`]); callers match
//! on the code, never on the message. Fatal problems (the file as a whole is unread, over a
//! limit, of an unknown shape) come back as [`ImportError`]; problems with single rows and
//! cells are **not** these — they land in the [`ImportReport`](crate::report::ImportReport) as
//! `ImportRowError`s, so one bad row never stops the import.
//!
//! # Codes
//!
//! | Code | Meaning |
//! | --- | --- |
//! | [`FILE_NOT_FOUND`] | the file does not exist or cannot be opened |
//! | [`FILE_TOO_LARGE`] | the file is over the configured maximum size |
//! | [`TOO_MANY_ROWS`] | the file has more rows than the configured maximum |
//! | [`TOO_MANY_COLUMNS`] | a row has more columns than the configured maximum |
//! | [`CELL_TOO_LONG`] | one cell is over the maximum cell length |
//! | [`UNKNOWN_SHEET`] | the requested XLSX sheet does not exist |
//! | [`NOT_A_WORKBOOK`] | the file is not the format its name claims |
//! | [`BAD_ENCODING`] | a CSV cell is not UTF-8 |
//! | [`NO_HEADER`] | the file has no header row at all |
//! | [`UNKNOWN_DOCTYPE`] | the target DocType is not in the metadata |
//! | [`META_ERROR`] | the metadata itself is inconsistent |
//!
//! Row and cell problems use the codes in [`crate::mapping`] and [`crate::error`].
//!
//! # Writer codes
//!
//! The back half ([`crate::apply`]) reports problems with single documents in the same report
//! format, so they use row-level codes too:
//!
//! | Code | Meaning |
//! | --- | --- |
//! | [`KEY_MISSING`] | the document has no value in the update mode's key field |
//! | [`KEY_NOT_FOUND`] | the key value names no document and the mode does not create one |
//! | [`MISSING_KEY_INDEX`] | a key mode was chosen without a [`KeyIndex`](crate::apply::KeyIndex) |
//! | [`SPOOL_FULL`] | all-or-nothing needed more buffer than the spool's cap |
//! | [`TOO_MANY_ERRORS`] | the run stopped at its `max_errors` limit |

use std::path::Path;

/// The file does not exist or cannot be opened.
pub const FILE_NOT_FOUND: &str = "import.file_not_found";
/// The file is larger than the configured `max_file_size`.
pub const FILE_TOO_LARGE: &str = "import.file_too_large";
/// The file has more rows than the configured `max_rows`.
pub const TOO_MANY_ROWS: &str = "import.too_many_rows";
/// A row has more columns than the configured `max_columns`.
pub const TOO_MANY_COLUMNS: &str = "import.too_many_columns";
/// A single cell is longer than the maximum cell length.
pub const CELL_TOO_LONG: &str = "import.cell_too_long";
/// The requested XLSX sheet does not exist in the workbook.
pub const UNKNOWN_SHEET: &str = "import.unknown_sheet";
/// The file is not the format its name or content claims (not an XLSX, or an unreadable CSV).
pub const NOT_A_WORKBOOK: &str = "import.not_a_workbook";
/// A CSV value is not UTF-8 (the platform's text is UTF-8 everywhere).
pub const BAD_ENCODING: &str = "import.bad_encoding";
/// The file holds no header row at all.
pub const NO_HEADER: &str = "import.no_header";
/// The target DocType is not in the effective metadata.
pub const UNKNOWN_DOCTYPE: &str = "meta.unknown_doctype";
/// The metadata itself is inconsistent (a table points at an undefined child DocType).
pub const META_ERROR: &str = "import.meta_error";

/// A cell's text does not convert to its field's type (`import.bad_value`), a row-level error.
pub const BAD_VALUE: &str = "import.bad_value";
/// A date or datetime cell matches two configured formats with different results
/// (`import.ambiguous_date`); name the format explicitly to pick one.
pub const AMBIGUOUS_DATE: &str = "import.ambiguous_date";
/// A child row has no parent row standing above it (`import.orphan_row`).
pub const ORPHAN_ROW: &str = "import.orphan_row";
/// The document has no value in the field the update mode keys on (`import.key_missing`).
pub const KEY_MISSING: &str = "import.key_missing";
/// The key value names no existing document and the mode does not create one
/// (`import.key_not_found`).
pub const KEY_NOT_FOUND: &str = "import.key_not_found";
/// An update or upsert mode was chosen but the caller supplied no key index
/// (`import.missing_key_index`).
pub const MISSING_KEY_INDEX: &str = "import.missing_key_index";
/// All-or-nothing needed to buffer more validated parents than the spool's cap allows
/// (`import.spool_full`); nothing was written.
pub const SPOOL_FULL: &str = "import.spool_full";
/// The same key value appears in more than one parent of an update or upsert file
/// (`import.duplicate_key`).
pub const DUPLICATE_KEY: &str = "import.duplicate_key";
/// A real run was started without a unique batch label (`import.config_batch_label`).
pub const CONFIG_BATCH_LABEL: &str = "import.config_batch_label";
/// A create returned a document that is not the one imported, which means its command id
/// belonged to some other import (`import.idempotency_conflict`); it is not counted as created.
pub const IDEMPOTENCY_CONFLICT: &str = "import.idempotency_conflict";
/// The command id for a write could not be built (`import.config_command_id`).
pub const CONFIG_COMMAND_ID: &str = "import.config_command_id";
/// The import stopped after the configured number of failed documents
/// (`import.too_many_errors`).
pub const TOO_MANY_ERRORS: &str = "import.too_many_errors";

/// One failure that stops the import as a whole.
///
/// Row-level problems are not these: they are [`ImportRowError`]s in the report, so the user
/// fixes everything at once. This enum is `#[non_exhaustive]`; match with a catch-all arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ImportError {
    /// The file does not exist or cannot be opened.
    #[error("cannot open {path}: {io}")]
    FileNotFound {
        /// The path that could not be opened (empty when the failure is mid-read).
        path: String,
        /// The underlying I/O error text.
        io: String,
    },
    /// The file is over the configured maximum size.
    #[error("the file is {found} bytes, over the {limit} byte limit")]
    FileTooLarge {
        /// The file's size in bytes.
        found: u64,
        /// The configured limit.
        limit: u64,
    },
    /// The file has more rows than the configured maximum.
    #[error("the file has more than {limit} rows")]
    TooManyRows {
        /// The configured limit.
        limit: u64,
    },
    /// A row has more columns than the configured maximum.
    #[error("row {row} has more than {limit} columns")]
    TooManyColumns {
        /// The 1-based number of the offending row.
        row: u64,
        /// The configured limit.
        limit: usize,
    },
    /// One cell is longer than the maximum cell length.
    #[error("cell {column} of row {row} is {found} characters, over the {limit} limit")]
    CellTooLong {
        /// The 1-based row number.
        row: u64,
        /// The column's 1-based index (as text, since headers may not be known yet).
        column: String,
        /// The cell's length in characters.
        found: usize,
        /// The configured limit.
        limit: usize,
    },
    /// The requested XLSX sheet does not exist.
    #[error("the workbook has no sheet named {name}; it has: {available}")]
    UnknownSheet {
        /// The requested sheet name.
        name: String,
        /// The sheet names the workbook does have, comma-separated.
        available: String,
    },
    /// The file is not the format its name or content claims.
    #[error("{path} is not a readable import file: {reason}")]
    NotAWorkbook {
        /// The path, or empty when the failure is mid-stream.
        path: String,
        /// Why the file was refused.
        reason: String,
    },
    /// A CSV value is not UTF-8.
    #[error("row {row}, column {column}: the cell is not UTF-8 ({details})")]
    BadEncoding {
        /// The 1-based number of the row holding the bad bytes.
        row: u64,
        /// The offending column's 1-based index.
        column: usize,
        /// The underlying `Utf8Error` text.
        details: String,
    },
    /// The file holds no header row at all.
    #[error("no header row: the file holds no readable rows")]
    NoHeader,
    /// The target DocType is not in the effective metadata.
    #[error("the DocType {doctype} does not exist")]
    UnknownDoctype {
        /// The requested DocType name.
        doctype: String,
    },
    /// The metadata is inconsistent (checked before any row is read).
    #[error("the metadata is inconsistent: {message}")]
    Meta {
        /// What is inconsistent.
        message: String,
    },
}

impl ImportError {
    /// The stable dotted code of this failure.
    pub fn code(&self) -> &'static str {
        match self {
            ImportError::FileNotFound { .. } => FILE_NOT_FOUND,
            ImportError::FileTooLarge { .. } => FILE_TOO_LARGE,
            ImportError::TooManyRows { .. } => TOO_MANY_ROWS,
            ImportError::TooManyColumns { .. } => TOO_MANY_COLUMNS,
            ImportError::CellTooLong { .. } => CELL_TOO_LONG,
            ImportError::UnknownSheet { .. } => UNKNOWN_SHEET,
            ImportError::NotAWorkbook { .. } => NOT_A_WORKBOOK,
            ImportError::BadEncoding { .. } => BAD_ENCODING,
            ImportError::NoHeader => NO_HEADER,
            ImportError::UnknownDoctype { .. } => UNKNOWN_DOCTYPE,
            ImportError::Meta { .. } => META_ERROR,
        }
    }
}

/// Reads the size of `path`, failing with [`ImportError::FileNotFound`] when it cannot.
pub(crate) fn file_size(path: &Path) -> Result<u64, ImportError> {
    std::fs::metadata(path)
        .map(|m| m.len())
        .map_err(|e| ImportError::FileNotFound {
            path: path.display().to_string(),
            io: e.to_string(),
        })
}

/// Fails when `size` is over `max_file_size`.
pub(crate) fn check_file_size(size: u64, max_file_size: Option<u64>) -> Result<(), ImportError> {
    match max_file_size {
        Some(limit) if size > limit => Err(ImportError::FileTooLarge { found: size, limit }),
        _ => Ok(()),
    }
}

/// An I/O failure while streaming a file.
pub(crate) fn read_io(e: std::io::Error, path: &Path) -> ImportError {
    ImportError::FileNotFound {
        path: path.display().to_string(),
        io: e.to_string(),
    }
}
