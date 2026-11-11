//! The value a rule binds: a record key, or a scalar from storage or a literal.
//!
//! Rules never see floats (ADR-12): a decimal is [`rust_decimal::Decimal`] and an integer is
//! `i64`. A value with no counterpart in the rule language (a time of day, JSON, raw bytes) is
//! dropped rather than guessed, so a rule can never read it.

use std::fmt;

use rust_decimal::Decimal;
use spot_expr::mapping;
use spot_expr::{Date, DateTime, Value as ExprValue};
use spot_storage_contract::{NodeRef, Value};

/// One value a rule binds.
///
/// `Ord` is total and deterministic (records by entity then id, then the scalar variants in
/// declaration order), which is what makes every result set sorted without a hash-map order
/// leak.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Datum {
    /// A record key: an edge endpoint or a link target.
    Record(NodeRef),
    /// No value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit integer.
    Int(i64),
    /// An exact decimal.
    Decimal(Decimal),
    /// Text.
    Text(String),
    /// A calendar date.
    Date(Date),
    /// An instant in UTC.
    DateTime(DateTime),
}

impl Datum {
    /// A record key.
    #[must_use]
    pub fn record(node: NodeRef) -> Self {
        Self::Record(node)
    }

    /// A storage value as a datum, or `None` when the rule language has no counterpart (a
    /// time of day, JSON, raw bytes). A record id from a link field reads as its text, since a
    /// link is an edge and never a field predicate.
    #[must_use]
    pub fn from_storage(value: &Value) -> Option<Self> {
        let expr = mapping::from_storage(value).ok()?;
        Self::from_expr(&expr)
    }

    /// A language value as a datum, or `None` for a list (rules bind scalars and records only).
    #[must_use]
    pub fn from_expr(value: &ExprValue) -> Option<Self> {
        Some(match value {
            ExprValue::Null => Self::Null,
            ExprValue::Bool(b) => Self::Bool(*b),
            ExprValue::Int(n) => Self::Int(*n),
            ExprValue::Decimal(d) => Self::Decimal(*d),
            ExprValue::Text(s) => Self::Text(s.clone()),
            ExprValue::Date(d) => Self::Date(*d),
            ExprValue::DateTime(t) => Self::DateTime(*t),
            ExprValue::List(_) => return None,
        })
    }

    /// The datum as a language value, for a guard's context.
    #[must_use]
    pub fn to_expr(&self) -> ExprValue {
        match self {
            Self::Record(node) => ExprValue::Text(record_text(node)),
            Self::Null => ExprValue::Null,
            Self::Bool(b) => ExprValue::Bool(*b),
            Self::Int(n) => ExprValue::Int(*n),
            Self::Decimal(d) => ExprValue::Decimal(*d),
            Self::Text(s) => ExprValue::Text(s.clone()),
            Self::Date(d) => ExprValue::Date(*d),
            Self::DateTime(t) => ExprValue::DateTime(*t),
        }
    }

    /// The type's name, for messages.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Record(_) => "record",
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Decimal(_) => "decimal",
            Self::Text(_) => "text",
            Self::Date(_) => "date",
            Self::DateTime(_) => "datetime",
        }
    }

    /// The canonical text used in a content hash: a tag, then the value, tab-separated.
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            Self::Record(node) => format!("r\t{}", record_text(node)),
            Self::Null => "n".to_owned(),
            Self::Bool(b) => format!("b\t{b}"),
            Self::Int(n) => format!("i\t{n}"),
            Self::Decimal(d) => format!("d\t{d}"),
            Self::Text(s) => format!("t\t{}", escape(s)),
            Self::Date(d) => format!("dt\t{d}"),
            Self::DateTime(t) => format!("ts\t{t}"),
        }
    }
}

/// The text of a record key, `entity:id`, for a guard or a canonical form.
fn record_text(node: &NodeRef) -> String {
    format!("{}:{}", node.entity.as_str(), node.id)
}

/// Text with tabs and newlines escaped, so a canonical record stays one line.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

impl fmt::Display for Datum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Record(node) => f.write_str(&record_text(node)),
            Self::Null => f.write_str("null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Int(n) => write!(f, "{n}"),
            Self::Decimal(d) => write!(f, "{d}"),
            Self::Text(s) => write!(f, "{s}"),
            Self::Date(d) => write!(f, "{d}"),
            Self::DateTime(t) => write!(f, "{t}"),
        }
    }
}
