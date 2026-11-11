//! The mapping between the language's values and the storage value model
//! (`spot_storage_contract::Value`, the `Value` of the app API).
//!
//! A record id becomes its canonical text; `Time`, `Json`, `Bytes`, geo points and ranges have no counterpart in
//! the language and are refused. A list has no storage counterpart either.

use spot_storage_contract::Value as Stored;
use time::{Date as TimeDate, Month, OffsetDateTime};

use crate::value::{Date, DateTime, Value};

/// A value that does not map.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a {0} value has no counterpart")]
pub struct Unmappable(pub &'static str);

/// A storage value as a language value.
pub fn from_storage(v: &Stored) -> Result<Value, Unmappable> {
    Ok(match v {
        Stored::Null => Value::Null,
        Stored::Bool(b) => Value::Bool(*b),
        Stored::Int(n) => Value::Int(*n),
        Stored::Decimal(d) => Value::Decimal(*d),
        Stored::Text(s) => Value::Text(s.clone()),
        Stored::Id(id) => Value::Text(id.to_string()),
        Stored::Date(d) => Value::Date(
            Date::from_ymd(d.year(), u32::from(u8::from(d.month())), u32::from(d.day()))
                .ok_or(Unmappable("date"))?,
        ),
        Stored::DateTime(t) => {
            let micros = t.unix_timestamp_nanos().div_euclid(1000);
            let micros = i64::try_from(micros).map_err(|_| Unmappable("datetime"))?;
            Value::DateTime(DateTime::from_micros(micros).ok_or(Unmappable("datetime"))?)
        }
        Stored::Time(_) => return Err(Unmappable("time")),
        Stored::Json(_) => return Err(Unmappable("json")),
        Stored::Bytes(_) => return Err(Unmappable("bytes")),
        Stored::GeoPoint(_) => return Err(Unmappable("geo point")),
        Stored::Range(_) => return Err(Unmappable("range")),
    })
}

/// A language value as a storage value. Lists do not map.
pub fn to_storage(v: &Value) -> Result<Stored, Unmappable> {
    Ok(match v {
        Value::Null => Stored::Null,
        Value::Bool(b) => Stored::Bool(*b),
        Value::Int(n) => Stored::Int(*n),
        Value::Decimal(d) => Stored::Decimal(*d),
        Value::Text(s) => Stored::Text(s.clone()),
        Value::Date(d) => {
            let (y, m, day) = d.ymd();
            let month = Month::try_from(u8::try_from(m).map_err(|_| Unmappable("date"))?)
                .map_err(|_| Unmappable("date"))?;
            Stored::Date(
                TimeDate::from_calendar_date(
                    y,
                    month,
                    u8::try_from(day).map_err(|_| Unmappable("date"))?,
                )
                .map_err(|_| Unmappable("date"))?,
            )
        }
        Value::DateTime(t) => Stored::DateTime(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(t.micros()) * 1000)
                .map_err(|_| Unmappable("datetime"))?,
        ),
        Value::List(_) => return Err(Unmappable("list")),
    })
}
