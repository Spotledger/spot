//! Text to [`Value`] conversion per field type (brief item 4).
//!
//! Every cell of a mapped column crosses [`convert`], which turns the cell's text into the
//! field's [`Value`] (or into "unset" for an empty cell) and refuses anything that does not
//! fit. The rules are deliberately narrow:
//!
//! - **No floating point anywhere** (ADR-12): decimals come from `Decimal::from_str_exact`
//!   after a strict shape check (sign, digits, one dot — no underscores `"1_000"`, no
//!   exponents `"1e3"`, no leading or trailing spaces), and the cell may not hold more
//!   fraction digits than the field's `scale` allows (extra places are an error, never
//!   rounded).
//! - Integers are `i64` texts with the same shape rule.
//! - Bools are words from a configurable pair of lists (`yes/no/true/false/1/0` by default),
//!   compared case-insensitively; the cell is trimmed for this one comparison because a bool
//!   word is a token, not data (text values are never trimmed).
//! - Dates, datetimes and times are parsed with an **explicit list of formats** and no
//!   day/month guessing: every configured format is tried, and when more than one matches
//!   with a different result the text is ambiguous (`import.ambiguous_date`) unless the user
//!   names a single format. Formats use `time`'s strftime syntax
//!   (`parse_strftime_borrowed`); `%S`-ending formats additionally accept a fractional
//!   second (1–9 digits after `.` or `,`), which `time`'s strftime subset cannot express;
//!   `%f`/`%.f` are not valid here (probed on time 0.3.55). A datetime may end with `Z` or
//!   an offset `±HH`, `±HHMM` or `±HH:MM`, which is applied as an integer minute shift — the
//!   result is always the UTC instant the wall-clock time names.
//! - Selects match an option exactly, or case-insensitively when configured; the canonical
//!   option's spelling is stored in that case, so the report and the file may disagree while
//!   the document stays clean.
//! - Links are kept as the given string: a canonical lower-case hyphenated UUIDv7 becomes
//!   [`Value::Id`], anything else stays [`Value::Text`] for the writer to resolve (WP-DOC-09
//!   item 4: existence checks need storage and belong to the writer). A text link makes
//!   `validate::check` report the field — the honest answer while this crate cannot look
//!   the name up.
//! - An empty string is unset (null) per `empty_to_null`; a *missing* column is always
//!   unset. With `empty_to_null = false` an empty cell is empty **text**: valid for text
//!   fields, a bad value for every other type.
//! - JSON keeps its text as [`Value::Json`] (validity is checked at storage write); bytes
//!   cannot be imported from cell text at all (`import.bad_value`).

use rust_decimal::Decimal;
use spot_app_api::prelude::Value;
use spot_meta::{FieldSpec, FieldType};
use time::format_description::BorrowedFormatItem;
use time::{Date, OffsetDateTime, PrimitiveDateTime, Time};

use crate::error::{BAD_VALUE, ImportError};

/// The words a bool cell means by: lowercased at build time, compared case-insensitively.
#[derive(Clone, Debug)]
pub struct BoolWords {
    /// Words for `true` (lowercase).
    pub true_words: Vec<String>,
    /// Words for `false` (lowercase).
    pub false_words: Vec<String>,
}

impl Default for BoolWords {
    fn default() -> Self {
        Self {
            true_words: ["yes", "true", "1"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            false_words: ["no", "false", "0"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

/// The caller's conversion choices (brief item 4: configurable bool words, optional
/// case-insensitive select, empty string to null per option).
#[derive(Clone, Debug)]
pub struct ConversionOptions {
    /// The bool word lists.
    pub bool_words: BoolWords,
    /// Select values also match case-insensitively; the canonical option is stored.
    pub select_case_insensitive: bool,
    /// An empty cell is unset (null); `false` keeps empty text as a value.
    pub empty_to_null: bool,
}

impl Default for ConversionOptions {
    fn default() -> Self {
        Self {
            bool_words: BoolWords::default(),
            select_case_insensitive: false,
            empty_to_null: true,
        }
    }
}

/// One compiled format: the parsed description and whether the format string ended in `%S`
/// (that allows attaching a fractional second, which the strftime subset cannot express).
#[derive(Debug, Clone)]
struct FormatEntry {
    item: BorrowedFormatItem<'static>,
    seconds: bool,
}

/// The compiled format lists (brief: explicit format list, no guessing, ambiguous dates are
/// an error unless one format is given). Built once per plan, so cell conversion never
/// re-parses a format description; a format `time` cannot read fails the plan build with
/// [`ImportError::Meta`] before any row runs.
#[derive(Clone, Debug)]
pub struct Formats {
    /// Formats for `date` fields.
    date: Vec<FormatEntry>,
    /// Formats for `datetime` fields.
    datetime: Vec<FormatEntry>,
    /// Formats for `time` fields.
    time: Vec<FormatEntry>,
}

impl Formats {
    /// Compiles the three format lists. Defaults: `%Y-%m-%d` (dates), the ISO and
    /// space-separated datetime forms with and without seconds, `%H:%M:%S` and `%H:%M`.
    pub fn build(
        date: &[String],
        datetime: &[String],
        time_of_day: &[String],
    ) -> Result<Self, ImportError> {
        Ok(Self {
            date: compile(date)?,
            datetime: compile(datetime)?,
            time: compile(time_of_day)?,
        })
    }

    /// The brief's defaults, as a set the caller can hand back to [`Formats::build`].
    pub fn default_lists() -> (Vec<String>, Vec<String>, Vec<String>) {
        (
            vec!["%Y-%m-%d".to_owned()],
            vec![
                "%Y-%m-%dT%H:%M:%S".to_owned(),
                "%Y-%m-%d %H:%M:%S".to_owned(),
                "%Y-%m-%dT%H:%M".to_owned(),
                "%Y-%m-%d %H:%M".to_owned(),
            ],
            vec!["%H:%M:%S".to_owned(), "%H:%M".to_owned()],
        )
    }
}

/// Leaks one format string to `'static` and parses it, so entries own nothing they borrow.
/// A handful of short strings per import; the plan outlives every cell. The description's
/// items are joined into ONE format item (`parse_strftime_borrowed` hands the
/// sequence back as a slice, and parsing against `&slice[..]` rejects a text the
/// per-item parse would accept with trailing characters).
fn compile(list: &[String]) -> Result<Vec<FormatEntry>, ImportError> {
    list.iter()
        .map(|f| {
            let leaked: &'static str = Box::leak(f.clone().into_boxed_str());
            let parsed =
                time::format_description::parse_strftime_borrowed(leaked).map_err(|e| {
                    ImportError::Meta {
                        message: format!("the date/time format `{f}` is not supported: {e}"),
                    }
                })?;
            let joined: Vec<BorrowedFormatItem<'static>> = parsed.to_vec();
            let boxed: Box<[BorrowedFormatItem<'static>]> = joined.into_boxed_slice();
            let item: BorrowedFormatItem<'static> = BorrowedFormatItem::Compound(Box::leak(boxed));
            Ok(FormatEntry {
                item,
                seconds: leaked.ends_with("%S"),
            })
        })
        .collect()
}

/// One cell's failure: its stable code and a message naming the column and cell text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversionError {
    /// `import.bad_value` or `import.ambiguous_date`.
    pub code: &'static str,
    /// What the caller can tell the user.
    pub message: String,
}

impl ConversionError {
    /// A plain `import.bad_value` with a message.
    fn bad(message: impl Into<String>) -> Self {
        Self {
            code: BAD_VALUE,
            message: message.into(),
        }
    }
}

/// Converts one cell's text for `spec`.
///
/// `Ok(None)` is "the field is unset": a missing column, or an empty cell when
/// `empty_to_null` is on. Every other outcome is a [`Value`] or a [`ConversionError`].
pub fn convert(
    spec: &FieldSpec,
    cell: &str,
    options: &ConversionOptions,
    formats: &Formats,
) -> Result<Option<Value>, ConversionError> {
    if cell.is_empty() {
        if options.empty_to_null {
            return Ok(None);
        }
        return match spec.ty {
            FieldType::Text | FieldType::LongText | FieldType::Select => {
                Ok(Some(Value::Text(String::new())))
            }
            _ => Err(ConversionError::bad(format!(
                "field `{}` is empty, which only text fields accept as a value",
                spec.name
            ))),
        };
    }
    let value = match spec.ty {
        FieldType::Bool => bool_value(spec, cell, options)?,
        FieldType::Int => int_value(spec, cell)?,
        FieldType::Decimal => decimal_value(spec, cell)?,
        FieldType::Text | FieldType::LongText => Value::Text(cell.to_owned()),
        FieldType::Select => select_value(spec, cell, options)?,
        FieldType::Date => {
            Value::Date(parse_date(cell, &formats.date).map_err(|r| refusal_error(r, spec, cell))?)
        }
        FieldType::Time => {
            Value::Time(parse_time(cell, &formats.time).map_err(|r| refusal_error(r, spec, cell))?)
        }
        FieldType::DateTime => Value::DateTime(
            parse_datetime(cell, &formats.datetime).map_err(|r| refusal_error(r, spec, cell))?,
        ),
        // An attach field stores a reference to a file record (ADR-068 §2): its cell is the id.
        FieldType::Link | FieldType::DynamicLink | FieldType::Attach => link_value(cell)?,
        FieldType::Json => Value::Json(cell.to_owned()),
        FieldType::Bytes => {
            return Err(ConversionError::bad(format!(
                "field `{}` holds bytes, which cannot be imported from cell text",
                spec.name
            )));
        }
        // No cell-text form exists yet for a position or a range.
        FieldType::Geo | FieldType::Range => {
            return Err(ConversionError::bad(format!(
                "field `{}` holds a position or a range, which has no cell-text form yet",
                spec.name
            )));
        }
        // A table's columns are child fields; a table itself is never a mapped value.
        FieldType::Table | FieldType::TableMultiselect => {
            return Err(ConversionError::bad(format!(
                "field `{}` is a table and holds rows, not a value",
                spec.name
            )));
        }
        // A secret has no column: the secrets service holds it, never a document (ADR-068 §2).
        FieldType::Secret => {
            return Err(ConversionError::bad(format!(
                "field `{}` is a secret and cannot be imported; set it through the secrets service",
                spec.name
            )));
        }
    };
    Ok(Some(value))
}

fn bad_text(spec: &FieldSpec, cell: &str) -> String {
    format!(
        "field `{}` ({:?}) cannot read the text `{cell}` in any configured format",
        spec.name, spec.ty
    )
}

fn bool_value(
    spec: &FieldSpec,
    cell: &str,
    options: &ConversionOptions,
) -> Result<Value, ConversionError> {
    let word = cell.trim().to_ascii_lowercase();
    if options.bool_words.true_words.iter().any(|w| w == &word) {
        return Ok(Value::Bool(true));
    }
    if options.bool_words.false_words.iter().any(|w| w == &word) {
        return Ok(Value::Bool(false));
    }
    Err(ConversionError::bad(format!(
        "field `{}` needs {}, but the cell holds `{cell}`",
        spec.name,
        options
            .bool_words
            .true_words
            .iter()
            .chain(options.bool_words.false_words.iter())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("/")
    )))
}

/// Number shape: optional sign, digits, at most one dot, nothing else — `1_000` and `1e3`
/// are refused even though `Decimal::from_str_exact` would take them.
fn number_shape(cell: &str) -> bool {
    let body = cell.strip_prefix(['+', '-']).unwrap_or(cell);
    let mut parts = body.split('.');
    let (Some(int), rest) = (parts.next(), parts.next()) else {
        return false;
    };
    let ok = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    (ok(int) || (int.is_empty() && rest.is_some()))
        && rest.is_none_or(ok)
        && parts.next().is_none()
        && body.contains(|b: char| b.is_ascii_digit())
}

fn int_value(spec: &FieldSpec, cell: &str) -> Result<Value, ConversionError> {
    if !number_shape(cell) {
        return Err(ConversionError::bad(bad_text(spec, cell)));
    }
    let value = cell.parse::<i64>().map_err(|_| {
        ConversionError::bad(format!(
            "field `{}` cannot hold `{cell}`: it does not fit a 64-bit integer",
            spec.name
        ))
    })?;
    Ok(Value::Int(value))
}

fn decimal_value(spec: &FieldSpec, cell: &str) -> Result<Value, ConversionError> {
    if !number_shape(cell) {
        return Err(ConversionError::bad(bad_text(spec, cell)));
    }
    let places = cell.split_once('.').map_or(0, |(_, frac)| frac.len());
    let scale = usize::from(spec.scale.unwrap_or(0));
    if places > scale {
        return Err(ConversionError::bad(format!(
            "field `{}` allows {scale} decimal places, but `{cell}` has {places}; shorten it instead of rounding",
            spec.name
        )));
    }
    let value =
        Decimal::from_str_exact(cell).map_err(|_| ConversionError::bad(bad_text(spec, cell)))?;
    Ok(Value::Decimal(value))
}

fn select_value(
    spec: &FieldSpec,
    cell: &str,
    options: &ConversionOptions,
) -> Result<Value, ConversionError> {
    let values = spec.select_values();
    if options.select_case_insensitive {
        let lower = cell.to_ascii_lowercase();
        if let Some(found) = values.iter().find(|v| v.to_ascii_lowercase() == lower) {
            return Ok(Value::Text(found.clone()));
        }
        return Err(ConversionError::bad(format!(
            "field `{}` takes one of {}, but the cell holds `{cell}`",
            spec.name,
            values.join(", ")
        )));
    }
    if values.iter().any(|v| v == cell) {
        return Ok(Value::Text(cell.to_owned()));
    }
    Err(ConversionError::bad(format!(
        "field `{}` takes one of {}, but the cell holds `{cell}`",
        spec.name,
        values.join(", ")
    )))
}

/// Kept as the given string: a canonical UUIDv7 becomes a link id, anything else stays text
/// for the writer's lookup (existence checks need storage and belong to the writer).
fn link_value(cell: &str) -> Result<Value, ConversionError> {
    let Some(bytes) = canonical_uuid_v7(cell) else {
        return Ok(Value::Text(cell.to_owned()));
    };
    let id = spot_app_api::prelude::RecordId::from_bytes(bytes).map_err(|_| {
        ConversionError::bad(format!("the link value `{cell}` is not a valid UUIDv7"))
    })?;
    Ok(Value::Id(id))
}

/// The 16 bytes of a canonical lower-case hyphenated UUIDv7 text, or `None` for anything else
/// (this module has no `uuid` dependency: the shape is 36 chars, four hyphens, hex digits,
/// version 7 at the highest nibble of the third group, variant `8..=b`).
fn canonical_uuid_v7(text: &str) -> Option<[u8; 16]> {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    for at in [8usize, 13, 18, 23] {
        if bytes[at] != b'-' {
            return None;
        }
    }
    let mut out = [0u8; 16];
    let mut pairs = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'-' {
            i += 1;
            continue;
        }
        // Guard the slot BEFORE storing: 16 pairs fill the 16 bytes and the 17th pair is
        // not a canonical UUID (a 37-hex-digit text cannot have matched the 36-char shape
        // with 4 hyphens, but the count must be refused, never wrapped).
        if pairs >= 16 {
            return None;
        }
        let hi = (bytes[i] as char).to_digit(16)?;
        let lo = (bytes[i + 1] as char).to_digit(16)?;
        out[pairs] = ((hi << 4) | lo) as u8;
        pairs += 1;
        // i advances by 2 below via the loop's tail; the `?` above consumed the pair check.
        i += 2;
    }
    if pairs != 16 || bytes[14] != b'7' || !matches!(bytes[19], b'8'..=b'b') {
        return None;
    }
    Some(out)
}

/// Splits a fractional second (1–9 digits after the last `.` or `,`) when one trails the
/// text: `(main text, nanoseconds)`. More than 9 digits or a missing digit run is `None`, so
/// the unsplit text fails every format and the cell is reported as a bad value.
fn split_fraction(text: &str) -> Option<(&str, u32)> {
    let cut = text
        .char_indices()
        .rev()
        .find(|(_, c)| *c == '.' || *c == ',')?
        .0;
    let frac = &text[cut + 1..];
    let digits = frac.len();
    if digits == 0 || digits > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u32 = frac.parse().ok()?;
    Some((
        &text[..cut],
        value * 10_u32.pow(u32::try_from(9 - digits).ok()?),
    ))
}

/// Splits a trailing instant offset: `Z`, `±HH:MM`, `±HHMM` or `±HH`, with hours below 24 and
/// minutes below 60. Returns the offset's signed minutes and the text before it. An
/// unmatched or invalid suffix is `None`, which lets every format reject the text.
fn split_offset(text: &str) -> Option<(i64, &str)> {
    if let Some(main) = text.strip_suffix('Z') {
        return Some((0, main));
    }
    let bytes = text.as_bytes();
    let sign_at = |back: usize| -> Option<usize> {
        bytes
            .len()
            .checked_sub(back)
            .filter(|&p| matches!(bytes[p], b'+' | b'-'))
    };
    // `±HH:MM`, then `±HHMM`, then `±HH`; the first *valid* shape wins, so `+05:00` is not
    // mistaken for `+05` followed by trailing text (the parse of any leftover would fail).
    if let Some(p) = sign_at(6) {
        let (h, m) = (text.get(p + 1..p + 3)?, text.get(p + 4..p + 6)?);
        if bytes[p + 3] == b':' && digits2(h) && digits2(m) {
            return checked_offset(p, text, h, m);
        }
    }
    if let Some(p) = sign_at(5) {
        let (h, m) = (text.get(p + 1..p + 3)?, text.get(p + 3..)?);
        if digits2(h) && digits2(m) {
            return checked_offset(p, text, h, m);
        }
    }
    if let Some(p) = sign_at(3) {
        let h = text.get(p + 1..)?;
        if digits2(h) {
            return checked_offset(p, text, h, "0");
        }
    }
    None
}

fn digits2(s: &str) -> bool {
    s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit())
}

fn checked_offset<'a>(p: usize, text: &'a str, h: &str, m: &str) -> Option<(i64, &'a str)> {
    let hours: i64 = h.parse().ok()?;
    let minutes: i64 = m.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let sign = if text.as_bytes()[p] == b'-' { -1 } else { 1 };
    Some((sign * (hours * 60 + minutes), &text[..p]))
}

/// Tries every date format: one distinct result wins, none is a bad value, and two
/// **different** results are ambiguous (`import.ambiguous_date`).
fn parse_date(cell: &str, formats: &[FormatEntry]) -> Result<Date, ParseRefusal> {
    let found: Vec<Date> = formats
        .iter()
        .filter_map(|f| Date::parse(cell, &f.item).ok())
        .collect();
    one_of(&found).copied().ok_or_else(|| parse_refusal(&found))
}

fn parse_time(cell: &str, formats: &[FormatEntry]) -> Result<Time, ParseRefusal> {
    let mut found = Vec::new();
    for f in formats {
        if let Ok(t) = Time::parse(cell, &f.item) {
            found.push(t);
        }
        if f.seconds
            && let Some((main, nanos)) = split_fraction(cell)
            && let Ok(t) = Time::parse(main, &f.item)
        {
            found.push(
                Time::from_hms_nano(t.hour(), t.minute(), t.second(), nanos)
                    .map_err(|_| ParseRefusal::NoMatch)?,
            );
        }
    }
    one_of(&found).copied().ok_or_else(|| parse_refusal(&found))
}

/// Datetimes: strip an offset, try every format (with fractions on `%S`-ending ones), attach
/// the fraction, and shift the assumed-UTC wall clock by the offset's signed minutes with
/// integer arithmetic only. Two distinct results are ambiguous; none is a bad value.
fn parse_datetime(cell: &str, formats: &[FormatEntry]) -> Result<OffsetDateTime, ParseRefusal> {
    let (offset_minutes, main) = split_offset(cell).unwrap_or((0, cell));
    let mut found = Vec::new();
    for f in formats {
        let push = |naive: PrimitiveDateTime| -> Result<OffsetDateTime, ParseRefusal> {
            // The offset suffix names UTC: the wall clock is `offset` AHEAD of UTC, so the
            // UTC instant is the wall clock MINUS the offset minutes (integer arithmetic
            // only; `10:00+03:00` is the instant `07:00Z` names). `assume_utc` stamps the
            // wall clock AS IF it were UTC; subtracting the shift moves to the real instant.
            let instant = naive.assume_utc();
            let stamp = instant.unix_timestamp() - offset_minutes * 60;
            OffsetDateTime::from_unix_timestamp(stamp).map_err(|_| ParseRefusal::NoMatch)
        };
        if let Ok(naive) = PrimitiveDateTime::parse(main, &f.item) {
            found.push(push(naive)?);
        }
        if f.seconds
            && let Some((body, nanos)) = split_fraction(main)
            && let Ok(naive) = PrimitiveDateTime::parse(body, &f.item)
            && let Ok(time) =
                Time::from_hms_nano(naive.hour(), naive.minute(), naive.second(), nanos)
        {
            found.push(push(naive.replace_time(time))?);
        }
    }
    one_of(&found).copied().ok_or_else(|| parse_refusal(&found))
}

/// Renders [`one_of`]'s two refusals (brief item 4): no format matched is `import.bad_value`,
/// two formats matching with different results is `import.ambiguous_date`. The brief calls
/// for the difference: ambiguity means the file's day/month order cannot be known and the
/// user must name one format, while a plain no-match means the text is just not a date.
enum ParseRefusal {
    /// Every configured format failed to read the text.
    NoMatch,
    /// More than one format read the text with different results.
    Ambiguous,
}

/// [`one_of`]'s `None` as a [`ParseRefusal`]: empty means nothing matched.
fn parse_refusal<T>(found: &[T]) -> ParseRefusal {
    if found.is_empty() {
        ParseRefusal::NoMatch
    } else {
        ParseRefusal::Ambiguous
    }
}

/// A [`ParseRefusal`] as the cell's [`ConversionError`], naming field and cell.
fn refusal_error(refusal: ParseRefusal, spec: &FieldSpec, cell: &str) -> ConversionError {
    match refusal {
        ParseRefusal::NoMatch => ConversionError::bad(bad_text(spec, cell)),
        ParseRefusal::Ambiguous => ConversionError {
            code: crate::error::AMBIGUOUS_DATE,
            message: format!(
                "field `{}` reads `{cell}` differently under two of the configured formats; configure a single format for the column",
                spec.name
            ),
        },
    }
}

/// The one value in `found`, or `None` when there is none or more than one distinct result.
fn one_of<T: PartialEq>(found: &[T]) -> Option<&T> {
    let first = found.first()?;
    found.iter().all(|v| v == first).then_some(first)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::error::AMBIGUOUS_DATE;

    /// A field spec by name and type, with the extra properties a test needs.
    fn spec(name: &str, ty: FieldType) -> FieldSpec {
        FieldSpec::new(name, ty)
    }

    fn decimal_spec(scale: u8) -> FieldSpec {
        let mut s = spec("total", FieldType::Decimal);
        s.scale = Some(scale);
        s
    }

    fn formats() -> Formats {
        let (d, dt, t) = Formats::default_lists();
        Formats::build(&d, &dt, &t).unwrap()
    }

    fn opts() -> ConversionOptions {
        ConversionOptions::default()
    }

    #[test]
    fn attach_is_a_reference_and_secret_and_multiselect_are_refused() {
        let f = formats();
        assert!(
            convert(&spec("scan", FieldType::Attach), "FILE-0001", &opts(), &f)
                .unwrap()
                .is_some()
        );
        for ty in [FieldType::Secret, FieldType::TableMultiselect] {
            let e = convert(&spec("x", ty), "anything", &opts(), &f).unwrap_err();
            assert_eq!(e.code, BAD_VALUE, "{ty:?}");
        }
    }

    #[test]
    fn decimals_are_exact_and_scale_strict() {
        let f = formats();
        assert_eq!(
            convert(&decimal_spec(2), "1.50", &opts(), &f).unwrap(),
            Some(Value::Decimal(Decimal::from_str_exact("1.50").unwrap()))
        );
        assert_eq!(
            convert(&decimal_spec(2), "42", &opts(), &f).unwrap(),
            Some(Value::Decimal(Decimal::from_str_exact("42").unwrap()))
        );
        // Extra places are an error, never rounded.
        let e = convert(&decimal_spec(2), "1.505", &opts(), &f).unwrap_err();
        assert_eq!(e.code, BAD_VALUE);
        assert!(
            e.message.contains("`1.505` has 3"),
            "the message must name the cell and its extra places: {}",
            e.message
        );
        // Shape rules: no underscores, no exponents, no spaces. `Decimal::from_str_exact`
        // would accept `1_000`; the shape check refuses it first. (An empty cell is unset
        // under the default `empty_to_null`, so it is not part of this list.)
        for bad in ["1_000", "1e3", " 1", "1 ", "1.2.3", "abc", "+-1", "."] {
            assert!(
                convert(&decimal_spec(2), bad, &opts(), &f).is_err(),
                "{bad} must fail"
            );
        }
        // `.5`-style inputs parse: digits exist somewhere in the body.
        assert!(convert(&decimal_spec(2), ".5", &opts(), &f).is_ok());
        // A negative with a fraction, sign kept.
        assert_eq!(
            convert(&decimal_spec(3), "-2.5", &opts(), &f).unwrap(),
            Some(Value::Decimal(Decimal::from_str_exact("-2.5").unwrap()))
        );
    }

    #[test]
    fn ints_refuse_fractions_and_overflow() {
        let f = formats();
        let s = spec("n", FieldType::Int);
        assert_eq!(
            convert(&s, "42", &opts(), &f).unwrap(),
            Some(Value::Int(42))
        );
        assert_eq!(
            convert(&s, "-7", &opts(), &f).unwrap(),
            Some(Value::Int(-7))
        );
        for bad in ["1.5", "1e3", "9223372036854775808", "1_0", "x"] {
            assert!(convert(&s, bad, &opts(), &f).is_err(), "{bad} must fail");
        }
    }

    #[test]
    fn bools_take_configured_words_case_insensitively() {
        let f = formats();
        let s = spec("ok", FieldType::Bool);
        for word in ["yes", "YES", "True", "1"] {
            assert_eq!(
                convert(&s, word, &opts(), &f).unwrap(),
                Some(Value::Bool(true)),
                "{word}"
            );
        }
        for word in ["no", "FALSE", "0"] {
            assert_eq!(
                convert(&s, word, &opts(), &f).unwrap(),
                Some(Value::Bool(false)),
                "{word}"
            );
        }
        assert!(convert(&s, "maybe", &opts(), &f).is_err());
        // Configurable words.
        let custom = ConversionOptions {
            bool_words: BoolWords {
                true_words: vec!["y".into()],
                false_words: vec!["n".into()],
            },
            ..ConversionOptions::default()
        };
        assert_eq!(
            convert(&s, "Y", &custom, &f).unwrap(),
            Some(Value::Bool(true))
        );
        assert!(convert(&s, "yes", &custom, &f).is_err());
    }

    #[test]
    fn dates_need_a_format_and_refuse_ambiguity() {
        let f = formats();
        let s = spec("d", FieldType::Date);
        // The ISO default parses; US-style text does not.
        assert_eq!(
            convert(&s, "2026-01-31", &opts(), &f).unwrap(),
            Some(Value::Date(
                time::Date::from_calendar_date(2026, time::Month::January, 31).unwrap()
            ))
        );
        let e = convert(&s, "31/01/2026", &opts(), &f).unwrap_err();
        assert_eq!(e.code, BAD_VALUE);

        // With both day-first and month-first configured, the ambiguous text fails while
        // unambiguous ones parse per the first format that can.
        let dmy = vec!["%d/%m/%Y".to_owned(), "%m/%d/%Y".to_owned()];
        let fmts = Formats::build(&dmy, &[], &[]).unwrap();
        let ambiguous = convert(&s, "01/02/2026", &opts(), &fmts).unwrap_err();
        assert_eq!(ambiguous.code, AMBIGUOUS_DATE);
        // A date only one order can read is not ambiguous.
        assert!(convert(&s, "31/01/2026", &opts(), &fmts).is_ok());
        // Impossible under day-first, readable month-first: the format list decides.
        assert!(convert(&s, "31/01/2026", &opts(), &fmts).is_ok());
    }

    #[test]
    fn datetimes_take_fractions_and_offsets() {
        let f = formats();
        let s = spec("at", FieldType::DateTime);
        let plain = convert(&s, "2026-01-31T10:00:00", &opts(), &f).unwrap();
        assert!(matches!(plain, Some(Value::DateTime(_))));
        // Fraction attaches to the seconds-form.
        assert!(convert(&s, "2026-01-31T10:00:00.250", &opts(), &f).is_ok());
        // Space-separated, no seconds.
        assert!(convert(&s, "2026-01-31 10:00", &opts(), &f).is_ok());
        // Offsets: +05:00 on 10:00 wall clock is 05:00 UTC.
        let Some(Value::DateTime(at)) = convert(&s, "2026-01-31T10:00:00+05:00", &opts(), &f)
            .ok()
            .flatten()
        else {
            panic!("offset form must parse");
        };
        assert_eq!(at.unix_timestamp(), 1_769_835_600); // 2026-01-31T05:00:00Z
        // Z is UTC as-is.
        let Some(Value::DateTime(z)) = convert(&s, "2026-01-31T10:00:00Z", &opts(), &f)
            .ok()
            .flatten()
        else {
            panic!("Z form must parse");
        };
        assert_eq!(z.unix_timestamp(), 1_769_853_600); // 2026-01-31T10:00:00Z
        // Without seconds the offset form still parses.
        assert!(convert(&s, "2026-01-31T10:00+05:00", &opts(), &f).is_ok());
        // More than 9 fraction digits is refused, and so is a nonsense offset.
        assert!(convert(&s, "2026-01-31T10:00:00.1234567890", &opts(), &f).is_err());
        assert!(convert(&s, "2026-01-31T10:00:00+99:99", &opts(), &f).is_err());
    }

    #[test]
    fn times_parse_with_and_without_seconds() {
        let f = formats();
        let s = spec("t", FieldType::Time);
        assert!(convert(&s, "09:30", &opts(), &f).is_ok());
        assert!(convert(&s, "09:30:15", &opts(), &f).is_ok());
        assert!(convert(&s, "09:30:15.5", &opts(), &f).is_ok());
        assert!(convert(&s, "24:00", &opts(), &f).is_err());
        assert!(convert(&s, "9:30", &opts(), &f).is_err());
    }

    #[test]
    fn selects_match_exactly_or_case_insensitively() {
        let f = formats();
        let mut s = spec("status", FieldType::Select);
        s.options = Some("Open\nPaid\nOverdue".into());
        assert_eq!(
            convert(&s, "Paid", &opts(), &f).unwrap(),
            Some(Value::Text("Paid".into()))
        );
        assert!(convert(&s, "paid", &opts(), &f).is_err());
        let insensitive = ConversionOptions {
            select_case_insensitive: true,
            ..ConversionOptions::default()
        };
        // The canonical option's spelling is stored.
        assert_eq!(
            convert(&s, "pAiD", &insensitive, &f).unwrap(),
            Some(Value::Text("Paid".into()))
        );
        assert!(convert(&s, "Refunded", &insensitive, &f).is_err());
    }

    #[test]
    fn links_keep_the_given_string_or_become_ids() {
        let f = formats();
        let s = spec("customer", FieldType::Link);
        // A canonical UUIDv7 stays a link id.
        let uuid = "0198a3d2-7b1a-7000-8000-000000000001";
        let Some(Value::Id(_)) = convert(&s, uuid, &opts(), &f).ok().flatten() else {
            panic!("canonical uuid7 must become an Id");
        };
        // A name or a non-v7 UUID stays text for the writer's lookup.
        assert_eq!(
            convert(&s, "Acme Ltd", &opts(), &f).unwrap(),
            Some(Value::Text("Acme Ltd".into()))
        );
        assert_eq!(
            convert(&s, "550e8400-e29b-41d4-a716-446655440000", &opts(), &f).unwrap(),
            Some(Value::Text("550e8400-e29b-41d4-a716-446655440000".into()))
        );
        // Almost-canonical shapes stay text too: version nibble is 4 (a v4), not 7.
        assert_eq!(
            convert(&s, "0198a3d2-4b1a-4000-8000-000000000001", &opts(), &f).unwrap(),
            Some(Value::Text("0198a3d2-4b1a-4000-8000-000000000001".into()))
        );
        // Variant nibble outside 8..=b: text as well.
        assert_eq!(
            convert(&s, "0198a3d2-7b1a-7000-c000-000000000001", &opts(), &f).unwrap(),
            Some(Value::Text("0198a3d2-7b1a-7000-c000-000000000001".into()))
        );
    }

    #[test]
    fn empty_cells_follow_empty_to_null() {
        let f = formats();
        let text = spec("note", FieldType::Text);
        let num = decimal_spec(2);
        assert_eq!(convert(&text, "", &opts(), &f).unwrap(), None);
        assert_eq!(convert(&num, "", &opts(), &f).unwrap(), None);
        // With empty_to_null off, an empty cell is empty text: fine for text, bad for numbers.
        let strict = ConversionOptions {
            empty_to_null: false,
            ..ConversionOptions::default()
        };
        assert_eq!(
            convert(&text, "", &strict, &f).unwrap(),
            Some(Value::Text(String::new()))
        );
        assert!(convert(&num, "", &strict, &f).is_err());
    }

    #[test]
    fn geo_and_range_have_no_cell_text_form_yet() {
        let f = formats();
        for ty in [FieldType::Geo, FieldType::Range] {
            let e = convert(&spec("place", ty), "1,2", &opts(), &f).unwrap_err();
            assert!(e.message.contains("no cell-text form"), "{}", e.message);
        }
    }

    #[test]
    fn json_and_bytes_behave() {
        let f = formats();
        let json = spec("extra", FieldType::Json);
        assert_eq!(
            convert(&json, "{\"a\":1}", &opts(), &f).unwrap(),
            Some(Value::Json("{\"a\":1}".into()))
        );
        let bytes = spec("data", FieldType::Bytes);
        let e = convert(&bytes, "aGk=", &opts(), &f).unwrap_err();
        assert_eq!(e.code, BAD_VALUE);
        assert!(e.message.contains("bytes"));
    }

    #[test]
    fn bad_format_lists_fail_the_build() {
        let bad = vec!["%Y-%m-%d%.f".to_owned()];
        let e = Formats::build(&bad, &[], &[]).unwrap_err();
        assert!(matches!(e, ImportError::Meta { .. }));
        assert!(e.to_string().contains("%Y-%m-%d%.f"));
    }
}
