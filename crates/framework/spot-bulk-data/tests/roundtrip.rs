//! Round trip: a value exported here imports back through `spot-import` identically.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_bulk_data::format::{self, Format};
use spot_import::convert::{ConversionOptions, Formats, convert};
use spot_meta::{FieldSpec, FieldType};
use spot_storage_contract::{RecordId, Value};
use time::{Date, Month, OffsetDateTime, Time};

fn formats() -> Formats {
    let (d, dt, t) = Formats::default_lists();
    Formats::build(&d, &dt, &t).unwrap()
}

fn spec(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

fn decimal_spec(scale: u8) -> FieldSpec {
    let mut s = spec("v", FieldType::Decimal);
    s.scale = Some(scale);
    s
}

/// One value shape: the field it belongs to and the value.
fn cases() -> Vec<(FieldSpec, Value)> {
    let date = Date::from_calendar_date(2026, Month::January, 31).unwrap();
    let time = Time::from_hms(9, 30, 15).unwrap();
    let time_frac = Time::from_hms_nano(9, 30, 15, 500_000_000).unwrap();
    let instant = OffsetDateTime::from_unix_timestamp(1_769_853_600).unwrap();
    vec![
        // 1-5: text shapes.
        (spec("v", FieldType::Text), Value::Text("hello".into())),
        (
            spec("v", FieldType::Text),
            Value::Text("line one\nline two".into()),
        ),
        (
            spec("v", FieldType::Text),
            Value::Text("Ünïcøde — 漢字 — Ω".into()),
        ),
        (
            spec("v", FieldType::Text),
            Value::Text("commas, \"quotes\" and 'apostrophes'".into()),
        ),
        (
            spec("v", FieldType::LongText),
            Value::Text("a long note\nwith a newline and a tab\there".into()),
        ),
        // 6-8: numbers.
        (
            decimal_spec(2),
            Value::Decimal(rust_decimal::Decimal::from_str_exact("1.50").unwrap()),
        ),
        (
            decimal_spec(2),
            Value::Decimal(rust_decimal::Decimal::from_str_exact("-2.50").unwrap()),
        ),
        (spec("v", FieldType::Int), Value::Int(42)),
        // 9-10: booleans.
        (spec("v", FieldType::Bool), Value::Bool(true)),
        (spec("v", FieldType::Bool), Value::Bool(false)),
        // 11-13: date and time.
        (spec("v", FieldType::Date), Value::Date(date)),
        (spec("v", FieldType::Time), Value::Time(time)),
        (spec("v", FieldType::Time), Value::Time(time_frac)),
        // 14: instant.
        (spec("v", FieldType::DateTime), Value::DateTime(instant)),
        // 15: link id.
        (spec("v", FieldType::Link), Value::Id(RecordId::new_v7())),
        // 16: JSON.
        (spec("v", FieldType::Json), Value::Json("{\"a\":1}".into())),
    ]
}

#[test]
fn every_value_shape_round_trips_through_spot_import() {
    let formats = formats();
    let options = ConversionOptions::default();
    let all = cases();
    assert!(all.len() >= 12, "the brief asks for twelve shapes");
    for (spec, value) in all {
        let text = format::value_text(&value);
        let back = convert(&spec, &text, &options, &formats)
            .unwrap_or_else(|e| panic!("{} -> `{text}`: {}", spec.name, e.message));
        assert_eq!(
            back,
            Some(value.clone()),
            "shape for `{}` did not round trip",
            spec.name
        );
    }
}

/// The export writes instants with six fractional digits (the brief's form), even though
/// `spot-import`'s converter currently truncates the fraction on read.
#[test]
fn instants_are_written_with_microseconds() {
    let instant = OffsetDateTime::from_unix_timestamp(1_769_853_600)
        .unwrap()
        .replace_nanosecond(123_456_000)
        .unwrap();
    let text = format::value_text(&Value::DateTime(instant));
    assert_eq!(text, "2026-01-31T10:00:00.123456Z");
    let whole = format::value_text(&Value::DateTime(
        OffsetDateTime::from_unix_timestamp(1_769_853_600).unwrap(),
    ));
    assert_eq!(whole, "2026-01-31T10:00:00.000000Z");
}

/// A whole CSV row with nasty text survives a real `spot-import` read.
#[test]
fn a_nasty_csv_row_reads_back_through_spot_import() {
    let columns = vec!["v".to_owned()];
    let nasty = "=cmd, \"quoted\"\nnewline";
    let bytes = format::row(
        &columns,
        &[Value::Text(nasty.to_owned())],
        Format::Csv,
        false,
    );
    let mut file = bytes.clone();
    file.splice(0..0, b"v\n".iter().copied());
    let path = std::env::temp_dir().join(format!("bulk-roundtrip-{}.csv", std::process::id()));
    std::fs::write(&path, &file).unwrap();
    let mut source = spot_import::CsvSource::open(&path, &spot_import::Limits::default()).unwrap();
    use spot_import::RowSource as _;
    let row = source.rows().next().unwrap().unwrap();
    assert_eq!(row.cells[0].as_str(), nasty);
    let _ = std::fs::remove_file(&path);
}
