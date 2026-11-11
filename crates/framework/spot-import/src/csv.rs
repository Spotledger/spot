//! The CSV reader (brief item 1): an RFC 4180 streaming parser and the [`CsvSource`].
//!
//! RFC 4180 plus the usual extensions: quoted fields, `""` escapes, embedded newlines, CRLF
//! and LF line endings (a lone CR also ends a record, matching every common producer), the
//! delimiter `,` `;` `\t` — detected from the first 8 KiB outside quotes (ties keep `,`) or
//! set explicitly — and a skipped UTF-8 BOM. A non-UTF-8 cell fails the whole import
//! ([`ImportError::BadEncoding`] names row and column): the file is not the text it claims to
//! be, and guessing an encoding would silently corrupt names. Blank lines are skipped and
//! never numbered.
//!
//! The parser is hand-written, not the `csv` crate, for one probe-verified reason: the crate's
//! line counter (`position().line()`) misses the last newline of every CRLF-terminated record
//! and every skipped blank line, while the engine's row numbers must point at the physical
//! line the user sees in their editor. This parser counts every `\n` byte as it scans, so the
//! number is exact by construction. Memory holds one record at a time; the cell-length limit
//! fires while a field is still growing, so nothing over the limit is ever buffered whole.
//!
//! Quoting: a quote in the middle of an unquoted field is literal text (`a"b` stays `a"b`),
//! matching RFC 4180's producer rules and every common spreadsheet export. Text after a
//! closing quote continues the field leniently (`"a"b` reads `ab`+`b` = `ab`); strict producers
//! never write such a field.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use crate::error::ImportError;
use crate::source::{BoxedRows, Cell, Limits, Row, RowSource};

/// The CSV delimiter: detected from the file, or set explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delimiter {
    /// The most frequent of `,`, `;`, `\t` in the first 8 KiB outside quotes; ties and files
    /// with no candidate keep the RFC 4180 default `,`.
    Detect,
    /// The explicit delimiter byte, e.g. `b','`, `b';'`, `b'\t'`.
    Byte(u8),
}

impl Delimiter {
    /// The byte this kind resolves to, given the detected one.
    fn resolve(self, detected: u8) -> u8 {
        match self {
            Delimiter::Detect => detected,
            Delimiter::Byte(b) => b,
        }
    }
}

/// Detects the delimiter from a sample: counts `,`, `;`, `\t` outside quotes and takes the
/// most frequent; on a tie the RFC 4180 default `,` wins — unless the sample holds any quote
/// at all, in which case the quoted field's trailing `,` is field content and the tighter `,`
/// interpretation is refused (`;`, then `\t`, wins instead). No candidate at all keeps `,`
/// (a single-column file).
pub(crate) fn detect_delimiter(sample: &[u8]) -> u8 {
    let mut counts = [0u64; 3];
    let mut in_quotes = false;
    let mut quotes = false;
    for &b in sample {
        match b {
            b'"' => {
                in_quotes = !in_quotes;
                quotes = true;
            }
            b',' if !in_quotes => counts[0] += 1,
            b';' if !in_quotes => counts[1] += 1,
            b'\t' if !in_quotes => counts[2] += 1,
            _ => {}
        }
    }
    let tie = |comma: bool| {
        // `comma` selects the `,`-first order for quote-free samples.
        if comma { b',' } else { b';' }
    };
    if counts[1] > counts[0] && counts[1] >= counts[2] {
        b';'
    } else if counts[2] > counts[0] && counts[2] > counts[1] {
        b'\t'
    } else if counts[0] == counts[1] && quotes {
        tie(false)
    } else {
        tie(true)
    }
}

/// The UTF-8 BOM, which some CSV producers prepend.
const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// The parser's refill window: bytes pulled from the reader between scans.
const WINDOW: usize = 8 * 1024;

/// How many bytes the delimiter detection samples: the first 8 KiB.
const DETECT_SAMPLE: usize = 8 * 1024;

/// Reads up to `buf.len()` bytes from the start of `file`, leaving the position there; the
/// caller rewinds before streaming.
fn read_head(file: &mut File, buf: &mut [u8]) -> Result<usize, ImportError> {
    file.rewind()
        .map_err(|e| crate::error::read_io(e, Path::new("")))?;
    let mut read = 0;
    while read < buf.len() {
        match file.read(&mut buf[read..]) {
            Ok(0) => break,
            Ok(n) => read += n,
            Err(e) => return Err(crate::error::read_io(e, Path::new(""))),
        }
    }
    Ok(read)
}

/// Strips a leading UTF-8 BOM so detection and headers never see it.
fn skip_bom(sample: &[u8]) -> &[u8] {
    sample.strip_prefix(&BOM).unwrap_or(sample)
}

/// What ended the field last parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldEnd {
    /// The field ended at a delimiter; the record continues with the next field.
    Delimiter,
    /// The field ended at a record terminator (or the end of the file).
    RecordEnd,
}

/// An RFC 4180 streaming parser over any reader, counting physical lines exactly.
///
/// Bytes flow through a fixed [`WINDOW`] ([`CsvParser::pull_byte`]) into a small state machine
/// (unquoted / quoted field), so the line counter is exact by construction: every `\n` counts
/// once, wherever it sits — inside a quoted field, in a CRLF pair, or on a skipped blank line.
/// Cell text accumulates as raw bytes and is validated as UTF-8 per field ([`Cell`]); a bad
/// byte reports the row and column. Nothing is ever a float. A one-byte pushback
/// ([`CsvParser::push_back`]) carries terminator lookaheads and text after a closing quote.
pub struct CsvParser<R: Read> {
    reader: R,
    /// Refill window; `buf[pos..end]` holds the bytes not yet scanned.
    buf: Box<[u8; WINDOW]>,
    /// Scanning position inside `buf`.
    pos: usize,
    /// End of the bytes currently in the window.
    end: usize,
    /// Whether the reader reached end of file.
    eof: bool,
    /// Whether the leading BOM was already handled (once per file).
    bom_seen: bool,
    /// The 1-based line of the next byte to scan.
    line: u64,
    /// The delimiter byte.
    delimiter: u8,
    limits: Limits,
}

impl<R: Read> CsvParser<R> {
    /// A parser over `reader` with the given delimiter byte and default limits.
    fn new(reader: R, delimiter: u8) -> Self {
        Self {
            reader,
            buf: Box::new([0; WINDOW]),
            pos: 0,
            end: 0,
            eof: false,
            bom_seen: false,
            line: 1,
            delimiter,
            limits: Limits::default(),
        }
    }

    /// Installs the caller's limits (cell length, column count).
    fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    /// Pulls the next byte, refilling the window as needed; `None` at end of file. An I/O
    /// failure becomes a typed [`ImportError`].
    fn pull_byte(&mut self) -> Result<Option<u8>, ImportError> {
        if self.pos < self.end {
            let byte = self.buf[self.pos];
            self.pos += 1;
            if byte == b'\n' {
                self.line += 1;
            }
            return Ok(Some(byte));
        }
        if self.eof {
            return Ok(None);
        }
        loop {
            match self.reader.read(&mut self.buf[..]) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(None);
                }
                Ok(n) => {
                    self.pos = 1;
                    self.end = n;
                    let byte = self.buf[0];
                    if byte == b'\n' {
                        self.line += 1;
                    }
                    return Ok(Some(byte));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(crate::error::read_io(e, Path::new(""))),
            }
        }
    }

    /// Pushes one pulled byte back, to be pulled again (lookahead support). Only ever called
    /// just after a pull, so the window still holds the byte's slot.
    fn push_back(&mut self, byte: u8) {
        debug_assert!(self.pos > 0, "push_back needs a pulled byte behind it");
        self.pos -= 1;
        self.buf[self.pos] = byte;
        if byte == b'\n' {
            self.line -= 1;
        }
    }

    /// Skips the UTF-8 BOM if the file begins with one. Bytes that only partly match the BOM
    /// are pushed back in reverse order, so a non-BOM file loses nothing (the match can only
    /// consume bytes from one fresh window, and pushback never crosses a refill there).
    fn skip_bom(&mut self) -> Result<(), ImportError> {
        let mut seen: [u8; 3] = [0; 3];
        let mut n = 0;
        for expected in BOM {
            match self.pull_byte()? {
                Some(byte) if byte == expected => {
                    seen[n] = byte;
                    n += 1;
                }
                Some(byte) => {
                    seen[n] = byte;
                    n += 1;
                    for back in seen[..n].iter().rev() {
                        self.push_back(*back);
                    }
                    return Ok(());
                }
                None => {
                    for back in seen[..n].iter().rev() {
                        self.push_back(*back);
                    }
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// The next record's cells and its exact 1-based start line, or `None` at end of file.
    ///
    /// Blank physical lines (one empty field between terminators) are skipped and never
    /// numbered, so a trailing blank line changes nothing. The record is refused past the
    /// caller's column limit, every cell past the cell-length limit.
    fn next_record(&mut self) -> Result<Option<(u64, Vec<Cell>)>, ImportError> {
        if !self.bom_seen {
            self.bom_seen = true;
            self.skip_bom()?;
        }
        loop {
            // Find the record's first byte, skipping terminators and blank lines.
            let mut first = match self.pull_byte()? {
                None => return Ok(None),
                Some(byte) => byte,
            };
            loop {
                let skip = if first == b'\n' {
                    true
                } else if first == b'\r' {
                    match self.pull_byte()? {
                        // CRLF and CR-at-EOF are consumed; a lone CR hands its follower on.
                        Some(b'\n') | None => true,
                        Some(next) => {
                            first = next;
                            false
                        }
                    }
                } else {
                    false
                };
                if !skip {
                    break;
                }
                first = match self.pull_byte()? {
                    None => return Ok(None),
                    Some(byte) => byte,
                };
            }
            // `first` begins a record on the current line.
            let line = self.line;
            let mut cells: Vec<Cell> = Vec::new();
            let mut rest = Some(first);
            loop {
                let column = cells.len() + 1;
                let (cell, end) = self.parse_field(line, &mut rest, column)?;
                cells.push(cell);
                match end {
                    FieldEnd::Delimiter => {
                        // A delimiter after the last allowed column means another field
                        // follows: the record is over the cap. Fires at the violation, not
                        // at the record's end, so a runaway record stops early.
                        if cells.len() >= self.limits.max_columns {
                            return Err(ImportError::TooManyColumns {
                                row: line,
                                limit: self.limits.max_columns,
                            });
                        }
                        // The next field starts on the byte after the delimiter; a record
                        // ending `a,` (a terminator or EOF right after) gives that final
                        // slot one empty cell, which the parser handles directly.
                        rest = match self.pull_byte()? {
                            None => {
                                cells.push(Cell::Text(String::new()));
                                break;
                            }
                            Some(byte @ (b'\n' | b'\r')) => {
                                self.push_back(byte);
                                self.consume_terminator()?;
                                cells.push(Cell::Text(String::new()));
                                break;
                            }
                            Some(byte) => Some(byte),
                        };
                    }
                    FieldEnd::RecordEnd => break,
                }
            }
            // A blank physical line is one empty field between terminators: skip it.
            if cells.len() == 1 && cells[0].as_str().is_empty() {
                continue;
            }
            return Ok(Some((line, cells)));
        }
    }

    /// Parses the record's next field on `line`, ending at delimiter or terminator.
    ///
    /// `rest` carries the record's already-pulled first byte into the first field; the field
    /// number comes in as `column` (1-based). A leading quote opens a quoted field, and a
    /// `rest` byte that is ITSELF the delimiter is an empty field (the previous field's
    /// delimiter was consumed, so the empty cell sits between the two delimiters).
    fn parse_field(
        &mut self,
        line: u64,
        rest: &mut Option<u8>,
        column: usize,
    ) -> Result<(Cell, FieldEnd), ImportError> {
        let mut text: Vec<u8> = Vec::new();
        match rest.take() {
            // The empty middle field `a,,b`: the previous delimiter is behind us and this
            // record's next byte is the NEXT delimiter — the cell is empty and still ends
            // at a delimiter (pushing the byte as text would swallow the cell, `a,,b`
            // reading as two fields).
            Some(byte) if byte == self.delimiter => {
                finish_field(text, column, line, FieldEnd::Delimiter)
            }
            Some(b'"') => {
                self.scan_quoted(&mut text, line, column)?;
                // scan_quoted stopped right before the byte after the closing quote (or at
                // EOF): classify it (the byte is consumed here; classify pushes back itself).
                let byte = match self.pull_byte()? {
                    Some(byte) => byte,
                    None => return finish_field(text, column, line, FieldEnd::RecordEnd),
                };
                self.classify_after_quote(text, byte, line, column)
            }
            Some(byte) => {
                text.push(byte);
                let end = self.scan_unquoted(&mut text, line, column)?;
                finish_field(text, column, line, end)
            }
            None => unreachable!("parse_field needs the record's first byte or a delimiter"),
        }
    }

    /// Scans a quoted field into `text`, stopping right after its closing quote (that byte is
    /// pulled and classified by the caller). A doubled quote is a literal quote.
    fn scan_quoted(
        &mut self,
        text: &mut Vec<u8>,
        line: u64,
        column: usize,
    ) -> Result<(), ImportError> {
        loop {
            match self.pull_byte()? {
                None => return Ok(()), // no closing quote: lenient, EOF ends the field
                Some(b'"') => match self.pull_byte()? {
                    Some(b'"') => text.push(b'"'),
                    Some(byte) => {
                        self.push_back(byte);
                        return Ok(());
                    }
                    None => return Ok(()),
                },
                Some(byte) => text.push(byte),
            }
            if text.len() > self.limits.max_cell_length {
                return Err(ImportError::CellTooLong {
                    row: line,
                    column: column.to_string(),
                    found: String::from_utf8_lossy(text).chars().count(),
                    limit: self.limits.max_cell_length,
                });
            }
        }
    }

    /// Scans an unquoted field beginning with `text` (possibly one byte already), ending at a
    /// delimiter or a record terminator. Every byte lands in `text` exactly once.
    fn scan_unquoted(
        &mut self,
        text: &mut Vec<u8>,
        line: u64,
        column: usize,
    ) -> Result<FieldEnd, ImportError> {
        loop {
            match self.pull_byte()? {
                None => return Ok(FieldEnd::RecordEnd),
                Some(byte) if byte == self.delimiter => return Ok(FieldEnd::Delimiter),
                Some(b'\n') | Some(b'\r') => {
                    self.consume_terminator()?;
                    return Ok(FieldEnd::RecordEnd);
                }
                Some(byte) => text.push(byte),
            }
            if text.len() > self.limits.max_cell_length {
                return Err(ImportError::CellTooLong {
                    row: line,
                    column: column.to_string(),
                    found: String::from_utf8_lossy(text).chars().count(),
                    limit: self.limits.max_cell_length,
                });
            }
        }
    }

    /// Consumes a terminator already pulled inside an unquoted field: `\n` and `\r` end the
    /// record. A CRLF pair is consumed whole; a lone CR leaves its follower pushed back.
    fn consume_terminator(&mut self) -> Result<(), ImportError> {
        match self.pull_byte()? {
            None | Some(b'\n') => Ok(()),
            Some(byte) => {
                self.push_back(byte);
                Ok(())
            }
        }
    }

    /// Classifies the first byte after a closing quote: a delimiter ends the field, a
    /// terminator ends the record, anything else continues the field leniently as unquoted
    /// text with the byte pushed back ([`CsvParser::push_back`]), so no byte is lost.
    fn classify_after_quote(
        &mut self,
        text: Vec<u8>,
        byte: u8,
        line: u64,
        column: usize,
    ) -> Result<(Cell, FieldEnd), ImportError> {
        let mut text = text;
        if byte == self.delimiter {
            return finish_field(text, column, line, FieldEnd::Delimiter);
        }
        if byte == b'\n' || byte == b'\r' {
            self.push_back(byte);
            self.consume_terminator()?;
            return finish_field(text, column, line, FieldEnd::RecordEnd);
        }
        // Lenient continuation: `"a"b` reads as field `a`+`b`.
        self.push_back(byte);
        self.scan_unquoted(&mut text, line, column)?;
        finish_field(text, column, line, FieldEnd::RecordEnd)
    }
}

/// Validates `text` as UTF-8 and wraps it as the record's next [`Cell`].
fn finish_field(
    text: Vec<u8>,
    column: usize,
    line: u64,
    end: FieldEnd,
) -> Result<(Cell, FieldEnd), ImportError> {
    let string = std::str::from_utf8(&text).map_err(|e| ImportError::BadEncoding {
        row: line,
        column,
        details: e.to_string(),
    })?;
    Ok((Cell::Text(string.to_owned()), end))
}

impl<R: Read> Iterator for CsvParser<R> {
    type Item = Result<(u64, Vec<Cell>), ImportError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_record().transpose()
    }
}

/// Reads up to the source's limits; a streaming CSV reader ([`RowSource`]).
///
/// One record is parsed at a time from the file; nothing beyond the current record and the
/// header is held. Every row has the header's width: short rows are padded with
/// [`Cell::Missing`] (the mapping reads columns by header), longer rows are cut to it (the
/// column limit was already enforced while reading).
pub struct CsvSource {
    parser: CsvParser<BufReader<File>>,
    headers: Vec<String>,
    width: usize,
    size: u64,
    limits: Limits,
}

impl std::fmt::Debug for CsvSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsvSource")
            .field("headers", &self.headers)
            .field("width", &self.width)
            .field("size", &self.size)
            .finish()
    }
}

impl CsvSource {
    /// Builds the source over an open file with a resolved delimiter, reading the header row.
    fn build(file: File, delimiter: u8, limits: &Limits) -> Result<Self, ImportError> {
        let mut parser = CsvParser::new(BufReader::with_capacity(64 * 1024, file), delimiter);
        parser.set_limits(limits.clone());
        let (_, cells) = parser.next_record()?.ok_or(ImportError::NoHeader)?;
        let headers = cells.iter().map(Cell::as_str).map(str::to_owned).collect();
        Ok(Self {
            width: cells.len(),
            parser,
            headers,
            size: 0,
            limits: limits.clone(),
        })
    }

    /// The next record's cells and exact line, from the parser.
    fn next_cells(&mut self) -> Result<Option<(u64, Vec<Cell>)>, ImportError> {
        self.parser.next_record()
    }
}

impl RowSource for CsvSource {
    fn open(path: &Path, limits: &Limits) -> Result<Self, ImportError> {
        let size = crate::error::file_size(path)?;
        crate::error::check_file_size(size, limits.max_file_size)?;
        let mut file = File::open(path).map_err(|e| crate::error::read_io(e, path))?;
        let mut head = [0u8; DETECT_SAMPLE];
        let read = read_head(&mut file, &mut head)?;
        file.rewind().map_err(|e| crate::error::read_io(e, path))?;
        let sample = skip_bom(&head[..read]);
        let delimiter = limits.delimiter.resolve(detect_delimiter(sample));
        let mut source = Self::build(file, delimiter, limits)?;
        source.size = size;
        Ok(source)
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
        let width = self.width;
        let limits = self.limits.clone();
        let mut done = false;
        let mut yielded = 0u64;
        Box::new(std::iter::from_fn(move || {
            if done {
                return None;
            }
            match self.next_cells() {
                Err(e) => {
                    done = true;
                    Some(Err(e))
                }
                Ok(None) => {
                    done = true;
                    None
                }
                Ok(Some((line, mut cells))) => {
                    yielded += 1;
                    if let Some(max_rows) = limits.max_rows
                        && yielded > max_rows
                    {
                        done = true;
                        return Some(Err(ImportError::TooManyRows { limit: max_rows }));
                    }
                    while cells.len() < width {
                        cells.push(Cell::Missing);
                    }
                    // Jagged rows: longer than the header is cut (the docs promise a row
                    // at the header's width; the extra values name no column and cannot be
                    // mapped — keeping them would only shift later rows' shape).
                    cells.truncate(width);
                    Some(Ok(Row {
                        number: line,
                        cells,
                    }))
                }
            }
        }))
    }
}

/// Writes `bytes` to a temp file and returns its path, for tests that build files inline.
#[cfg(test)]
pub(crate) fn temp_file(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    // Per process: concurrent CI jobs on one machine share the temp directory.
    let dir = std::env::temp_dir().join(format!("spot-import-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("temp dir: {e}"));
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("temp file: {e}"));
    path
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::source::open_err;

    /// Collects a source's rows, failing on any reading error (tests read well-formed files).
    fn all_rows<S: RowSource>(source: &mut S) -> Vec<Row> {
        source
            .rows()
            .map(|r| r.unwrap_or_else(|e| panic!("row: {e:?}")))
            .collect()
    }

    /// A comma CSV with a quoted field holding the delimiter and a newline.
    #[test]
    fn csv_basic_and_quoted() {
        let path = temp_file(
            "csv_basic.csv",
            b"name,notes\r\nAcme,\"two, commas\"\r\nBeta,\"line\nbreak\"\r\n",
        );
        let mut source = CsvSource::open(&path, &Limits::default()).expect("open");
        assert_eq!(source.headers(), &["name".to_owned(), "notes".to_owned()]);
        let rows = all_rows(&mut source);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            Row {
                number: 2,
                cells: vec![Cell::Text("Acme".into()), Cell::Text("two, commas".into())],
            }
        );
        // Beta's record starts on the physical line 3 (its quoted field holds a newline).
        assert_eq!(rows[1].number, 3);
        assert_eq!(rows[1].cells[1].as_str(), "line\nbreak");
    }

    /// Semicolon and tab delimiters are detected; an explicit one wins over detection.
    #[test]
    fn csv_delimiters() {
        let semicolon = temp_file("csv_semi.csv", b"a;b\n1;2\n");
        let mut source = CsvSource::open(&semicolon, &Limits::default()).expect("open");
        assert_eq!(all_rows(&mut source)[0].cells[1].as_str(), "2");

        let tab = temp_file("csv_tab.csv", b"a\tb\n1\t2\n");
        let mut source = CsvSource::open(&tab, &Limits::default()).expect("open");
        assert_eq!(all_rows(&mut source)[0].cells[1].as_str(), "2");

        // With `;` pinned, a comma is ordinary cell text (the csv crate agrees: a quote
        // after field text is literal, so `x,"y,z"` is one field). Detection agrees too:
        // the quoted comma is field content, so the sample's quotes tip the tie to `;`.
        let tricky = temp_file("csv_tricky.csv", b"a;b\nx,\"y,z\"\n");
        let limits = Limits {
            delimiter: Delimiter::Byte(b';'),
            ..Limits::default()
        };
        let mut source = CsvSource::open(&tricky, &limits).expect("open");
        assert_eq!(all_rows(&mut source)[0].cells[0].as_str(), "x,\"y,z\"");
        let mut source = CsvSource::open(&tricky, &Limits::default()).expect("open");
        assert_eq!(all_rows(&mut source)[0].cells[0].as_str(), "x,\"y,z\"");

        // A real comma file with quoted commas detects `,` and reads the cell whole.
        let commas = temp_file("csv_commas.csv", b"a,b\nx,\"y,z\"\n");
        let mut source = CsvSource::open(&commas, &Limits::default()).expect("open");
        let row = all_rows(&mut source)[0].clone();
        assert_eq!(row.cells[0].as_str(), "x");
        assert_eq!(row.cells[1].as_str(), "y,z");
    }

    /// A UTF-8 BOM is stripped; blank lines are skipped and never numbered.
    #[test]
    fn csv_bom_and_blank_lines() {
        let path = temp_file("csv_bom.csv", b"\xEF\xBB\xBFa,b\n1,2\n\n\n3,4\n");
        let mut source = CsvSource::open(&path, &Limits::default()).expect("open");
        assert_eq!(source.headers()[0], "a");
        let rows = all_rows(&mut source);
        assert_eq!(
            rows.iter().map(|r| r.number).collect::<Vec<_>>(),
            vec![2, 5]
        );
    }

    /// A short row is padded with [`Cell::Missing`]; missing is distinct from an empty text.
    #[test]
    fn csv_short_rows_padded() {
        let path = temp_file("csv_short.csv", b"a,b,c\n1\n2,3\n");
        let mut source = CsvSource::open(&path, &Limits::default()).expect("open");
        let rows = all_rows(&mut source);
        assert_eq!(
            rows[0].cells,
            vec![Cell::Text("1".into()), Cell::Missing, Cell::Missing,]
        );
        assert_eq!(
            rows[1].cells,
            vec![
                Cell::Text("2".into()),
                Cell::Text("3".into()),
                Cell::Missing
            ]
        );
    }

    /// An empty-string cell is present but empty (distinct from a missing column).
    #[test]
    fn csv_empty_cell_is_present() {
        let path = temp_file("csv_empty_cell.csv", b"a,b\n1,\n");
        let mut source = CsvSource::open(&path, &Limits::default()).expect("open");
        let rows = all_rows(&mut source);
        assert_eq!(rows[0].cells[1], Cell::Text(String::new()));
    }

    /// Quotes: doubled quotes, mid-field literal quotes, quoted delimiters and newlines.
    #[test]
    fn csv_quote_shapes() {
        let path = temp_file(
            "csv_quotes.csv",
            b"a,b\nx,\"say \"\"hi\"\"\"\nz,plain\"quote\nw,\"deli,inside\"\n",
        );
        let mut source = CsvSource::open(&path, &Limits::default()).expect("open");
        let rows = all_rows(&mut source);
        assert_eq!(rows[0].cells[1].as_str(), "say \"hi\"");
        assert_eq!(rows[1].cells[1].as_str(), "plain\"quote");
        assert_eq!(rows[2].cells[1].as_str(), "deli,inside");
    }

    /// Invalid UTF-8 fails the whole import with `import.bad_encoding`.
    #[test]
    fn csv_bad_encoding() {
        let path = temp_file("csv_bad.csv", b"a,b\n1,\xFF\xFE\n");
        let mut source = CsvSource::open(&path, &Limits::default()).expect("open");
        let error = source.rows().next().expect("row").expect_err("bad utf-8");
        assert_eq!(error.code(), crate::error::BAD_ENCODING);
        assert!(error.to_string().contains("row 2"));
    }

    /// Over-limit files, rows, columns and cells fail with their typed codes.
    #[test]
    fn csv_limits() {
        let missing = temp_file("csv_limit_missing.csv", b"a\n1\n");
        let limits = Limits::platform(Some(0), None);
        let error = open_err::<CsvSource>(&missing, &limits);
        assert_eq!(error.code(), crate::error::FILE_TOO_LARGE);

        let rows = temp_file("csv_limit_rows.csv", b"a\n1\n2\n3\n4\n5\n");
        let limits = Limits::platform(None, Some(3));
        let mut source = CsvSource::open(&rows, &limits).expect("open");
        let mut stream = source.rows();
        for _ in 0..3 {
            stream.next().expect("row").expect("row ok");
        }
        let error = stream.next().expect("row").expect_err("too many rows");
        assert_eq!(error.code(), crate::error::TOO_MANY_ROWS);

        // The header itself is over the column cap, so the failure comes at open.
        let cols = temp_file("csv_limit_cols.csv", b"a,b,c,d\n1,2,3,4\n");
        let limits = Limits::tiny();
        let error = open_err::<CsvSource>(&cols, &limits);
        assert_eq!(error.code(), crate::error::TOO_MANY_COLUMNS);

        let long = temp_file("csv_limit_cell.csv", b"a\n123456789\n");
        let limits = Limits::tiny();
        let mut source = CsvSource::open(&long, &limits).expect("open");
        let error = source.rows().next().expect("row").expect_err("too long");
        assert_eq!(error.code(), crate::error::CELL_TOO_LONG);
    }

    /// A file with no readable rows has no header.
    #[test]
    fn csv_no_header() {
        let path = temp_file("csv_empty.csv", b"");
        let error = open_err::<CsvSource>(&path, &Limits::default());
        assert_eq!(error.code(), crate::error::NO_HEADER);
    }

    /// A blank-lines-only file has no header either.
    #[test]
    fn csv_blank_only_no_header() {
        let path = temp_file("csv_blank_only.csv", b"\n\n\n");
        let error = open_err::<CsvSource>(&path, &Limits::default());
        assert_eq!(error.code(), crate::error::NO_HEADER);
    }

    /// A missing file reports `import.file_not_found`.
    #[test]
    fn csv_missing_file() {
        let path = std::path::Path::new("/no/such/file.csv");
        let error = open_err::<CsvSource>(path, &Limits::default());
        assert_eq!(error.code(), crate::error::FILE_NOT_FOUND);
    }

    /// Many records across the parser's refill window keep exact numbering.
    #[test]
    fn csv_crosses_refill_windows() {
        let mut bytes = b"n\n".to_vec();
        for i in 0..5000 {
            bytes.extend_from_slice(format!("{i}\n").as_bytes());
        }
        let path = temp_file("csv_windows.csv", &bytes);
        let mut source = CsvSource::open(&path, &Limits::default()).expect("open");
        let rows = all_rows(&mut source);
        assert_eq!(rows.len(), 5000);
        assert_eq!(rows[1022].number, 1024);
        assert_eq!(rows[4999].number, 5001);
    }

    /// The delimiter probe: commas win ties, semicolons win on count, tabs too.
    #[test]
    fn delimiter_detection_order() {
        assert_eq!(detect_delimiter(b"a,b;c\n"), b',');
        assert_eq!(detect_delimiter(b"a;b,c\n1;2;3\n"), b';');
        assert_eq!(detect_delimiter(b"a\tb,c\n1\t2\t3\n"), b'\t');
        // Quotes keep their commas out of the count.
        assert_eq!(detect_delimiter(b"a;b,\"x,y\"\n"), b';');
        assert_eq!(detect_delimiter(b"plain\n"), b',');
    }
}

#[cfg(test)]
mod proptest_roundtrip {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestCaseError;

    /// The cell texts the property generates: quotes, commas, semicolons, tabs, newlines,
    /// unicode, empty. Bounded (<= 120 chars) to keep runs fast; the interesting shapes are
    /// all reachable.
    fn cell_text() -> impl Strategy<Value = String> {
        prop::string::string_regex("[^\\r]{0,120}").unwrap()
    }

    /// A row the reader skips as a blank line: one empty cell, or one cell holding only a
    /// U+FEFF (written unquoted, it is stripped as the file's BOM when it opens the file).
    fn blank_line(row: &[String]) -> bool {
        row.len() == 1 && (row[0].is_empty() || row[0] == "\u{feff}")
    }

    /// Escapes a cell the way a simple RFC 4180 writer does: quote-wrap only when needed,
    /// double the quotes inside. (The test's writer, not the code under test: the parser
    /// must read back whatever a straightforward producer emits.)
    fn write_cell(out: &mut Vec<u8>, cell: &str, delimiter: u8) {
        let delimiter = delimiter as char;
        let needed = cell.contains([delimiter, '"', '\n']);
        if needed {
            out.push(b'"');
            for c in cell.chars() {
                if c == '"' {
                    out.push(b'"');
                }
                out.extend_from_slice(c.to_string().as_bytes());
            }
            out.push(b'"');
        } else {
            out.extend_from_slice(cell.as_bytes());
        }
    }

    /// The escaped file one column per property run: with an EXPLICIT delimiter (the
    /// detected one is a separate, covered behaviour: a tab character in a cell is
    /// indistinguishable from a tab delimiter to any detector).
    fn roundtrip_case(meaningful: &[Vec<String>], delimiter: u8) -> Result<(), TestCaseError> {
        let width = meaningful[0].len();
        let pad = |mut row: Vec<String>| {
            row.resize(width, String::new());
            row
        };
        // The reader strips a UTF-8 BOM from the START of the file (`skip_bom`), and the
        // test's writer quotes a first cell only when it holds the delimiter, a quote or a
        // newline — so an unquoted header cell that begins with U+FEFF loses that BOM on
        // the way back in. That is exactly one asymmetry; the expectation mirrors it and
        // nothing else.
        let mut header = meaningful[0].clone();
        if let Some(first) = header.first_mut() {
            let unquoted =
                !first.contains(delimiter as char) && !first.contains('"') && !first.contains('\n');
            if unquoted && let Some(stripped) = first.strip_prefix('\u{feff}') {
                *first = stripped.to_owned();
            }
        }
        let expected: Vec<Vec<String>> = meaningful[1..].iter().map(|r| pad(r.clone())).collect();

        let mut bytes = Vec::new();
        for (i, row) in meaningful.iter().enumerate() {
            if i > 0 {
                bytes.push(b'\n');
            }
            for (j, cell) in row.iter().enumerate() {
                if j > 0 {
                    bytes.push(delimiter);
                }
                write_cell(&mut bytes, cell, delimiter);
            }
        }
        bytes.push(b'\n');
        let name = format!("prop_roundtrip_{delimiter}.csv");
        let path = temp_file(&name, &bytes);
        let limits = Limits {
            delimiter: Delimiter::Byte(delimiter),
            ..Limits::default()
        };
        let mut source = CsvSource::open(&path, &limits)
            .map_err(|e| TestCaseError::fail(format!("open: {e}")))?;
        // Row 1 is the header row and must match exactly (a short header is padded
        // to its own width, which is already `header`).
        prop_assert_eq!(source.headers(), &header);
        let read: Vec<Vec<String>> = source
            .rows()
            .map(|r| {
                r.map(|row| {
                    row.cells
                        .iter()
                        .map(Cell::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .map_err(|e| TestCaseError::fail(format!("row: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        prop_assert_eq!(read.len(), expected.len());
        prop_assert_eq!(read, expected);
        Ok(())
    }

    /// A file of arbitrary cells round-trips: the reader's cells equal the writer's, row
    /// for row (the header row too — the test reads it through `headers()`). Short rows are
    /// padded to the header's width with MISSING cells and long rows are cut, so the
    /// expectation pads them the same way.
    #[test]
    fn csv_roundtrips_arbitrary_cells() {
        proptest!(|(rows in prop::collection::vec(
            prop::collection::vec(cell_text(), 1..6), 0..20))| {
            // Skip rows that would be BLANK lines (a single empty cell): the reader skips
            // those by design.
            let meaningful: Vec<Vec<String>> = rows.into_iter()
                .filter(|r| !blank_line(r))
                .collect();
            // The first (header) row is required; rejecting is not a failure.
            prop_assume!(!meaningful.is_empty(), "no meaningful rows in case");
            roundtrip_case(&meaningful, b',')?
        })
    }

    /// The same property over ';' — the delimiter is quoted around like every other text.
    #[test]
    fn csv_roundtrips_arbitrary_cells_semicolon() {
        proptest!(|(rows in prop::collection::vec(
            prop::collection::vec(cell_text(), 1..6), 0..20))| {
            let meaningful: Vec<Vec<String>> = rows.into_iter()
                .filter(|r| !blank_line(r))
                .collect();
            prop_assume!(!meaningful.is_empty(), "no meaningful rows in case");
            roundtrip_case(&meaningful, b';')?
        })
    }
}
