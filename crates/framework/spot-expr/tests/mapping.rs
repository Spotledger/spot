//! The mapping to and from the storage value model.
#![allow(missing_docs, clippy::unwrap_used)]

use rust_decimal::Decimal;
use spot_expr::mapping::{from_storage, to_storage};
use spot_expr::{Date, DateTime, Value};
use spot_storage_contract::{RecordId, Value as Stored};

#[test]
fn scalars_round_trip() {
    let date = Date::from_ymd(2026, 10, 5).unwrap();
    let at = DateTime::parse("2026-10-05T10:20:30.250000Z").unwrap();
    for v in [
        Value::Null,
        Value::Bool(true),
        Value::Int(-7),
        Value::Decimal(Decimal::new(1250, 2)),
        Value::Text("héllo".into()),
        Value::Date(date),
        Value::DateTime(at),
    ] {
        let stored = to_storage(&v).unwrap();
        assert!(from_storage(&stored).unwrap().same(&v), "{v:?}");
    }
}

#[test]
fn an_id_becomes_its_text_and_the_unmappable_are_refused() {
    let id = RecordId::new_v7();
    assert_eq!(
        from_storage(&Stored::Id(id)),
        Ok(Value::Text(id.to_string()))
    );
    assert!(from_storage(&Stored::Json("{}".into())).is_err());
    assert!(from_storage(&Stored::Bytes(vec![1])).is_err());
    assert!(to_storage(&Value::List(vec![])).is_err());
}

#[test]
fn the_ends_of_the_date_range_map() {
    for text in ["0001-01-01", "9999-12-31"] {
        let d = Date::parse(text).unwrap();
        assert!(
            from_storage(&to_storage(&Value::Date(d)).unwrap())
                .unwrap()
                .same(&Value::Date(d))
        );
    }
}
