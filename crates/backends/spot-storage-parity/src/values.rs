//! Typed values in the case files, written as short tagged strings, and their canonical form.
//!
//! | Written | Value |
//! | --- | --- |
//! | `null` | `Value::Null` |
//! | `b:true` | a boolean |
//! | `i:42` | an integer |
//! | `d:1.50` | a decimal, scale as written |
//! | `t:Ali` | text |
//! | `trep:3:ab` | the text `ababab` (the text `ab`, 3 times) |
//! | `date:2026-01-31` | a date |
//! | `time:09:30:00.123456` | a wall-clock time (up to nanoseconds) |
//! | `dt:2026-01-31T10:00:00.5Z` | an instant; the offset (`Z` or `+05:00`) is converted to UTC |
//! | `json:{"a":1}` | JSON text |
//! | `bytes:00ff` | bytes in hex; `bytes:all` is the 256 byte values in order |
//! | `brep:3:ab` | the bytes `ababab` (the hex bytes, 3 times) |
//! | `id:a` | the record id of the document called `a` |
//! | `geo:13.4050 52.5200` | a WGS-84 point, longitude then latitude, exact decimals (ADR-087 §4) |
//! | `range:[date:2026-11-01,date:2026-11-02)` | a half-open range of one point kind (ADR-087 §5); `(` or `)` with an empty side is an unbounded bound, which a write refuses |
//!
//! Comparison goes through [`canon`], which keeps what a backend must preserve (a decimal's
//! scale, a text's exact characters) and drops what it may normalise (JSON key order and
//! whitespace).

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use spot_storage_contract::{GeoPoint, RangePoint, RangeValue, RecordId, Value};
use std::ops::Bound;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

/// Deterministic record ids for the names used in a case. Ids ascend in the order the names
/// are first used, so "ordered by id" is the order the documents were written.
#[derive(Debug, Default)]
pub struct Ids {
    ids: BTreeMap<String, RecordId>,
    names: BTreeMap<RecordId, String>,
}

impl Ids {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The id for `name`, made on first use.
    pub fn get(&mut self, name: &str) -> Result<RecordId, String> {
        if let Some(id) = self.ids.get(name) {
            return Ok(*id);
        }
        let n = self.ids.len() as u64 + 1;
        let ms = 1_700_000_000_000_u64 + n;
        let mut bytes = [0_u8; 16];
        bytes[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        bytes[15] = (n & 0xff) as u8;
        let id = RecordId::from_bytes(bytes).map_err(|e| e.to_string())?;
        self.ids.insert(name.to_owned(), id);
        self.names.insert(id, name.to_owned());
        Ok(id)
    }

    /// The name an id was made for.
    pub fn name_of(&self, id: &RecordId) -> Option<&str> {
        self.names.get(id).map(String::as_str)
    }
}

fn num<T: std::str::FromStr>(s: &str, what: &str) -> Result<T, String> {
    s.parse::<T>()
        .map_err(|_| format!("`{s}` is not a valid {what}"))
}

/// Parses `YYYY-MM-DD`.
pub fn parse_date(s: &str) -> Result<Date, String> {
    let mut p = s.splitn(3, '-');
    let (y, m, d) = (p.next(), p.next(), p.next());
    let (Some(y), Some(m), Some(d)) = (y, m, d) else {
        return Err(format!("`{s}` is not a date (YYYY-MM-DD)"));
    };
    let month = Month::try_from(num::<u8>(m, "month")?).map_err(|e| e.to_string())?;
    Date::from_calendar_date(num(y, "year")?, month, num(d, "day")?).map_err(|e| e.to_string())
}

/// Parses `HH:MM:SS` with an optional fraction of up to nine digits.
pub fn parse_time(s: &str) -> Result<Time, String> {
    let (hms, frac) = s.split_once('.').unwrap_or((s, ""));
    let mut p = hms.splitn(3, ':');
    let (h, m, sec) = (p.next(), p.next(), p.next());
    let (Some(h), Some(m), Some(sec)) = (h, m, sec) else {
        return Err(format!("`{s}` is not a time (HH:MM:SS[.fraction])"));
    };
    if frac.len() > 9 || !frac.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("`{s}` has a bad fraction"));
    }
    let mut nanos = 0_u32;
    if !frac.is_empty() {
        nanos = num::<u32>(&format!("{frac:0<9}"), "fraction")?;
    }
    Time::from_hms_nano(
        num(h, "hour")?,
        num(m, "minute")?,
        num(sec, "second")?,
        nanos,
    )
    .map_err(|e| e.to_string())
}

/// Parses `YYYY-MM-DDTHH:MM:SS[.fraction]` followed by `Z` or `+HH:MM` / `-HH:MM`.
pub fn parse_instant(s: &str) -> Result<OffsetDateTime, String> {
    let (date, rest) = s
        .split_once('T')
        .ok_or_else(|| format!("`{s}` is not an instant"))?;
    let (clock, offset) = if let Some(c) = rest.strip_suffix('Z') {
        (c, UtcOffset::UTC)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (c, o) = rest.split_at(i);
        let sign: i8 = if o.starts_with('-') { -1 } else { 1 };
        let (oh, om) = o[1..]
            .split_once(':')
            .ok_or_else(|| format!("`{s}` has a bad offset"))?;
        let off = UtcOffset::from_hms(
            sign * num::<i8>(oh, "offset hour")?,
            sign * num::<i8>(om, "offset minute")?,
            0,
        )
        .map_err(|e| e.to_string())?;
        (c, off)
    } else {
        return Err(format!("`{s}` needs Z or an offset"));
    };
    Ok(PrimitiveDateTime::new(parse_date(date)?, parse_time(clock)?).assume_offset(offset))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err(format!("`{s}` is not hex"));
    }
    (0..s.len() / 2)
        .map(|i| {
            u8::from_str_radix(&s[2 * i..2 * i + 2], 16).map_err(|_| format!("`{s}` is not hex"))
        })
        .collect()
}

/// Parses one tagged value string (see the module documentation).
pub fn parse_value(s: &str, ids: &mut Ids) -> Result<Value, String> {
    if s == "null" {
        return Ok(Value::Null);
    }
    let (tag, body) = s
        .split_once(':')
        .ok_or_else(|| format!("`{s}` has no type tag (t:, i:, d:, ...)"))?;
    Ok(match tag {
        "b" => Value::Bool(num(body, "boolean")?),
        "i" => Value::Int(num(body, "integer")?),
        "d" => Value::Decimal(Decimal::from_str_exact(body).map_err(|e| format!("`{body}`: {e}"))?),
        "t" => Value::Text(body.to_owned()),
        "trep" => {
            let (n, text) = body
                .split_once(':')
                .ok_or_else(|| format!("`{s}` is not trep:<count>:<text>"))?;
            Value::Text(text.repeat(num(n, "count")?))
        }
        "date" => Value::Date(parse_date(body)?),
        "time" => Value::Time(parse_time(body)?),
        "dt" => Value::DateTime(parse_instant(body)?),
        "json" => Value::Json(body.to_owned()),
        "brep" => {
            let (n, hexed) = body
                .split_once(':')
                .ok_or_else(|| format!("`{s}` is not brep:<count>:<hex>"))?;
            Value::Bytes(unhex(hexed)?.repeat(num(n, "count")?))
        }
        "bytes" if body == "all" => Value::Bytes((0..=255_u8).collect()),
        "bytes" => Value::Bytes(unhex(body)?),
        "id" => Value::Id(ids.get(body)?),
        "geo" => Value::GeoPoint(parse_geo(body)?),
        "range" => Value::Range(parse_range(body, ids)?),
        _ => return Err(format!("unknown value tag `{tag}` in `{s}`")),
    })
}

/// Parses `geo:<lon> <lat>`: two exact decimals, longitude first. The `geo:` tag is optional, so
/// a filter's own point (`(within-distance spot "geo:13.4 52.5" 5000)`) and a write's value
/// (`spot="geo:13.4 52.5"`) are both read by this one function. The value is not range- or
/// scale-checked here, so a case can write an out-of-range point and expect the refusal.
pub fn parse_geo(s: &str) -> Result<GeoPoint, String> {
    let body = s.strip_prefix("geo:").unwrap_or(s);
    let (lon, lat) = body
        .split_once(' ')
        .ok_or_else(|| format!("`{s}` is not `geo:<lon> <lat>`"))?;
    Ok(GeoPoint::new(
        Decimal::from_str_exact(lon).map_err(|e| format!("`{lon}`: {e}"))?,
        Decimal::from_str_exact(lat).map_err(|e| format!("`{lat}`: {e}"))?,
    ))
}

/// Parses `[<start>,<end>)` after the `range:` tag; the tag itself is optional, so a filter's own
/// range (`(range-overlaps during "range:[date:2026-11-01,date:2026-11-03)")`) and a write's
/// value (`during="range:[...)"`) are both read by this one function. Each side is a tagged
/// scalar (`i:`, `d:`, `date:`, `dt:`) or empty for an unbounded bound. Nothing is validated
/// here, so a case can write an empty, reversed or unbounded range and expect the refusal
/// (SEM-RNG-02, -06, -07).
pub fn parse_range(s: &str, ids: &mut Ids) -> Result<RangeValue, String> {
    let body = s.strip_prefix("range:").unwrap_or(s);
    let bounds = body
        .strip_prefix('[')
        .map(|rest| ('[', rest))
        .or_else(|| body.strip_prefix('(').map(|rest| ('(', rest)))
        .ok_or_else(|| format!("`{s}` does not start a range with `[` or `(`"))?;
    let (lower_bracket, rest) = bounds;
    let (inner, upper_bracket) = if let Some(i) = rest.strip_suffix(')') {
        (i, ')')
    } else if let Some(i) = rest.strip_suffix(']') {
        (i, ']')
    } else {
        return Err(format!("`{s}` does not end a range with `)` or `]`"));
    };
    let (lo, hi) = inner
        .split_once(',')
        .ok_or_else(|| format!("`{s}` needs `<start>,<end>`"))?;
    let mut point = |side: &str| -> Result<Option<RangePoint>, String> {
        if side.is_empty() {
            return Ok(None);
        }
        let v = parse_value(side, ids)?;
        RangePoint::from_value(&v)
            .map(Some)
            .ok_or_else(|| format!("`{side}` is not an int, decimal, date or instant"))
    };
    let mut bound = |bracket: char, side: &str, lower: bool| -> Result<Bound<RangePoint>, String> {
        match (point(side)?, bracket) {
            (None, _) => Ok(Bound::Unbounded),
            (Some(p), '[') | (Some(p), ']') => Ok(Bound::Included(p)),
            (Some(p), '(') | (Some(p), ')') => Ok(Bound::Excluded(p)),
            _ => Err(format!("bad range bracket `{bracket}` (lower: {lower})")),
        }
    };
    Ok(RangeValue {
        lower: bound(lower_bracket, lo, true)?,
        upper: bound(upper_bracket, hi, false)?,
    })
}

/// One range bound as tagged text, the inverse of [`parse_range`].
fn point_text(p: RangePoint) -> String {
    match p {
        RangePoint::Int(i) => format!("i:{i}"),
        RangePoint::Decimal(d) => format!("d:{d}"),
        RangePoint::Date(d) => format!("date:{}", date_text(d)),
        RangePoint::DateTime(t) => format!("dt:{}", instant_text(t)),
    }
}

/// A range as tagged text in the form [`parse_range`] reads.
fn range_text(r: &RangeValue) -> String {
    let mut s = String::from("range:");
    match &r.lower {
        Bound::Included(p) => {
            s.push('[');
            s.push_str(&point_text(*p));
        }
        Bound::Excluded(p) => {
            s.push('(');
            s.push_str(&point_text(*p));
        }
        Bound::Unbounded => s.push('('),
    }
    s.push(',');
    match &r.upper {
        Bound::Included(p) => {
            s.push_str(&point_text(*p));
            s.push(']');
        }
        Bound::Excluded(p) => {
            s.push_str(&point_text(*p));
            s.push(')');
        }
        Bound::Unbounded => s.push(')'),
    }
    s
}

/// An instant in UTC as `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`.
fn instant_text(t: OffsetDateTime) -> String {
    let u = t.to_offset(UtcOffset::UTC);
    format!("{}T{}Z", date_text(u.date()), time_text(u.time()))
}

fn json_canon(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{k:?}:{}", m.get(k).map_or_else(String::new, json_canon)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        serde_json::Value::Array(a) => {
            format!(
                "[{}]",
                a.iter().map(json_canon).collect::<Vec<_>>().join(",")
            )
        }
        other => other.to_string(),
    }
}

/// The form an expectation is compared in: an instant written with an offset is expected in UTC.
pub fn expected_form(v: Value) -> Value {
    match v {
        Value::DateTime(t) => Value::DateTime(t.to_offset(UtcOffset::UTC)),
        other => other,
    }
}

/// The canonical text of a value, used for every comparison and every diff.
pub fn canon(v: &Value, ids: &Ids) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("b:{b}"),
        Value::Int(i) => format!("i:{i}"),
        Value::Decimal(d) => format!("d:{d}"),
        Value::Text(t) => format!("t:{t:?}"),
        Value::Date(d) => format!("date:{}", date_text(*d)),
        Value::Time(t) => format!("time:{}", time_text(*t)),
        Value::DateTime(t) => {
            let shown = if t.offset() == UtcOffset::UTC {
                String::new()
            } else {
                format!(" (returned with offset {})", t.offset())
            };
            format!("dt:{}{shown}", instant_text(*t))
        }
        Value::Id(id) => match ids.name_of(id) {
            Some(n) => format!("id:{n}"),
            None => format!("id:{id}"),
        },
        Value::Json(j) => match serde_json::from_str::<serde_json::Value>(j) {
            Ok(parsed) => format!("json:{}", json_canon(&parsed)),
            Err(_) => format!("json-invalid:{j:?}"),
        },
        Value::Bytes(b) => format!("bytes:{}", hex(b)),
        Value::GeoPoint(p) => format!("geo:{} {}", p.lon.normalize(), p.lat.normalize()),
        Value::Range(r) => range_text(r),
    }
}

fn date_text(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
}

fn time_text(t: Time) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:09}",
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_and_canonicalise() -> Result<(), String> {
        let mut ids = Ids::new();
        let v = parse_value("d:1.50", &mut ids)?;
        assert_eq!(canon(&v, &ids), "d:1.50");
        let a = parse_value("dt:2026-01-31T10:00:00+05:00", &mut ids)?;
        // A returned offset other than UTC is visible; expectations are compared in UTC.
        assert!(canon(&a, &ids).contains("returned with offset"));
        assert_eq!(
            canon(&expected_form(a), &ids),
            "dt:2026-01-31T05:00:00.000000000Z"
        );
        let j1 = parse_value(r#"json:{"b":1, "a":[true]}"#, &mut ids)?;
        let j2 = parse_value(r#"json:{"a":[true],"b":1}"#, &mut ids)?;
        assert_eq!(canon(&j1, &ids), canon(&j2, &ids));
        assert_eq!(
            parse_value("trep:3:ab", &mut ids)?,
            Value::Text("ababab".into())
        );
        assert!(parse_value("x:1", &mut ids).is_err());
        let first = ids.get("a")?;
        let second = ids.get("b")?;
        assert!(first < second);
        Ok(())
    }

    /// A point and a range parse into the contract's values and canon back to the text they were
    /// written as; an unbounded or reversed range is representable, so a case can expect the
    /// write to be refused rather than the parser to fail.
    #[test]
    fn geo_and_range_values_parse_and_canonicalise() -> Result<(), String> {
        let mut ids = Ids::new();
        let point = parse_value("geo:13.4050 52.5200", &mut ids)?;
        assert_eq!(canon(&point, &ids), "geo:13.405 52.52");
        let range = parse_value("range:[date:2026-11-01,date:2026-11-02)", &mut ids)?;
        assert_eq!(
            canon(&range, &ids),
            "range:[date:2026-11-01,date:2026-11-02)"
        );
        let instants = parse_value(
            "range:[dt:2026-11-01T09:00:00Z,dt:2026-11-01T17:00:00+05:00)",
            &mut ids,
        )?;
        assert_eq!(
            canon(&instants, &ids),
            "range:[dt:2026-11-01T09:00:00.000000000Z,dt:2026-11-01T12:00:00.000000000Z)"
        );
        let unbounded = parse_value("range:(,date:2026-11-02)", &mut ids)?;
        assert_eq!(canon(&unbounded, &ids), "range:(,date:2026-11-02)");
        let inclusive = parse_value("range:[date:2026-11-01,date:2026-11-01]", &mut ids)?;
        assert_eq!(
            canon(&inclusive, &ids),
            "range:[date:2026-11-01,date:2026-11-01]"
        );
        assert!(parse_value("geo:13.4", &mut ids).is_err());
        assert_eq!(
            parse_range("range:[date:2026-11-02,date:2026-11-03)", &mut ids)?,
            parse_range("[date:2026-11-02,date:2026-11-03)", &mut ids)?
        );
        assert!(parse_value("range:[date:2026-11-01", &mut ids).is_err());
        assert!(parse_value("range:[t:x]y,date:2026-11-01)", &mut ids).is_err());
        // A filter's point is written with its tag, a write's value passes only the body: both
        // reach `parse_geo`.
        assert_eq!(
            parse_geo("geo:13.4050 52.5200")?,
            parse_geo("13.4050 52.5200")?
        );
        assert!(parse_geo("geo:13.4").is_err());
        Ok(())
    }
}
