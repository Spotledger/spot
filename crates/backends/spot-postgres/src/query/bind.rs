//! Compiled parameters as driver values, each with the exact PostgreSQL type it is bound as.

use bytes::{BufMut, BytesMut};
use rust_decimal::Decimal;
use spot_pg_query::{Param, PgType};
use time::{Date, OffsetDateTime, Time};
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, accepts, to_sql_checked};
use uuid::Uuid;

type BoxError = Box<dyn std::error::Error + Sync + Send>;

/// JSON text in a `jsonb` column: binary `jsonb` is a version byte (1) and the JSON text.
#[derive(Debug)]
pub(super) struct JsonText(pub(super) String);

impl ToSql for JsonText {
    fn to_sql(&self, _ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        out.put_u8(1);
        out.put_slice(self.0.as_bytes());
        Ok(IsNull::No)
    }

    accepts!(JSONB);
    to_sql_checked!();
}

impl<'a> FromSql<'a> for JsonText {
    fn from_sql(_ty: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        match raw.split_first() {
            Some((1, text)) => Ok(JsonText(std::str::from_utf8(text)?.to_owned())),
            _ => Err("unsupported jsonb version".into()),
        }
    }

    accepts!(JSONB);
}

fn uuid_of(id: spot_storage_contract::RecordId) -> Uuid {
    Uuid::from_bytes(*id.as_bytes())
}

/// A bound value and the type it is sent as.
pub(super) struct Bound {
    pub(super) value: Box<dyn ToSql + Sync + Send>,
    pub(super) ty: Type,
}

/// The driver type of a compiler parameter type.
pub(super) fn driver_type(ty: PgType) -> Type {
    match ty {
        PgType::Bool => Type::BOOL,
        PgType::Int8 => Type::INT8,
        PgType::Numeric => Type::NUMERIC,
        PgType::Text => Type::TEXT,
        PgType::Date => Type::DATE,
        PgType::Time => Type::TIME,
        PgType::TimestampTz => Type::TIMESTAMPTZ,
        PgType::Uuid => Type::UUID,
        PgType::Jsonb => Type::JSONB,
        PgType::Bytea => Type::BYTEA,
    }
}

fn null_of(ty: PgType) -> Box<dyn ToSql + Sync + Send> {
    match ty {
        PgType::Bool => Box::new(Option::<bool>::None),
        PgType::Int8 => Box::new(Option::<i64>::None),
        PgType::Numeric => Box::new(Option::<Decimal>::None),
        PgType::Text => Box::new(Option::<String>::None),
        PgType::Date => Box::new(Option::<Date>::None),
        PgType::Time => Box::new(Option::<Time>::None),
        PgType::TimestampTz => Box::new(Option::<OffsetDateTime>::None),
        PgType::Uuid => Box::new(Option::<Uuid>::None),
        PgType::Jsonb => Box::new(Option::<JsonText>::None),
        PgType::Bytea => Box::new(Option::<Vec<u8>>::None),
    }
}

/// Converts every parameter, in order.
pub(super) fn bind(params: &[Param]) -> Vec<Bound> {
    params
        .iter()
        .map(|p| {
            let value: Box<dyn ToSql + Sync + Send> = match p {
                Param::Bool(v) => Box::new(*v),
                Param::Int8(v) => Box::new(*v),
                Param::Numeric(v) => Box::new(*v),
                Param::Text(v) => Box::new(v.clone()),
                Param::Date(v) => Box::new(*v),
                Param::Time(v) => Box::new(*v),
                Param::TimestampTz(v) => Box::new(*v),
                Param::Uuid(v) => Box::new(uuid_of(*v)),
                Param::Jsonb(v) => Box::new(JsonText(v.clone())),
                Param::Bytea(v) => Box::new(v.clone()),
                Param::Null(t) => null_of(*t),
            };
            Bound {
                value,
                ty: driver_type(p.pg_type()),
            }
        })
        .collect()
}
