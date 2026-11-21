//! Conversion between storage [`Value`]s and PostgreSQL parameters and columns, plus the checks
//! that run before a value is sent.
//!
//! Every value reaches PostgreSQL as a bound parameter (ADR-45). JSON travels as text and is cast
//! in the statement (`$n::text::jsonb`), and is read back with `::text`, so the driver needs no
//! JSON feature; its syntax is checked here first (the server would also reject it, but only by
//! aborting the transaction).

use std::ops::Bound;

use bytes::{BufMut, BytesMut};
use rust_decimal::Decimal;
use spot_storage_contract::{
    ErrorKind, FieldDef, FieldType, GeoPoint, Name, RangePoint, RangeValue, RecordId, StorageError,
    StorageResult, Value,
};
use time::{Date, OffsetDateTime, Time};
use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};
use uuid::Uuid;

/// An owned bound parameter.
pub(crate) type Param = Box<dyn ToSql + Sync + Send>;

/// The SQL expression standing for parameter `n` when stored in a column of type `ty`.
pub(crate) fn placeholder(ty: &FieldType, n: usize) -> String {
    match ty {
        FieldType::Json => format!("(${n}::text)::jsonb"),
        // The point travels as well-known text with exact decimal digits and is parsed by
        // PostGIS (ADR-087 section 4.2): no float in this crate. SRID 4326 is the only one.
        FieldType::Geo { srid } => format!("public.ST_GeomFromText(${n}::text, {srid})"),
        _ => format!("${n}"),
    }
}

/// The SQL expression selecting a column of type `ty`, quoted, for [`read_value`].
pub(crate) fn select_expr(ty: &FieldType, quoted_column: &str) -> String {
    match ty {
        FieldType::Json => format!("{quoted_column}::text"),
        // Seven decimal places is the scale of a point (`GEO_SCALE`); PostGIS prints no more
        // digits than that and drops trailing zeros.
        FieldType::Geo { .. } => format!("public.ST_AsText({quoted_column}, 7)"),
        _ => quoted_column.to_owned(),
    }
}

/// The uuid of a record id.
pub(crate) fn uuid_of(id: RecordId) -> Uuid {
    Uuid::from_bytes(*id.as_bytes())
}

/// The parameter for `value` in a column of type `ty`. `value` has passed
/// [`check_value`], so a mismatch here is an internal error, never a user error.
pub(crate) fn to_param(ty: &FieldType, value: &Value) -> StorageResult<Param> {
    fn null<T: ToSql + Sync + Send + 'static>() -> Param {
        Box::new(Option::<T>::None)
    }
    Ok(match (ty, value) {
        (FieldType::Bool, Value::Null) => null::<bool>(),
        (FieldType::Int, Value::Null) => null::<i64>(),
        (FieldType::Decimal { .. }, Value::Null) => null::<Decimal>(),
        (FieldType::Text { .. } | FieldType::Json, Value::Null) => null::<String>(),
        (FieldType::Date, Value::Null) => null::<Date>(),
        (FieldType::Time, Value::Null) => null::<Time>(),
        (FieldType::DateTime, Value::Null) => null::<OffsetDateTime>(),
        (FieldType::Link { .. }, Value::Null) => null::<Uuid>(),
        (FieldType::Bytes, Value::Null) => null::<Vec<u8>>(),
        (FieldType::Geo { .. }, Value::Null) => null::<String>(),
        (FieldType::Range { .. }, Value::Null) => null::<PgRange>(),
        (FieldType::Geo { .. }, Value::GeoPoint(p)) => Box::new(point_text(p)),
        (FieldType::Range { .. }, Value::Range(r)) => Box::new(PgRange::new(r)?),
        (FieldType::Bool, Value::Bool(v)) => Box::new(*v),
        (FieldType::Int, Value::Int(v)) => Box::new(*v),
        (FieldType::Decimal { .. }, Value::Decimal(v)) => Box::new(*v),
        (FieldType::Text { .. }, Value::Text(v)) | (FieldType::Json, Value::Json(v)) => {
            Box::new(v.clone())
        }
        (FieldType::Date, Value::Date(v)) => Box::new(*v),
        (FieldType::Time, Value::Time(v)) => Box::new(*v),
        (FieldType::DateTime, Value::DateTime(v)) => Box::new(*v),
        (FieldType::Link { .. }, Value::Id(v)) => Box::new(uuid_of(*v)),
        (FieldType::Bytes, Value::Bytes(v)) => Box::new(v.clone()),
        _ => {
            return Err(StorageError::new(
                ErrorKind::Internal,
                "a value that passed validation does not fit its column",
            ));
        }
    })
}

/// Reads column `index` of `row` as a value of type `ty`.
pub(crate) fn read_value(row: &Row, index: usize, ty: &FieldType) -> StorageResult<Value> {
    fn get<'a, T: tokio_postgres::types::FromSql<'a>>(
        row: &'a Row,
        index: usize,
    ) -> StorageResult<Option<T>> {
        row.try_get(index).map_err(|_| {
            StorageError::new(
                ErrorKind::Internal,
                "a stored value could not be read in its declared type",
            )
        })
    }
    fn some<T>(v: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
        v.map_or(Value::Null, f)
    }
    Ok(match ty {
        FieldType::Bool => some(get::<bool>(row, index)?, Value::Bool),
        FieldType::Int => some(get::<i64>(row, index)?, Value::Int),
        FieldType::Decimal { .. } => some(get::<Decimal>(row, index)?, Value::Decimal),
        FieldType::Text { .. } => some(get::<String>(row, index)?, Value::Text),
        FieldType::Json => some(get::<String>(row, index)?, Value::Json),
        FieldType::Date => some(get::<Date>(row, index)?, Value::Date),
        FieldType::Time => some(get::<Time>(row, index)?, Value::Time),
        FieldType::DateTime => some(get::<OffsetDateTime>(row, index)?, |t| {
            Value::DateTime(t.to_offset(time::UtcOffset::UTC))
        }),
        FieldType::Link { .. } => match get::<Uuid>(row, index)? {
            None => Value::Null,
            Some(u) => Value::Id(RecordId::from_bytes(*u.as_bytes())?),
        },
        FieldType::Bytes => some(get::<Vec<u8>>(row, index)?, Value::Bytes),
        FieldType::Geo { .. } => match get::<String>(row, index)? {
            None => Value::Null,
            Some(text) => Value::GeoPoint(parse_point(&text)?),
        },
        FieldType::Range { .. } => some(get::<PgRange>(row, index)?, |r| Value::Range(r.0)),
    })
}

/// `POINT(lon lat)` with the coordinates' exact decimal digits.
fn point_text(p: &GeoPoint) -> String {
    format!("POINT({} {})", p.lon, p.lat)
}

/// Parses PostGIS' `POINT(lon lat)` into exact decimals. Nothing else is a stored point.
fn parse_point(text: &str) -> StorageResult<GeoPoint> {
    let corrupt = || {
        StorageError::new(
            ErrorKind::Internal,
            "a stored geo value is not a point with finite coordinates",
        )
    };
    let inner = text
        .strip_prefix("POINT(")
        .and_then(|t| t.strip_suffix(')'))
        .ok_or_else(corrupt)?;
    let (lon, lat) = inner.split_once(' ').ok_or_else(corrupt)?;
    let coordinate = |s: &str| Decimal::from_str_exact(s).map_err(|_| corrupt());
    Ok(GeoPoint::new(coordinate(lon)?, coordinate(lat)?))
}

/// A half-open range in PostgreSQL's binary range format (flags, then each present bound as a
/// length and the subtype's own binary form), so bounds travel as the exact integer, decimal,
/// date or instant, never as text or a float (ADR-087 section 5).
#[derive(Debug)]
pub(crate) struct PgRange(RangeValue);

/// `RANGE_LB_INC`: the only flag a stored `[start, end)` range has.
const RANGE_LOWER_INCLUSIVE: u8 = 0x02;
const RANGE_EMPTY: u8 = 0x01;
const RANGE_UPPER_INCLUSIVE: u8 = 0x04;
const RANGE_LOWER_INFINITE: u8 = 0x08;
const RANGE_UPPER_INFINITE: u8 = 0x10;

type BoxError = Box<dyn std::error::Error + Sync + Send>;

impl PgRange {
    fn new(range: &RangeValue) -> StorageResult<Self> {
        // A value reaching here passed `FieldType::check`; this keeps the encoder total.
        if range.bounds().is_none() {
            return Err(StorageError::new(
                ErrorKind::Internal,
                "a range that passed validation has no finite bounds",
            ));
        }
        Ok(Self(*range))
    }
}

fn write_point(point: RangePoint, inner: &Type, out: &mut BytesMut) -> Result<(), BoxError> {
    let at = out.len();
    out.put_i32(0);
    match point {
        RangePoint::Int(v) => v.to_sql(inner, out)?,
        RangePoint::Decimal(v) => v.to_sql(inner, out)?,
        RangePoint::Date(v) => v.to_sql(inner, out)?,
        RangePoint::DateTime(v) => v.to_sql(inner, out)?,
    };
    let len = i32::try_from(out.len() - at - 4)?;
    out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

fn read_point(raw: &mut &[u8], inner: &Type) -> Result<RangePoint, BoxError> {
    let (len, rest) = raw
        .split_first_chunk::<4>()
        .ok_or("a truncated range bound")?;
    let len = usize::try_from(i32::from_be_bytes(*len)).map_err(|_| "a null range bound")?;
    if rest.len() < len {
        return Err("a truncated range bound".into());
    }
    let (bytes, rest) = rest.split_at(len);
    *raw = rest;
    Ok(match *inner {
        Type::INT8 => RangePoint::Int(i64::from_sql(inner, bytes)?),
        Type::NUMERIC => RangePoint::Decimal(Decimal::from_sql(inner, bytes)?),
        Type::DATE => RangePoint::Date(Date::from_sql(inner, bytes)?),
        Type::TIMESTAMPTZ => RangePoint::DateTime(
            OffsetDateTime::from_sql(inner, bytes)?.to_offset(time::UtcOffset::UTC),
        ),
        _ => return Err("an unsupported range subtype".into()),
    })
}

fn supported_subtype(inner: &Type) -> bool {
    matches!(
        *inner,
        Type::INT8 | Type::NUMERIC | Type::DATE | Type::TIMESTAMPTZ
    )
}

impl ToSql for PgRange {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        let Kind::Range(inner) = ty.kind() else {
            return Err("not a range type".into());
        };
        let (lower, upper) = self.0.bounds().ok_or("a range without finite bounds")?;
        out.put_u8(RANGE_LOWER_INCLUSIVE);
        write_point(lower, inner, out)?;
        write_point(upper, inner, out)?;
        Ok(IsNull::No)
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Range(inner) if supported_subtype(inner))
    }

    to_sql_checked!();
}

impl<'a> FromSql<'a> for PgRange {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        let Kind::Range(inner) = ty.kind() else {
            return Err("not a range type".into());
        };
        let (&flags, mut rest) = raw.split_first().ok_or("an empty range value")?;
        if flags & RANGE_EMPTY != 0 {
            return Err("an empty range is not a stored value".into());
        }
        let mut bound = |infinite: u8, inclusive: u8| -> Result<Bound<RangePoint>, BoxError> {
            if flags & infinite != 0 {
                return Ok(Bound::Unbounded);
            }
            let point = read_point(&mut rest, inner)?;
            Ok(if flags & inclusive != 0 {
                Bound::Included(point)
            } else {
                Bound::Excluded(point)
            })
        };
        let lower = bound(RANGE_LOWER_INFINITE, RANGE_LOWER_INCLUSIVE)?;
        let upper = bound(RANGE_UPPER_INFINITE, RANGE_UPPER_INCLUSIVE)?;
        Ok(Self(RangeValue { lower, upper }))
    }

    fn accepts(ty: &Type) -> bool {
        matches!(ty.kind(), Kind::Range(inner) if supported_subtype(inner))
    }
}

/// Checks `value` against `field` before it is sent: type, nullability, decimal scale and
/// precision (never rounded), text limit, and what PostgreSQL cannot store (a NUL character in
/// text, invalid JSON). The error names the entity and field.
pub(crate) fn check_value(entity: &Name, field: &FieldDef, value: &Value) -> StorageResult<()> {
    let reject = |why: String| {
        StorageError::new(
            ErrorKind::InvalidValue,
            format!("`{entity}.{}`: {why}", field.name),
        )
        .with_entity(entity)
        .with_field(&field.name)
    };
    field.ty.check(value, field.nullable).map_err(reject)?;
    match value {
        Value::Text(s) if s.contains('\0') => {
            Err(reject("text cannot contain the NUL character".into()))
        }
        Value::Json(s) => validate_json(s).map_err(|why| reject(format!("not valid JSON: {why}"))),
        _ => Ok(()),
    }
}

/// Checks JSON syntax (RFC 8259) and that no string contains `\u0000`, which `jsonb` cannot hold.
pub(crate) fn validate_json(text: &str) -> Result<(), String> {
    let mut parser = Json {
        bytes: text.as_bytes(),
        at: 0,
        depth: 0,
    };
    parser.space();
    parser.value()?;
    parser.space();
    if parser.at != parser.bytes.len() {
        return Err(format!("unexpected text at byte {}", parser.at));
    }
    Ok(())
}

/// A small recursive-descent JSON syntax checker. Depth is limited so hostile input cannot
/// exhaust the stack.
struct Json<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: u32,
}

const MAX_JSON_DEPTH: u32 = 128;

impl Json<'_> {
    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.at) == Some(&byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.eat(byte) {
            Ok(())
        } else {
            Err(format!(
                "expected `{}` at byte {}",
                char::from(byte),
                self.at
            ))
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), String> {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            Ok(())
        } else {
            Err(format!("unexpected text at byte {}", self.at))
        }
    }

    fn value(&mut self) -> Result<(), String> {
        match self.bytes.get(self.at) {
            Some(b'{') => self.nested(Self::object),
            Some(b'[') => self.nested(Self::array),
            Some(b'"') => self.string(),
            Some(b't') => self.literal("true"),
            Some(b'f') => self.literal("false"),
            Some(b'n') => self.literal("null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(format!("a value was expected at byte {}", self.at)),
        }
    }

    fn nested(&mut self, f: fn(&mut Self) -> Result<(), String>) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_JSON_DEPTH {
            return Err("nested too deeply".into());
        }
        let result = f(self);
        self.depth -= 1;
        result
    }

    fn object(&mut self) -> Result<(), String> {
        self.expect(b'{')?;
        self.space();
        if self.eat(b'}') {
            return Ok(());
        }
        loop {
            self.space();
            self.string()?;
            self.space();
            self.expect(b':')?;
            self.space();
            self.value()?;
            self.space();
            if self.eat(b',') {
                continue;
            }
            return self.expect(b'}');
        }
    }

    fn array(&mut self) -> Result<(), String> {
        self.expect(b'[')?;
        self.space();
        if self.eat(b']') {
            return Ok(());
        }
        loop {
            self.space();
            self.value()?;
            self.space();
            if self.eat(b',') {
                continue;
            }
            return self.expect(b']');
        }
    }

    fn string(&mut self) -> Result<(), String> {
        self.expect(b'"')?;
        loop {
            match self.bytes.get(self.at) {
                None => return Err("a string is not closed".into()),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(());
                }
                Some(0..=0x1f) => return Err(format!("a control character at byte {}", self.at)),
                Some(b'\\') => {
                    self.at += 1;
                    match self.bytes.get(self.at) {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => {
                            self.at += 1
                        }
                        Some(b'u') => {
                            let digits = self.bytes.get(self.at + 1..self.at + 5);
                            let Some(hex) = digits.filter(|d| d.iter().all(u8::is_ascii_hexdigit))
                            else {
                                return Err(format!("a bad \\u escape at byte {}", self.at));
                            };
                            if hex == b"0000" {
                                return Err(
                                    "a string contains \\u0000, which cannot be stored".into()
                                );
                            }
                            self.at += 5;
                        }
                        _ => return Err(format!("a bad escape at byte {}", self.at)),
                    }
                }
                Some(_) => self.at += 1,
            }
        }
    }

    fn digits(&mut self) -> Result<(), String> {
        let start = self.at;
        while matches!(self.bytes.get(self.at), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        if self.at == start {
            Err(format!("a digit was expected at byte {}", self.at))
        } else {
            Ok(())
        }
    }

    fn number(&mut self) -> Result<(), String> {
        self.eat(b'-');
        if self.eat(b'0') {
            // A leading zero stands alone.
        } else {
            self.digits()?;
        }
        if self.eat(b'.') {
            self.digits()?;
        }
        if self.eat(b'e') || self.eat(b'E') {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            self.digits()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_json_and_rejects_what_is_not() {
        for ok in [
            "null",
            "true",
            "0",
            "-12.5e+3",
            r#""aé\n""#,
            r#"{"tier": "gold", "tags": [1, 2, {"x": []}], "n": null}"#,
            "  [ ]  ",
        ] {
            assert_eq!(validate_json(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "{not json",
            "01",
            "1.",
            "[1,]",
            r#"{"a" 1}"#,
            r#"{"a": 1,}"#,
            "'single'",
            r#""unclosed"#,
            r#""bad \x escape""#,
            "[1] 2",
            "tru",
            r#""\u0000""#,
            "\"tab\there\"",
        ] {
            assert!(validate_json(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn deep_nesting_is_refused_without_overflowing() {
        let deep = "[".repeat(1000);
        assert!(validate_json(&deep).is_err());
    }
}
