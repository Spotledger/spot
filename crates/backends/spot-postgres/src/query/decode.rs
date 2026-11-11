//! Result rows to storage values, exactly: nothing is rounded, coerced or guessed.

use spot_pg_query::ColumnType;
use spot_storage_contract::{ErrorKind, RecordId, StorageError, StorageResult, Value};
use time::{Date, OffsetDateTime, Time, UtcOffset};
use tokio_postgres::Row;
use tokio_postgres::types::{FromSql, Type};
use uuid::Uuid;

use super::bind::JsonText;
use super::numeric::ExactNumeric;

/// Decodes one row by the compiler's column types.
pub(super) fn decode_row(row: &Row, types: &[ColumnType]) -> StorageResult<Vec<Value>> {
    types
        .iter()
        .enumerate()
        .map(|(i, ty)| decode(row, i, *ty))
        .collect()
}

fn get<'a, T: FromSql<'a>>(row: &'a Row, index: usize, ty: ColumnType) -> StorageResult<Option<T>> {
    row.try_get(index).map_err(|_| match ty {
        // See `ExactNumeric`: too many digits, too many fractional digits, or not a number.
        ColumnType::Decimal { .. } => StorageError::new(
            ErrorKind::InvalidValue,
            "a decimal result does not fit the exact decimal type (more than 28 fractional \
             digits or 96 bits of digits, or not a number); it was not rounded",
        ),
        _ => StorageError::new(
            ErrorKind::Internal,
            "a query result could not be read in the type the query compiler named",
        ),
    })
}

fn decode(row: &Row, index: usize, ty: ColumnType) -> StorageResult<Value> {
    fn some<T>(v: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
        v.map_or(Value::Null, f)
    }
    Ok(match ty {
        ColumnType::Bool => some(get::<bool>(row, index, ty)?, Value::Bool),
        // A system column such as a child row's `idx` is `integer`; widening is exact.
        ColumnType::Int => {
            let widened = match *row.columns()[index].type_() {
                Type::INT4 => get::<i32>(row, index, ty)?.map(i64::from),
                Type::INT2 => get::<i16>(row, index, ty)?.map(i64::from),
                _ => get::<i64>(row, index, ty)?,
            };
            some(widened, Value::Int)
        }
        ColumnType::Decimal { scale } => some(
            get::<ExactNumeric>(row, index, ty)?,
            |ExactNumeric(mut d)| {
                // Only ever widens the scale (a known scale the value does not show); an overflow
                // leaves the exact value as it came.
                if let Some(scale) = scale
                    && d.scale() < scale
                {
                    d.rescale(scale);
                }
                Value::Decimal(d)
            },
        ),
        ColumnType::Text => some(get::<String>(row, index, ty)?, Value::Text),
        ColumnType::Date => some(get::<Date>(row, index, ty)?, Value::Date),
        ColumnType::Time => some(get::<Time>(row, index, ty)?, Value::Time),
        ColumnType::DateTime => some(get::<OffsetDateTime>(row, index, ty)?, |t| {
            Value::DateTime(t.to_offset(UtcOffset::UTC))
        }),
        ColumnType::Id => match get::<Uuid>(row, index, ty)? {
            None => Value::Null,
            Some(u) => Value::Id(RecordId::from_bytes(*u.as_bytes())?),
        },
        ColumnType::Json => some(get::<JsonText>(row, index, ty)?, |j| Value::Json(j.0)),
        ColumnType::Bytes => some(get::<Vec<u8>>(row, index, ty)?, Value::Bytes),
    })
}
