//! The wire text of an export: CSV and NDJSON, exact and round-trippable.
//!
//! The forms are chosen so a file exported here imports back through `spot-import` identically:
//!
//! - Decimals are their **exact** text (`1.50`, never `1.5`, never a float — ADR-12).
//! - Instants are UTC RFC 3339 with **six** fractional digits (`2026-01-31T10:00:00.000000Z`);
//!   `spot-import`'s `%S`-ending datetime formats accept the fraction and the `Z`.
//! - Dates are ISO `%Y-%m-%d`; times are `%H:%M:%S` with a fraction only when the stored time
//!   has a sub-second part.
//! - A link id is the canonical hyphenated UUIDv7 text, which `spot-import` reads back as an
//!   id.
//! - `Null` is an empty CSV field and JSON `null`.
//!
//! CSV quoting is RFC 4180: a field holding a comma, a quote or a newline is wrapped in quotes
//! and its quotes doubled, so a field of only commas, newlines and quotes survives. A **text**
//! cell that begins with `=`, `+`, `-`, `@`, a tab or a carriage return is prefixed with a
//! single quote (`'`) to defuse spreadsheet formula injection; this is switchable and applies
//! to CSV only (NDJSON is not a spreadsheet target).

use base64::Engine as _;
use serde_json::Value as JsonValue;
use spot_storage_contract::Value;
use time::UtcOffset;

/// The file format of an export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Comma-separated values, RFC 4180, LF line endings, a header row.
    Csv,
    /// One JSON object per line, keys in column order.
    Ndjson,
}

impl Format {
    /// The stable lower-case name used in the plan digest and audit records.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::Ndjson => "ndjson",
        }
    }
}

/// The characters that start a spreadsheet formula and are defused with a leading `'`.
const FORMULA_STARTS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// The CSV header row (with a trailing newline), or nothing for NDJSON (the keys are inline).
#[must_use]
pub fn header(columns: &[String], format: Format) -> Vec<u8> {
    match format {
        Format::Ndjson => Vec::new(),
        Format::Csv => {
            let mut out = Vec::new();
            for (i, name) in columns.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_csv_cell(&mut out, name, false);
            }
            out.push(b'\n');
            out
        }
    }
}

/// One row's bytes (with a trailing newline) in `format`.
///
/// `columns` names the cells (NDJSON keys); `values` is one value per column, in order. A
/// shorter `values` is padded with [`Value::Null`].
#[must_use]
pub fn row(columns: &[String], values: &[Value], format: Format, hardening: bool) -> Vec<u8> {
    match format {
        Format::Csv => {
            let mut out = Vec::new();
            for (i, _) in columns.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                let value = values.get(i).unwrap_or(&Value::Null);
                let text = value_text(value);
                // Hardening applies to text cells only: a number that starts with `-` is not a
                // formula, and a date or an id is not text.
                let harden = hardening && matches!(value, Value::Text(_));
                write_csv_cell(&mut out, &text, harden);
            }
            out.push(b'\n');
            out
        }
        Format::Ndjson => {
            let mut out = Vec::new();
            out.push(b'{');
            for (i, name) in columns.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_json_string(&mut out, name);
                out.push(b':');
                let value = values.get(i).unwrap_or(&Value::Null);
                write_json_scalar(&mut out, value);
            }
            out.extend_from_slice(b"}\n");
            out
        }
    }
}

/// The exact text form of a value for CSV.
#[must_use]
pub fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
        Value::Int(n) => n.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Text(s) => s.clone(),
        Value::Date(d) => format_date(*d),
        Value::Time(t) => format_time(*t),
        Value::DateTime(dt) => format_datetime(*dt),
        Value::Id(id) => id.to_string(),
        Value::Json(j) => j.clone(),
        Value::Bytes(b) => base64::engine::general_purpose::STANDARD.encode(b),
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug form.
        Value::GeoPoint(_) | Value::Range(_) => format!("{value:?}"),
    }
}

/// `%Y-%m-%d`.
#[must_use]
pub fn format_date(d: time::Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day())
}

/// `%H:%M:%S`, with a fraction only when the time has a sub-second part.
#[must_use]
pub fn format_time(t: time::Time) -> String {
    let base = format!("{:02}:{:02}:{:02}", t.hour(), t.minute(), t.second());
    match fraction_text(t.nanosecond()) {
        Some(frac) => format!("{base}.{frac}"),
        None => base,
    }
}

/// UTC RFC 3339 with six fractional digits.
#[must_use]
pub fn format_datetime(dt: time::OffsetDateTime) -> String {
    let dt = dt.to_offset(UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second(),
        dt.nanosecond() / 1_000
    )
}

/// The minimal fraction text for `nanos` (trailing zeros dropped), or `None` for zero.
fn fraction_text(nanos: u32) -> Option<String> {
    if nanos == 0 {
        return None;
    }
    let mut digits = format!("{nanos:09}");
    while digits.ends_with('0') {
        digits.pop();
    }
    Some(digits)
}

/// Writes one CSV cell, quoting when it holds a comma, a quote, a carriage return or a
/// newline. `harden` prefixes a leading formula character with `'`.
fn write_csv_cell(out: &mut Vec<u8>, text: &str, harden: bool) {
    let prefixed = harden
        && text
            .chars()
            .next()
            .is_some_and(|c| FORMULA_STARTS.contains(&c));
    if prefixed {
        out.push(b'\'');
    }
    if prefixed || text.contains([',', '"', '\n', '\r']) {
        out.push(b'"');
        for b in text.bytes() {
            if b == b'"' {
                out.push(b'"');
            }
            out.push(b);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(text.as_bytes());
    }
}

/// Writes a JSON string (escaped) to `out`.
fn write_json_string(out: &mut Vec<u8>, s: &str) {
    // `serde_json`'s string escaping is exactly what JSON needs and never a float.
    let quoted = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned());
    out.extend_from_slice(quoted.as_bytes());
}

/// Writes one value as a JSON scalar. Decimals, dates, times, instants, ids and bytes are JSON
/// strings (exact text); integers and bools are JSON scalars; JSON text is embedded when it is
/// valid JSON, otherwise quoted.
fn write_json_scalar(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
        Value::Decimal(d) => write_json_string(out, &d.to_string()),
        Value::Text(s) => write_json_string(out, s),
        Value::Date(d) => write_json_string(out, &format_date(*d)),
        Value::Time(t) => write_json_string(out, &format_time(*t)),
        Value::DateTime(dt) => write_json_string(out, &format_datetime(*dt)),
        Value::Id(id) => write_json_string(out, &id.to_string()),
        Value::Json(j) => {
            if serde_json::from_str::<JsonValue>(j).is_ok() {
                out.extend_from_slice(j.as_bytes());
            } else {
                write_json_string(out, j);
            }
        }
        Value::Bytes(b) => {
            write_json_string(out, &base64::engine::general_purpose::STANDARD.encode(b))
        }
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug form.
        Value::GeoPoint(_) | Value::Range(_) => write_json_string(out, &format!("{value:?}")),
    }
}
