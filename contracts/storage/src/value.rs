//! The storage value model.

use std::cmp::Ordering;
use std::ops::Bound;

use rust_decimal::Decimal;
use time::{Date, OffsetDateTime, Time};

use crate::id::RecordId;
use crate::limits::GEO_SCALE;
use crate::schema::{FieldType, RangeOf};

/// One stored value.
///
/// Parity rules every backend follows (STO-10 extends them):
/// - `Null` and an empty `Text` are different values; storage never converts one into the other.
/// - `Decimal` values are stored exactly. A value with more fractional digits than the field's
///   scale is rejected with [`ErrorKind::InvalidValue`](crate::ErrorKind::InvalidValue), never
///   rounded: rounding rules are data applied above storage (ADR-12).
/// - `DateTime` is an instant; it is stored and returned in UTC.
/// - Text equality is exact (case- and accent-sensitive) unless a query asks otherwise; text
///   ordering follows the catalog's [`Collation`](crate::Collation).
/// - There is no floating-point variant (ADR-12). The geo and range variants (ADR-087) carry
///   decimals, dates, instants and integers only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// No value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit integer (counts, positions; never money or quantities).
    Int(i64),
    /// An exact decimal: money, quantities, rates.
    Decimal(Decimal),
    /// UTF-8 text.
    Text(String),
    /// A calendar date.
    Date(Date),
    /// A wall-clock time without date.
    Time(Time),
    /// An instant, returned in UTC.
    DateTime(OffsetDateTime),
    /// A reference to another record (a link).
    Id(RecordId),
    /// A JSON document as text. It must be valid JSON; backends may normalise whitespace and
    /// key order, so compare JSON values semantically.
    Json(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// A WGS-84 point (ADR-087 section 4). A null point is [`Value::Null`], never 0,0.
    GeoPoint(GeoPoint),
    /// A half-open range of integers, decimals, dates or instants (ADR-087 section 5).
    Range(RangeValue),
}

impl Value {
    /// True for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Whether a non-null value can be stored in a field of type `ty` (ignoring nullability and
    /// decimal scale, which [`FieldType::check`] adds).
    pub fn fits(&self, ty: &FieldType) -> bool {
        matches!(
            (self, ty),
            (Value::Bool(_), FieldType::Bool)
                | (Value::Int(_), FieldType::Int)
                | (Value::Decimal(_), FieldType::Decimal { .. })
                | (Value::Text(_), FieldType::Text { .. })
                | (Value::Date(_), FieldType::Date)
                | (Value::Time(_), FieldType::Time)
                | (Value::DateTime(_), FieldType::DateTime)
                | (Value::Id(_), FieldType::Link { .. })
                | (Value::Json(_), FieldType::Json)
                | (Value::Bytes(_), FieldType::Bytes)
                | (Value::GeoPoint(_), FieldType::Geo { .. })
        ) || matches!((self, ty), (Value::Range(r), FieldType::Range { of }) if r.of() == Some(*of))
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Value::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Int(v)
    }
}

impl From<Decimal> for Value {
    fn from(v: Decimal) -> Self {
        Value::Decimal(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Value::Text(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Value::Text(v)
    }
}

impl From<RecordId> for Value {
    fn from(v: RecordId) -> Self {
        Value::Id(v)
    }
}

/// A WGS-84 point: longitude and latitude in decimal degrees, exact (ADR-087 section 4).
///
/// Coordinates have at most [`GEO_SCALE`] (7) decimal places, about one centimetre at the
/// equator; a point with more places is refused, never rounded. Longitude is in `-180..=180`
/// and latitude in `-90..=90`; anything else is `InvalidValue` on a write and `InvalidQuery` in
/// a filter. There is no float.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeoPoint {
    /// Longitude in degrees, `-180..=180`.
    pub lon: Decimal,
    /// Latitude in degrees, `-90..=90`.
    pub lat: Decimal,
}

impl GeoPoint {
    /// A point; check it with [`GeoPoint::validate`] (writes and `FieldType::check` do).
    pub fn new(lon: Decimal, lat: Decimal) -> Self {
        Self { lon, lat }
    }

    /// Whether the coordinates are in range and carry at most seven decimal places.
    pub fn validate(&self) -> Result<(), String> {
        check_coordinate("longitude", self.lon, 180)?;
        check_coordinate("latitude", self.lat, 90)
    }
}

/// Checks one coordinate against `-limit..=limit` degrees and the scale of 7.
pub(crate) fn check_coordinate(what: &str, v: Decimal, limit: i64) -> Result<(), String> {
    if v.abs() > Decimal::from(limit) {
        return Err(format!("{what} {v} is outside -{limit}..={limit} degrees"));
    }
    if v.normalize().scale() > GEO_SCALE {
        return Err(format!(
            "{what} {v} has more than {GEO_SCALE} decimal places; round it before storing"
        ));
    }
    Ok(())
}

/// One end of a range: an integer, an exact decimal, a calendar date or an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangePoint {
    /// An integer.
    Int(i64),
    /// An exact decimal.
    Decimal(Decimal),
    /// A calendar date.
    Date(Date),
    /// An instant.
    DateTime(OffsetDateTime),
}

impl RangePoint {
    /// The kind of payload.
    pub fn of(&self) -> RangeOf {
        match self {
            RangePoint::Int(_) => RangeOf::Int,
            RangePoint::Decimal(_) => RangeOf::Decimal,
            RangePoint::Date(_) => RangeOf::Date,
            RangePoint::DateTime(_) => RangeOf::DateTime,
        }
    }

    /// The point as a [`Value`].
    pub fn to_value(self) -> Value {
        match self {
            RangePoint::Int(v) => Value::Int(v),
            RangePoint::Decimal(v) => Value::Decimal(v),
            RangePoint::Date(v) => Value::Date(v),
            RangePoint::DateTime(v) => Value::DateTime(v),
        }
    }

    /// The point held by a scalar [`Value`], if it is an integer, decimal, date or instant.
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Int(v) => Some(RangePoint::Int(*v)),
            Value::Decimal(v) => Some(RangePoint::Decimal(*v)),
            Value::Date(v) => Some(RangePoint::Date(*v)),
            Value::DateTime(v) => Some(RangePoint::DateTime(*v)),
            _ => None,
        }
    }

    /// Orders two points of the same kind; `None` when the kinds differ.
    pub fn compare(&self, other: &RangePoint) -> Option<Ordering> {
        match (self, other) {
            (RangePoint::Int(a), RangePoint::Int(b)) => Some(a.cmp(b)),
            (RangePoint::Decimal(a), RangePoint::Decimal(b)) => Some(a.cmp(b)),
            (RangePoint::Date(a), RangePoint::Date(b)) => Some(a.cmp(b)),
            (RangePoint::DateTime(a), RangePoint::DateTime(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// The distance from `self` to `later` in whole days, for dates and instants (rounded
    /// down); `None` for integers and decimals, which have no calendar span.
    pub fn days_until(&self, later: &RangePoint) -> Option<i64> {
        match (self, later) {
            (RangePoint::Date(a), RangePoint::Date(b)) => Some((*b - *a).whole_days()),
            (RangePoint::DateTime(a), RangePoint::DateTime(b)) => Some((*b - *a).whole_days()),
            _ => None,
        }
    }
}

/// A range value (ADR-087 section 5): two bounds of one kind.
///
/// The only storable shape is half-open, `[start, end)`: the lower bound `Included`, the upper
/// bound `Excluded`, both finite, `start < end`. Every other shape is refused with
/// `InvalidValue` on write, by [`RangeValue::validate`], so every backend agrees:
///
/// - an **empty** range (`start >= end`, including `[3,3)`) is refused, because PostgreSQL
///   stores it as `empty`, which conflicts with nothing and would silently disable an exclusion;
/// - an **unbounded** bound is refused, because it would block a resource forever;
/// - an equal-but-inclusive range such as `[3,3]` is refused as empty in v1 (SEM-RNG-06); book a
///   point in time as a one-unit half-open range such as `[3,4)`;
/// - any other inclusivity, such as `(1,3]`, is refused: PostgreSQL canonicalises discrete ranges
///   to `[)`, and a value must come back as it went in.
///
/// Two half-open ranges touching at a bound do not overlap: `[1,3)` and `[3,5)` are disjoint,
/// `[1,3)` and `[2,4)` overlap (SEM-RNG-01).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RangeValue {
    /// The lower bound; must be `Included`.
    pub lower: Bound<RangePoint>,
    /// The upper bound; must be `Excluded`.
    pub upper: Bound<RangePoint>,
}

impl RangeValue {
    /// The half-open range `[start, end)`; check it with [`RangeValue::validate`].
    pub fn half_open(start: RangePoint, end: RangePoint) -> Self {
        Self {
            lower: Bound::Included(start),
            upper: Bound::Excluded(end),
        }
    }

    /// The payload kind, or `None` when the bounds are unbounded or of different kinds.
    pub fn of(&self) -> Option<RangeOf> {
        let l = bound_point(&self.lower).map(RangePoint::of);
        let u = bound_point(&self.upper).map(RangePoint::of);
        match (l, u) {
            (Some(a), Some(b)) if a == b => Some(a),
            _ => None,
        }
    }

    /// Whether this is a storable range: finite, one kind, half-open and not empty.
    pub fn validate(&self) -> Result<(), String> {
        let (Some(lo), Some(hi)) = (bound_point(&self.lower), bound_point(&self.upper)) else {
            return Err(
                "a range bound is unbounded or infinite; ranges must be finite (ADR-087)".into(),
            );
        };
        let Some(order) = lo.compare(hi) else {
            return Err("the two bounds of a range must be of the same kind".into());
        };
        if order != Ordering::Less {
            return Err(
                "the range is empty (start is not before end); empty ranges are not stored, and a \
                 point in time is booked as a one-unit range such as [3,4)"
                    .into(),
            );
        }
        if !matches!(self.lower, Bound::Included(_)) || !matches!(self.upper, Bound::Excluded(_)) {
            return Err("ranges are half-open, [start, end): start included, end excluded".into());
        }
        Ok(())
    }

    /// The start and end points of a valid range.
    pub fn bounds(&self) -> Option<(RangePoint, RangePoint)> {
        Some((*bound_point(&self.lower)?, *bound_point(&self.upper)?))
    }

    /// Whether two valid ranges of the same kind share a point: `a.start < b.end` and
    /// `b.start < a.end`, so ranges that only touch do not overlap.
    pub fn overlaps(&self, other: &RangeValue) -> bool {
        match (self.bounds(), other.bounds()) {
            (Some((a0, a1)), Some((b0, b1))) => {
                a0.compare(&b1) == Some(Ordering::Less) && b0.compare(&a1) == Some(Ordering::Less)
            }
            _ => false,
        }
    }
}

fn bound_point(b: &Bound<RangePoint>) -> Option<&RangePoint> {
    match b {
        Bound::Included(p) | Bound::Excluded(p) => Some(p),
        Bound::Unbounded => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u8) -> RangePoint {
        RangePoint::Date(
            Date::from_calendar_date(2026, time::Month::November, day)
                .unwrap_or_else(|e| panic!("{e}")),
        )
    }

    fn half_open(a: u8, b: u8) -> RangeValue {
        RangeValue::half_open(d(a), d(b))
    }

    #[test]
    fn a_geo_point_is_range_and_scale_checked() {
        let ok = |lon: (i64, u32), lat: (i64, u32)| {
            GeoPoint::new(Decimal::new(lon.0, lon.1), Decimal::new(lat.0, lat.1)).validate()
        };
        assert!(ok((1_800_000_000, 7), (-90, 0)).is_ok());
        assert!(ok((739_000_001, 7), (-338_688_197, 7)).is_ok());
        // Trailing zeros are not extra precision.
        assert!(ok((10_000_000_000, 9), (0, 0)).is_ok());
        assert!(ok((1_800_000_001, 7), (0, 0)).is_err());
        assert!(ok((0, 0), (900_000_001, 7)).is_err());
        assert!(ok((1, 8), (0, 0)).is_err());
    }

    #[test]
    fn only_finite_non_empty_half_open_ranges_are_valid() {
        assert!(half_open(1, 3).validate().is_ok());
        assert!(half_open(3, 3).validate().is_err(), "empty [3,3)");
        assert!(half_open(5, 3).validate().is_err(), "reversed");
        let both = |lower, upper| RangeValue { lower, upper };
        assert!(
            both(Bound::Included(d(3)), Bound::Included(d(3)))
                .validate()
                .is_err(),
            "[3,3] is refused as empty"
        );
        assert!(
            both(Bound::Included(d(3)), Bound::Included(d(5)))
                .validate()
                .is_err()
        );
        assert!(
            both(Bound::Excluded(d(3)), Bound::Excluded(d(5)))
                .validate()
                .is_err()
        );
        assert!(
            both(Bound::Included(d(3)), Bound::Unbounded)
                .validate()
                .is_err()
        );
        assert!(
            both(Bound::Unbounded, Bound::Excluded(d(3)))
                .validate()
                .is_err()
        );
        assert!(
            RangeValue::half_open(d(1), RangePoint::Int(2))
                .validate()
                .is_err(),
            "two kinds"
        );
    }

    #[test]
    fn touching_ranges_do_not_overlap() {
        assert!(!half_open(1, 3).overlaps(&half_open(3, 5)));
        assert!(half_open(1, 3).overlaps(&half_open(2, 4)));
        assert!(half_open(1, 5).overlaps(&half_open(2, 3)));
        assert!(!half_open(3, 5).overlaps(&half_open(1, 3)));
    }

    #[test]
    fn a_range_fits_only_a_range_field_of_its_kind() {
        let date_range = FieldType::Range { of: RangeOf::Date };
        let int_range = FieldType::Range { of: RangeOf::Int };
        let v = Value::Range(half_open(1, 3));
        assert!(v.fits(&date_range));
        assert!(!v.fits(&int_range));
        assert!(!v.fits(&FieldType::Date));
        let p = Value::GeoPoint(GeoPoint::new(Decimal::ZERO, Decimal::ZERO));
        assert!(p.fits(&FieldType::Geo { srid: 4326 }));
        assert!(!p.fits(&FieldType::Json));
        // Writes check what `fits` cannot: emptiness and coordinates.
        assert!(
            date_range
                .check(&Value::Range(half_open(3, 3)), true)
                .is_err()
        );
        assert!(date_range.check(&Value::Null, true).is_ok());
        assert!(date_range.check(&Value::Null, false).is_err());
    }
}
