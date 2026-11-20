//! Round trips, refusals and the no-float rule.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use rust_decimal::Decimal;
use spot_spatial_text::{Reason, format_point, format_range, parse_point, parse_range};
use spot_storage_contract::{GeoPoint, RangeOf, RangePoint, RangeValue};
use time::{Date, OffsetDateTime};

fn dec(s: &str) -> Decimal {
    Decimal::from_str_exact(s).unwrap()
}

#[test]
fn golden_point_text() {
    let p = GeoPoint::new(dec("67.0011000"), dec("24.8607"));
    assert_eq!(format_point(&p), "POINT(67.0011 24.8607)");
    assert_eq!(
        format_point(&GeoPoint::new(dec("-180"), dec("-90"))),
        "POINT(-180 -90)"
    );
    assert_eq!(
        parse_point("point (1 2)").unwrap(),
        GeoPoint::new(dec("1"), dec("2"))
    );
    assert_eq!(
        parse_point("  POINT( 67.0011  24.8607 )  ").unwrap(),
        GeoPoint::new(dec("67.0011"), dec("24.8607"))
    );
}

#[test]
fn golden_range_text() {
    let int = RangeValue::half_open(RangePoint::Int(3), RangePoint::Int(10));
    assert_eq!(format_range(&int).unwrap(), "[3,10)");
    let d = RangeValue::half_open(
        RangePoint::Date(Date::from_calendar_date(2026, time::Month::January, 1).unwrap()),
        RangePoint::Date(Date::from_calendar_date(2026, time::Month::February, 1).unwrap()),
    );
    assert_eq!(format_range(&d).unwrap(), "[2026-01-01,2026-02-01)");
    let t0 = OffsetDateTime::from_unix_timestamp(1_767_254_400).unwrap();
    let t1 = OffsetDateTime::from_unix_timestamp(1_767_258_000).unwrap();
    let t = RangeValue::half_open(RangePoint::DateTime(t0), RangePoint::DateTime(t1));
    assert_eq!(
        format_range(&t).unwrap(),
        "[2026-01-01T08:00:00.000000Z,2026-01-01T09:00:00.000000Z)"
    );
    assert_eq!(
        parse_range(&format_range(&t).unwrap(), RangeOf::DateTime).unwrap(),
        t
    );
}

#[test]
fn refused_points_carry_their_reason() {
    let cases = [
        ("POINT(181 0)", Reason::OutOfRange),
        ("POINT(0 91)", Reason::OutOfRange),
        ("POINT(1.12345678 2)", Reason::TooManyDecimals),
        ("POINT(1)", Reason::Syntax),
        ("POINT(1 2 3)", Reason::Syntax),
        ("POINT(1e3 2)", Reason::Syntax),
        ("POINT(1,5 2)", Reason::Syntax),
        ("POINT 1 2", Reason::Syntax),
        ("", Reason::Syntax),
    ];
    for (cell, reason) in cases {
        assert_eq!(parse_point(cell).unwrap_err().reason, reason, "{cell}");
    }
}

#[test]
fn refused_ranges_carry_their_reason() {
    let int = RangeOf::Int;
    let cases = [
        ("[3,3)", int, Reason::Empty),
        ("[4,3)", int, Reason::Empty),
        ("[1,3]", int, Reason::NotHalfOpen),
        ("(1,3)", int, Reason::NotHalfOpen),
        ("[1,)", int, Reason::Unbounded),
        ("[,3)", int, Reason::Unbounded),
        ("[a,b)", int, Reason::Syntax),
        ("[1, 3)", int, Reason::Syntax),
        ("[1,2,3)", int, Reason::Syntax),
        ("1,3", int, Reason::Syntax),
        (
            "[2026-01-01,2026-02-01)",
            RangeOf::DateTime,
            Reason::KindMismatch,
        ),
        ("[1,2026-01-01)", int, Reason::KindMismatch),
    ];
    for (cell, of, reason) in cases {
        assert_eq!(parse_range(cell, of).unwrap_err().reason, reason, "{cell}");
    }
    assert!(parse_range("  [1,3)  ", int).is_ok());
}

#[test]
fn messages_repeat_at_most_forty_characters() {
    let long = format!("POINT({})", "9".repeat(100));
    let error = parse_point(&long).unwrap_err();
    assert!(!error.message().contains(&"9".repeat(41)));
}

#[test]
fn format_refuses_unstorable_ranges() {
    use std::ops::Bound;
    let closed = RangeValue {
        lower: Bound::Included(RangePoint::Int(1)),
        upper: Bound::Included(RangePoint::Int(3)),
    };
    assert_eq!(
        format_range(&closed).unwrap_err().reason,
        Reason::NotHalfOpen
    );
    let open = RangeValue {
        lower: Bound::Included(RangePoint::Int(1)),
        upper: Bound::Unbounded,
    };
    assert_eq!(format_range(&open).unwrap_err().reason, Reason::Unbounded);
}

#[test]
fn the_crate_has_no_float() {
    let banned = [format!("f{}", 64), format!("f{}", 32)];
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        for word in &banned {
            assert!(!text.contains(word.as_str()), "{} in {path:?}", word);
        }
    }
}

proptest! {
    #[test]
    fn point_round_trips(lon in -1_800_000_000i64..=1_800_000_000, lat in -900_000_000i64..=900_000_000) {
        let p = GeoPoint::new(Decimal::new(lon, 7), Decimal::new(lat, 7));
        let text = format_point(&p);
        let back = parse_point(&text).unwrap();
        prop_assert_eq!(back.lon.normalize(), p.lon.normalize());
        prop_assert_eq!(back.lat.normalize(), p.lat.normalize());
        prop_assert_eq!(format_point(&back), text);
    }

    #[test]
    fn int_range_round_trips(a in -1_000_000i64..1_000_000, len in 1i64..1_000_000) {
        let r = RangeValue::half_open(RangePoint::Int(a), RangePoint::Int(a + len));
        prop_assert_eq!(parse_range(&format_range(&r).unwrap(), RangeOf::Int).unwrap(), r);
    }

    #[test]
    fn decimal_range_round_trips(a in -1_000_000i64..1_000_000, len in 1i64..1_000_000, scale in 0u32..6) {
        let r = RangeValue::half_open(
            RangePoint::Decimal(Decimal::new(a, scale)),
            RangePoint::Decimal(Decimal::new(a + len, scale)),
        );
        let back = parse_range(&format_range(&r).unwrap(), RangeOf::Decimal).unwrap();
        prop_assert_eq!(back, r);
    }

    #[test]
    fn date_range_round_trips(a in 2_415_021i32..2_500_000, len in 1i32..4000) {
        let d0 = Date::from_julian_day(a).unwrap();
        let d1 = Date::from_julian_day(a + len).unwrap();
        let r = RangeValue::half_open(RangePoint::Date(d0), RangePoint::Date(d1));
        prop_assert_eq!(parse_range(&format_range(&r).unwrap(), RangeOf::Date).unwrap(), r);
    }

    #[test]
    fn instant_range_round_trips(a in 0i64..4_000_000_000, micros in 0i128..1_000_000, len in 1i64..100_000) {
        let t0 = OffsetDateTime::from_unix_timestamp_nanos(i128::from(a) * 1_000_000_000 + micros * 1_000).unwrap();
        let t1 = t0 + time::Duration::seconds(len);
        let r = RangeValue::half_open(RangePoint::DateTime(t0), RangePoint::DateTime(t1));
        prop_assert_eq!(parse_range(&format_range(&r).unwrap(), RangeOf::DateTime).unwrap(), r);
    }
}
