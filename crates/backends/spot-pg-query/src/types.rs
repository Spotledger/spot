//! The compiler's output types: typed parameters, result column types and [`CompiledQuery`].

use rust_decimal::Decimal;
use spot_storage_contract::{Name, QueryPath, RecordId};
use time::{Date, OffsetDateTime, Time};

/// The PostgreSQL type a parameter is bound as. Binding names the type explicitly, so it never
/// depends on type inference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgType {
    /// `bool`
    Bool,
    /// `int8`
    Int8,
    /// `numeric`
    Numeric,
    /// `text`
    Text,
    /// `date`
    Date,
    /// `time`
    Time,
    /// `timestamptz`
    TimestampTz,
    /// `uuid`
    Uuid,
    /// `jsonb`
    Jsonb,
    /// `bytea`
    Bytea,
}

impl PgType {
    /// The type's name in PostgreSQL.
    pub fn sql_name(self) -> &'static str {
        match self {
            PgType::Bool => "bool",
            PgType::Int8 => "int8",
            PgType::Numeric => "numeric",
            PgType::Text => "text",
            PgType::Date => "date",
            PgType::Time => "time",
            PgType::TimestampTz => "timestamptz",
            PgType::Uuid => "uuid",
            PgType::Jsonb => "jsonb",
            PgType::Bytea => "bytea",
        }
    }
}

/// A typed value bound to a `$n` placeholder. WP-STO-03 converts it to the driver's `ToSql`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Param {
    /// `bool`
    Bool(bool),
    /// `int8`
    Int8(i64),
    /// `numeric`
    Numeric(Decimal),
    /// `text`; never contains NUL.
    Text(String),
    /// `date`
    Date(Date),
    /// `time`
    Time(Time),
    /// `timestamptz`
    TimestampTz(OffsetDateTime),
    /// `uuid`
    Uuid(RecordId),
    /// `jsonb`, as JSON text.
    Jsonb(String),
    /// `bytea`
    Bytea(Vec<u8>),
    /// SQL null of the given type (a null literal in the IR, such as a `Coalesce` operand).
    Null(PgType),
}

impl Param {
    /// The type the parameter is bound as.
    pub fn pg_type(&self) -> PgType {
        match self {
            Param::Bool(_) => PgType::Bool,
            Param::Int8(_) => PgType::Int8,
            Param::Numeric(_) => PgType::Numeric,
            Param::Text(_) => PgType::Text,
            Param::Date(_) => PgType::Date,
            Param::Time(_) => PgType::Time,
            Param::TimestampTz(_) => PgType::TimestampTz,
            Param::Uuid(_) => PgType::Uuid,
            Param::Jsonb(_) => PgType::Jsonb,
            Param::Bytea(_) => PgType::Bytea,
            Param::Null(t) => *t,
        }
    }
}

/// How the executor turns a result column back into a [`Value`], and the type the compiler
/// inferred for an expression.
///
/// - `Int` columns are `int8`. `Sum` over integers is cast back to `int8`, so an overflow fails
///   in PostgreSQL with `22003`, which maps to `InvalidValue`, as in the fake.
/// - `Decimal` columns are `numeric`. The executor decodes them into `rust_decimal::Decimal` and
///   refuses a value that does not fit with `InvalidValue`. `scale` is the number of fractional
///   digits the expression is known to have (a field's declared scale, or the result of
///   arithmetic on known scales); the executor may rescale to it. `None` means unknown.
/// - A null literal in a select list decodes as `Text`; the value is null either way.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnType {
    /// `bool` → `Value::Bool`
    Bool,
    /// `int8` → `Value::Int`
    Int,
    /// `numeric` → `Value::Decimal`
    Decimal {
        /// Fractional digits, when known.
        scale: Option<u32>,
    },
    /// `text` → `Value::Text`
    Text,
    /// `date` → `Value::Date`
    Date,
    /// `time` → `Value::Time`
    Time,
    /// `timestamptz` → `Value::DateTime`
    DateTime,
    /// `uuid` → `Value::Id`
    Id,
    /// `jsonb` → `Value::Json`
    Json,
    /// `bytea` → `Value::Bytes`
    Bytes,
}

impl ColumnType {
    pub(crate) fn pg_type(self) -> PgType {
        match self {
            ColumnType::Bool => PgType::Bool,
            ColumnType::Int => PgType::Int8,
            ColumnType::Decimal { .. } => PgType::Numeric,
            ColumnType::Text => PgType::Text,
            ColumnType::Date => PgType::Date,
            ColumnType::Time => PgType::Time,
            ColumnType::DateTime => PgType::TimestampTz,
            ColumnType::Id => PgType::Uuid,
            ColumnType::Json => PgType::Jsonb,
            ColumnType::Bytes => PgType::Bytea,
        }
    }

    pub(crate) fn is_number(self) -> bool {
        matches!(self, ColumnType::Int | ColumnType::Decimal { .. })
    }

    /// Whether values of the two types can be compared or combined (numbers mix).
    pub(crate) fn compatible(self, other: ColumnType) -> bool {
        (self.is_number() && other.is_number())
            || std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }
}

/// The result of compiling a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledQuery {
    /// SQL text with `$1…$n` placeholders. No value of the query appears in it.
    pub sql: String,
    /// The values for `$1…$n`, in order.
    pub params: Vec<Param>,
    /// Result column names, from the selection aliases.
    pub columns: Vec<Name>,
    /// How to decode each result column.
    pub decode: Vec<ColumnType>,
    /// Passed through from the query. Index coverage is checked from query plans by WP-STO-03
    /// and CI, not here.
    pub path: QueryPath,
}
