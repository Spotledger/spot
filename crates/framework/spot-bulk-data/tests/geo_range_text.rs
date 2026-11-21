//! Geo points and ranges print in the one documented text form (WP-DOC-09.T7), never as the
//! Debug dump.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::ops::Bound;

use rust_decimal::Decimal;
use spot_bulk_data::format::{self, Format, value_text};
use spot_storage_contract::{GeoPoint, RangePoint, RangeValue, Value};
use time::{Date, Month, OffsetDateTime};

fn point() -> Value {
    Value::GeoPoint(GeoPoint::new(
        Decimal::from_str_exact("67.0011000").unwrap(),
        Decimal::from_str_exact("24.8607").unwrap(),
    ))
}

fn int_range() -> Value {
    Value::Range(RangeValue::half_open(
        RangePoint::Int(3),
        RangePoint::Int(10),
    ))
}

fn date_range() -> Value {
    Value::Range(RangeValue::half_open(
        RangePoint::Date(Date::from_calendar_date(2026, Month::January, 1).unwrap()),
        RangePoint::Date(Date::from_calendar_date(2026, Month::February, 1).unwrap()),
    ))
}

fn instant_range() -> Value {
    Value::Range(RangeValue::half_open(
        RangePoint::DateTime(OffsetDateTime::from_unix_timestamp(1_767_254_400).unwrap()),
        RangePoint::DateTime(OffsetDateTime::from_unix_timestamp(1_767_258_000).unwrap()),
    ))
}

#[test]
fn value_text_is_the_documented_form() {
    assert_eq!(value_text(&point()), "POINT(67.0011 24.8607)");
    assert_eq!(value_text(&int_range()), "[3,10)");
    assert_eq!(value_text(&date_range()), "[2026-01-01,2026-02-01)");
    assert_eq!(
        value_text(&instant_range()),
        "[2026-01-01T08:00:00.000000Z,2026-01-01T09:00:00.000000Z)"
    );
    for v in [point(), int_range(), date_range(), instant_range()] {
        assert!(!value_text(&v).contains("GeoPoint"));
        assert!(!value_text(&v).contains("Range"));
    }
}

#[test]
fn a_range_that_cannot_print_is_empty_text_not_a_panic() {
    let bad = Value::Range(RangeValue {
        lower: Bound::Included(RangePoint::Int(1)),
        upper: Bound::Unbounded,
    });
    assert_eq!(value_text(&bad), "");
}

#[test]
fn csv_lines_are_golden() {
    let columns = vec!["at".to_owned(), "span".to_owned()];
    let line = format::row(&columns, &[point(), int_range()], Format::Csv, false);
    // The range holds a comma, so the whole cell is CSV-quoted.
    assert_eq!(
        String::from_utf8(line).unwrap(),
        "POINT(67.0011 24.8607),\"[3,10)\"\n"
    );
}

#[test]
fn ndjson_lines_are_golden() {
    let columns = vec!["at".to_owned(), "span".to_owned()];
    let line = format::row(&columns, &[point(), date_range()], Format::Ndjson, false);
    assert_eq!(
        String::from_utf8(line).unwrap(),
        "{\"at\":\"POINT(67.0011 24.8607)\",\"span\":\"[2026-01-01,2026-02-01)\"}\n"
    );
}
