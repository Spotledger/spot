//! The wire encoding: every value type round-trips through JSON (WP-DOC-07.T1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_activity::wire;
use spot_app_api::prelude::{Decimal, Value};
use time::macros::{date, datetime, time};

#[test]
fn null_and_bools_round_trip() {
    for v in [Value::Null, Value::Bool(true), Value::Bool(false)] {
        let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
        let back = wire::helpers::value_from_json(&json).unwrap();
        assert_eq!(back, v);
    }
}

#[test]
fn ints_round_trip() {
    for v in [
        Value::Int(0),
        Value::Int(-1),
        Value::Int(i64::MAX),
        Value::Int(i64::MIN),
    ] {
        let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
        assert_eq!(wire::helpers::value_from_json(&json).unwrap(), v);
    }
}

#[test]
fn decimals_stay_strings_with_scale() {
    let v = dec("10.50");
    let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
    assert_eq!(json, r#""10.50""#);
    let back = wire::helpers::value_from_json(&json).unwrap();
    assert_eq!(back, v);
}

#[test]
fn a_decimal_json_number_is_refused() {
    let e = wire::helpers::value_from_json("10.50").unwrap_err();
    assert!(e.contains("JSON string"), "{e}");
}

#[test]
fn text_dates_times_and_timestamps_round_trip() {
    let cases = [
        (Value::Text("plain".into()), r#""plain""#),
        // `2026-10-06` reads back a date, not text — the shape decides (documented).
        (Value::Date(date!(2026 - 10 - 06)), r#""2026-10-06""#),
        (Value::Time(time!(13:04:05)), r#""13:04:05""#),
        (
            Value::DateTime(datetime!(2026-10-06 13:04:05 UTC)),
            r#""2026-10-06T13:04:05Z""#,
        ),
    ];
    for (v, json_text) in cases {
        let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
        assert_eq!(json, json_text, "{v:?}");
        assert_eq!(wire::helpers::value_from_json(&json).unwrap(), v);
    }
}

#[test]
fn time_fractions_trim() {
    let v = Value::Time(time!(13:04:05.500000));
    let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
    assert_eq!(json, r#""13:04:05.5""#);
}

#[test]
fn ids_stay_canonical_uuidv7() {
    let id = uuid7(2, 3);
    let v = Value::Id(id);
    let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
    assert_eq!(json, format!("\"{id}\""));
    assert_eq!(wire::helpers::value_from_json(&json).unwrap(), v);
}

#[test]
fn a_non_canonical_id_text_is_refused() {
    // Upper-case hex is the same uuid but not the canonical text form.
    let id = uuid7(2, 3);
    let upper = id.to_string().to_uppercase();
    assert!(wire::helpers::value_from_json(&format!("\"{upper}\"")).is_err());
}

#[test]
fn json_is_embedded_not_stringified() {
    let v = Value::Json(r#"{"a":1}"#.to_owned());
    let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
    assert_eq!(json, r#"{"a":1}"#);
    let back = wire::helpers::value_from_json(&json).unwrap();
    assert_eq!(back, v);
}

#[test]
fn bytes_are_base64() {
    // Padded base64 is the unmistakable form and reads back as bytes. The wire module
    // documents why a type-less read decodes nothing else.
    for data in [b"a".as_slice(), b"ab".as_slice(), b"hello world".as_slice()] {
        let v = Value::Bytes(data.to_vec());
        let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
        let back = wire::helpers::value_from_json(&json).unwrap();
        assert_eq!(back, v, "round trip of {data:?} through {json}");
    }
    // The exact base64 alphabet: `hello` is `aGVsbG8=`.
    let v = Value::Bytes(b"hello".to_vec());
    let json = serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap();
    assert_eq!(json, r#""aGVsbG8=""#);
    // Shape's limits, documented in wire.rs: empty bytes and unpadded base64 are text on a
    // type-less read (`""` is Text per ADR-055, `YWJj` is four letters as easily as it is
    // three bytes); a caller that knows the field is Bytes decodes the text itself.
    assert_eq!(
        wire::helpers::value_from_json(r#""""#).unwrap(),
        Value::Text(String::new())
    );
    assert_eq!(
        wire::helpers::value_from_json(r#""YWJj""#).unwrap(),
        Value::Text("YWJj".into())
    );
}

#[test]
fn record_ids_round_trip_through_the_with_module() {
    let id = uuid7(1, 2);
    let json = wire::helpers::value_to_json(&Value::Id(id)).to_string();
    let back = wire::helpers::value_from_json(&json).unwrap();
    assert_eq!(back, Value::Id(id));
}

#[test]
fn timestamps_normalise_to_utc() {
    // An instant with an offset reads back in UTC.
    let json = "\"2026-10-06T18:04:05+05:00\"";
    let back = wire::helpers::value_from_json(json).unwrap();
    assert_eq!(back, Value::DateTime(datetime!(2026-10-06 13:04:05 UTC)));
}

#[test]
fn row_snapshots_round_trip() {
    let (r1, r2) = ids();
    let row = row_with(r1, "bolt", dec("2.500"));
    let json = wire::row::to_json(&row).to_string();
    let back = wire::row::parse(serde_json::from_str(&json).unwrap()).unwrap();
    assert_eq!(back, row);
    let _ = r2;
}

#[test]
fn decimal_display_with_scale_is_on_the_wire() {
    // The wire keeps scale as written: `0.10` stays `0.10` and is *not* `0.1`.
    let v = Value::Decimal(Decimal::from_str_exact("0.10").unwrap());
    assert_eq!(
        serde_json::to_string(&wire::helpers::value_to_json(&v)).unwrap(),
        r#""0.10""#
    );
}
