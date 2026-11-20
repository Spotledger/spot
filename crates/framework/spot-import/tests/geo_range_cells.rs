//! Export then import: geo and range cells round-trip exactly (WP-DOC-09.T7).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use rust_decimal::Decimal;
use spot_bulk_data::format::value_text;
use spot_import::convert::{ConversionOptions, Formats, convert};
use spot_meta::{FieldSpec, FieldType};
use spot_storage_contract::{GeoPoint, RangePoint, RangeValue, Value};
use time::{Date, Month, OffsetDateTime};

fn formats() -> Formats {
    let (d, dt, t) = Formats::default_lists();
    Formats::build(&d, &dt, &t).unwrap()
}

fn geo_spec() -> FieldSpec {
    FieldSpec::new("place", FieldType::Geo)
}

fn range_spec(format: &str) -> FieldSpec {
    let mut spec = FieldSpec::new("span", FieldType::Range);
    spec.format = Some(format.to_owned());
    spec
}

fn dec(text: &str) -> Decimal {
    Decimal::from_str_exact(text).unwrap()
}

fn round_trip(spec: &FieldSpec, value: &Value) {
    let text = value_text(value);
    let back = convert(spec, &text, &ConversionOptions::default(), &formats())
        .unwrap()
        .unwrap();
    assert_eq!(&back, value, "{text}");
}

#[test]
fn points_round_trip_at_the_extremes() {
    for (lon, lat) in [
        ("-180", "-90"),
        ("180", "90"),
        ("0", "0"),
        ("67.0011", "24.8607"),
        ("-0.1234567", "51.7654321"),
        ("12", "-45"),
    ] {
        let p = GeoPoint::new(dec(lon), dec(lat));
        let text = value_text(&Value::GeoPoint(p));
        let back = convert(
            &geo_spec(),
            &text,
            &ConversionOptions::default(),
            &formats(),
        )
        .unwrap()
        .unwrap();
        let Value::GeoPoint(back) = back else {
            panic!("not a point");
        };
        assert_eq!(back.lon.normalize(), p.lon.normalize());
        assert_eq!(back.lat.normalize(), p.lat.normalize());
    }
}

#[test]
fn date_and_instant_ranges_round_trip() {
    let d = |m: Month, day: u8| Date::from_calendar_date(2026, m, day).unwrap();
    round_trip(
        &range_spec("date_range"),
        &Value::Range(RangeValue::half_open(
            RangePoint::Date(d(Month::January, 1)),
            RangePoint::Date(d(Month::March, 31)),
        )),
    );
    let t = |s: i64, nanos: i128| {
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(s) * 1_000_000_000 + nanos).unwrap()
    };
    round_trip(
        &range_spec("instant_range"),
        &Value::Range(RangeValue::half_open(
            RangePoint::DateTime(t(1_767_254_400, 0)),
            RangePoint::DateTime(t(1_767_258_000, 250_000_000)),
        )),
    );
}

#[test]
fn a_thousand_rows_convert_exactly() {
    let spec = range_spec("date_range");
    let formats = formats();
    for i in 0..1_000i32 {
        let start = Date::from_julian_day(2_460_000 + i).unwrap();
        let end = Date::from_julian_day(2_460_000 + i + 1 + i % 30).unwrap();
        let value = Value::Range(RangeValue::half_open(
            RangePoint::Date(start),
            RangePoint::Date(end),
        ));
        let text = value_text(&value);
        let back = convert(&spec, &text, &ConversionOptions::default(), &formats)
            .unwrap()
            .unwrap();
        assert_eq!(back, value);
    }
}

#[test]
fn a_bad_cell_names_the_field_and_echoes_at_most_forty_characters() {
    let long = format!("POINT({})", "7".repeat(120));
    let error = convert(
        &geo_spec(),
        &long,
        &ConversionOptions::default(),
        &formats(),
    )
    .unwrap_err();
    assert!(error.message.contains("place"), "{}", error.message);
    assert!(
        !error.message.contains(&"7".repeat(41)),
        "{}",
        error.message
    );
    let error = convert(
        &range_spec("date_range"),
        "[,2026-01-01)",
        &ConversionOptions::default(),
        &formats(),
    )
    .unwrap_err();
    assert!(error.message.contains("span"));
}

#[test]
fn an_empty_cell_stays_unset() {
    let got = convert(&geo_spec(), "", &ConversionOptions::default(), &formats()).unwrap();
    assert_eq!(got, None);
}
