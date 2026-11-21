//! `[<start>,<end>)`.

use std::ops::Bound;

use spot_storage_contract::{RangeOf, RangePoint, RangeValue};
use time::UtcOffset;

use crate::error::{Reason, TextError};
use crate::num;

/// One bound as a single cell prints that type: digits, `Decimal::to_string`, `YYYY-MM-DD`,
/// or UTC RFC 3339 with six fractional digits and `Z` (the CSV export's `format_datetime`).
fn point_text(point: &RangePoint) -> String {
    match point {
        RangePoint::Int(n) => n.to_string(),
        RangePoint::Decimal(d) => d.to_string(),
        RangePoint::Date(d) => format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day()),
        RangePoint::DateTime(dt) => {
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
    }
}

/// `[<start>,<end>)`.
///
/// # Errors
///
/// A [`TextError`] for a value that is not a storable range: `Unbounded`, `NotHalfOpen`,
/// `KindMismatch` or `Empty`.
pub fn format_range(range: &RangeValue) -> Result<String, TextError> {
    let Some((lo, hi)) = range.bounds() else {
        return Err(TextError::plain(
            Reason::Unbounded,
            "a range bound is missing or infinite",
        ));
    };
    if !matches!(range.lower, Bound::Included(_)) || !matches!(range.upper, Bound::Excluded(_)) {
        return Err(TextError::plain(
            Reason::NotHalfOpen,
            "a range is half-open, [start,end)",
        ));
    }
    if lo.of() != hi.of() {
        return Err(TextError::plain(
            Reason::KindMismatch,
            "the two bounds are of different kinds",
        ));
    }
    range
        .validate()
        .map_err(|_| TextError::plain(Reason::Empty, "the range is empty"))?;
    Ok(format!("[{},{})", point_text(&lo), point_text(&hi)))
}

fn bound(text: &str, of: RangeOf) -> Option<RangePoint> {
    match of {
        RangeOf::Int => num::int(text).map(RangePoint::Int),
        RangeOf::Decimal => num::decimal(text).map(RangePoint::Decimal),
        RangeOf::Date => num::date(text).map(RangePoint::Date),
        RangeOf::DateTime => num::instant(text).map(RangePoint::DateTime),
    }
}

fn parses_as_another_kind(text: &str, of: RangeOf) -> bool {
    [
        RangeOf::Int,
        RangeOf::Decimal,
        RangeOf::Date,
        RangeOf::DateTime,
    ]
    .into_iter()
    .any(|other| other != of && bound(text, other).is_some())
}

/// Parses `[<start>,<end>)` as a range of `of`. Spaces around the whole cell are tolerated;
/// spaces inside a bound are not.
///
/// # Errors
///
/// A [`TextError`] with the reason: `Syntax`, `NotHalfOpen`, `Unbounded`, `KindMismatch` or
/// `Empty`.
pub fn parse_range(cell: &str, of: RangeOf) -> Result<RangeValue, TextError> {
    let text = cell.trim();
    let syntax = || TextError::new(Reason::Syntax, "a range is `[start,end)`", cell);
    let half_open = || {
        TextError::new(
            Reason::NotHalfOpen,
            "a range is half-open: `[` start and `)` end",
            cell,
        )
    };
    let first = text.chars().next().ok_or_else(syntax)?;
    let last = text.chars().next_back().ok_or_else(syntax)?;
    if !matches!(first, '[' | '(') || !matches!(last, ']' | ')') {
        return Err(syntax());
    }
    if first != '[' || last != ')' {
        return Err(half_open());
    }
    let inner = &text[1..text.len() - 1];
    let mut parts = inner.split(',');
    let (Some(start), Some(end), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(syntax());
    };
    if start.is_empty() || end.is_empty() {
        return Err(TextError::new(
            Reason::Unbounded,
            "a range needs both bounds",
            cell,
        ));
    }
    let mut points = [None, None];
    for (slot, part) in points.iter_mut().zip([start, end]) {
        match bound(part, of) {
            Some(point) => *slot = Some(point),
            None if parses_as_another_kind(part, of) => {
                return Err(TextError::new(
                    Reason::KindMismatch,
                    "a bound is of another kind than the field's",
                    cell,
                ));
            }
            None => return Err(syntax()),
        }
    }
    let [Some(lo), Some(hi)] = points else {
        return Err(syntax());
    };
    let range = RangeValue::half_open(lo, hi);
    range.validate().map_err(|_| {
        TextError::new(
            Reason::Empty,
            "the range is empty: start must be before end",
            cell,
        )
    })?;
    Ok(range)
}
