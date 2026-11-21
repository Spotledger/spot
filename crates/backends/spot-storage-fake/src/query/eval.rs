//! Expression evaluation: three-valued logic, exact numbers, literal text matching and the
//! catalog's collation. `Value::Null` stands for SQL's unknown wherever a truth value is asked.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorBorrowed, CollatorPreferences};
use icu_locale_core::Locale;
use rust_decimal::Decimal;
use spot_storage_contract::query::{AggFunc, ArithOp, CmpOp, Expr, FieldRef, TextMatch, TextMode};
use spot_storage_contract::{
    Collation, ErrorKind, Name, RecordId, StorageError, StorageResult, Value,
};

use crate::store::Record;

/// One source's current row in a joined row: `None` for the missing side of a left join.
#[derive(Clone, Copy)]
pub(super) struct Binding<'a> {
    pub id: RecordId,
    pub record: &'a Record,
}

/// A joined row: one binding per source alias, in alias order.
pub(super) type Env<'a> = Vec<Option<Binding<'a>>>;

/// What an expression is evaluated against.
#[derive(Clone, Copy)]
pub(super) enum Scope<'e, 'a> {
    /// One joined row.
    Row(&'e Env<'a>),
    /// A group of joined rows: aggregates fold over them, fields read the first (a grouping
    /// key, so every row agrees).
    Group(&'e [&'e Env<'a>]),
}

/// Evaluates expressions for one query.
pub(super) struct Evaluator<'q> {
    aliases: Vec<&'q Name>,
    collator: CollatorBorrowed<'static>,
}

pub(super) fn invalid_query(msg: impl Into<String>) -> StorageError {
    StorageError::new(ErrorKind::InvalidQuery, msg)
}

fn invalid_value(msg: impl Into<String>) -> StorageError {
    StorageError::new(ErrorKind::InvalidValue, msg)
}

/// A collator for the tenant's collation: default options for the locale, `und` the root order.
pub(super) fn collator_for(collation: &Collation) -> StorageResult<CollatorBorrowed<'static>> {
    let locale = Locale::try_from_str(collation.as_str()).map_err(|e| {
        StorageError::new(
            ErrorKind::Internal,
            format!("collation `{}` is not a locale: {e}", collation.as_str()),
        )
    })?;
    Collator::try_new(
        CollatorPreferences::from(&locale),
        CollatorOptions::default(),
    )
    .map_err(|e| {
        StorageError::new(
            ErrorKind::Internal,
            format!("no collation data for `{}`: {e}", collation.as_str()),
        )
    })
}

impl<'q> Evaluator<'q> {
    pub fn new(aliases: Vec<&'q Name>, collator: CollatorBorrowed<'static>) -> Self {
        Self { aliases, collator }
    }

    pub fn eval(&self, expr: &Expr, scope: Scope<'_, '_>) -> StorageResult<Value> {
        match expr {
            Expr::Field(r) => Ok(self.field(r, scope)),
            Expr::Value(v) => Ok(v.clone()),
            Expr::Cmp(a, op, b) => {
                let (a, b) = (self.eval(a, scope)?, self.eval(b, scope)?);
                if a.is_null() || b.is_null() {
                    return Ok(Value::Null);
                }
                self.compare_op(&a, *op, &b)
            }
            Expr::And(parts) => self.fold_logic(parts, scope, false),
            Expr::Or(parts) => self.fold_logic(parts, scope, true),
            Expr::Not(e) => Ok(match truth(&self.eval(e, scope)?)? {
                Some(b) => Value::Bool(!b),
                None => Value::Null,
            }),
            Expr::IsNull(e) => Ok(Value::Bool(self.eval(e, scope)?.is_null())),
            Expr::In(e, values) => {
                if values.is_empty() {
                    return Ok(Value::Bool(false));
                }
                let v = self.eval(e, scope)?;
                if v.is_null() {
                    return Ok(Value::Null);
                }
                for candidate in values {
                    if self.ordering(&v, candidate)? == Ordering::Equal {
                        return Ok(Value::Bool(true));
                    }
                }
                Ok(Value::Bool(false))
            }
            Expr::Text(e, m) => match self.eval(e, scope)? {
                Value::Null => Ok(Value::Null),
                Value::Text(s) => Ok(Value::Bool(text_matches(&s, m))),
                other => Err(invalid_query(format!(
                    "text matching needs text, found {other:?}"
                ))),
            },
            Expr::Arith(a, op, b) => arith(&self.eval(a, scope)?, *op, &self.eval(b, scope)?),
            // Unreachable after `Capabilities::check_query`; kept as a refusal, not an
            // approximation, so a future capability change cannot silently evaluate them.
            Expr::Geo(..) | Expr::Range(..) | Expr::Search(_) | Expr::SearchRank(_) => {
                Err(StorageError::new(
                    ErrorKind::Unsupported,
                    "the in-memory backend does not model geo, range or search tests",
                ))
            }
            Expr::Coalesce(parts) => {
                for e in parts {
                    let v = self.eval(e, scope)?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Ok(Value::Null)
            }
            Expr::Agg {
                func,
                arg,
                distinct,
            } => match scope {
                Scope::Row(_) => Err(invalid_query("an aggregate needs grouped rows")),
                Scope::Group(rows) => self.aggregate(*func, arg.as_deref(), *distinct, rows),
            },
        }
    }

    /// Whether the expression is true; unknown and false both read as no.
    pub fn is_true(&self, expr: &Expr, scope: Scope<'_, '_>) -> StorageResult<bool> {
        Ok(truth(&self.eval(expr, scope)?)? == Some(true))
    }

    fn field(&self, r: &FieldRef, scope: Scope<'_, '_>) -> Value {
        let env = match scope {
            Scope::Row(e) => Some(e),
            Scope::Group(rows) => rows.first().copied(),
        };
        let binding = env.and_then(|e| {
            let at = self.aliases.iter().position(|a| **a == r.alias)?;
            e.get(at).copied().flatten()
        });
        binding
            .and_then(|b| b.record.value(b.id, &r.field))
            .unwrap_or(Value::Null)
    }

    /// SQL `AND` (`any_wins` false) and `OR` (`any_wins` true) over three-valued operands.
    fn fold_logic(
        &self,
        parts: &[Expr],
        scope: Scope<'_, '_>,
        any_wins: bool,
    ) -> StorageResult<Value> {
        let mut unknown = false;
        for e in parts {
            match truth(&self.eval(e, scope)?)? {
                Some(b) if b == any_wins => return Ok(Value::Bool(any_wins)),
                Some(_) => {}
                None => unknown = true,
            }
        }
        Ok(if unknown {
            Value::Null
        } else {
            Value::Bool(!any_wins)
        })
    }

    fn compare_op(&self, a: &Value, op: CmpOp, b: &Value) -> StorageResult<Value> {
        if !matches!(op, CmpOp::Eq | CmpOp::Ne)
            && (matches!(a, Value::Json(_)) || matches!(b, Value::Json(_)))
        {
            return Err(invalid_query("JSON values can only be tested for equality"));
        }
        let ord = self.ordering(a, b)?;
        Ok(Value::Bool(match op {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Ne => ord != Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Le => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Ge => ord != Ordering::Less,
        }))
    }

    fn ordering(&self, a: &Value, b: &Value) -> StorageResult<Ordering> {
        self.compare(a, b)
            .ok_or_else(|| invalid_query(format!("cannot compare {a:?} with {b:?}")))
    }

    /// Orders two non-null values of comparable types. Text follows the collation, ties broken
    /// by `str` order, so `Equal` means identical text. `None`: the types do not compare.
    pub fn compare(&self, a: &Value, b: &Value) -> Option<Ordering> {
        Some(match (a, b) {
            (Value::Int(x), Value::Int(y)) => x.cmp(y),
            (Value::Int(x), Value::Decimal(y)) => Decimal::from(*x).cmp(y),
            (Value::Decimal(x), Value::Int(y)) => x.cmp(&Decimal::from(*y)),
            (Value::Decimal(x), Value::Decimal(y)) => x.cmp(y),
            (Value::Text(x), Value::Text(y)) => self.collator.compare(x, y).then_with(|| x.cmp(y)),
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            (Value::Date(x), Value::Date(y)) => x.cmp(y),
            (Value::Time(x), Value::Time(y)) => x.cmp(y),
            (Value::DateTime(x), Value::DateTime(y)) => x.cmp(y),
            (Value::Id(x), Value::Id(y)) => x.cmp(y),
            (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
            (Value::Json(x), Value::Json(y)) => json_cmp(x, y),
            _ => return None,
        })
    }

    /// A total order for sorting and `Min`/`Max`: nulls are handled by the caller, and values
    /// of types that do not compare fall back to a fixed type order.
    pub fn sort_cmp(&self, a: &Value, b: &Value) -> Ordering {
        self.compare(a, b)
            .unwrap_or_else(|| type_rank(a).cmp(&type_rank(b)))
    }

    fn aggregate(
        &self,
        func: AggFunc,
        arg: Option<&Expr>,
        distinct: bool,
        rows: &[&Env<'_>],
    ) -> StorageResult<Value> {
        let Some(arg) = arg else {
            // Only `Count` may omit its argument (checked by `Query::validate`).
            return Ok(Value::Int(count_i64(rows.len())));
        };
        let mut values = Vec::new();
        let mut seen = BTreeSet::new();
        for env in rows {
            let v = self.eval(arg, Scope::Row(env))?;
            if v.is_null() || (distinct && !seen.insert(group_key(&v))) {
                continue;
            }
            values.push(v);
        }
        match func {
            AggFunc::Count => Ok(Value::Int(count_i64(values.len()))),
            AggFunc::Sum => {
                let mut it = values.into_iter();
                let Some(first) = it.next() else {
                    return Ok(Value::Null);
                };
                it.try_fold(first, |acc, v| arith(&acc, ArithOp::Add, &v))
            }
            AggFunc::Min | AggFunc::Max => {
                let mut best: Option<Value> = None;
                for v in values {
                    let better = match &best {
                        None => true,
                        Some(b) => {
                            let o = self.ordering(&v, b)?;
                            if func == AggFunc::Min {
                                o == Ordering::Less
                            } else {
                                o == Ordering::Greater
                            }
                        }
                    };
                    if better {
                        best = Some(v);
                    }
                }
                Ok(best.unwrap_or(Value::Null))
            }
        }
    }
}

fn count_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The truth value of an operand: `None` is unknown.
fn truth(v: &Value) -> StorageResult<Option<bool>> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(invalid_query(format!(
            "a condition must be true, false or null, found {other:?}"
        ))),
    }
}

/// Literal matching: `%`, `_` and `\` are ordinary characters.
fn text_matches(text: &str, m: &TextMatch) -> bool {
    let (t, p) = if m.case_insensitive {
        (text.to_lowercase(), m.pattern.to_lowercase())
    } else {
        (text.to_owned(), m.pattern.clone())
    };
    match m.mode {
        TextMode::Equals => t == p,
        TextMode::Prefix => t.starts_with(&p),
        TextMode::Contains => t.contains(&p),
        TextMode::Suffix => t.ends_with(&p),
    }
}

/// Addition, subtraction and multiplication of `Int` and `Decimal`; null in, null out. Overflow
/// is `InvalidValue`, never a wrap.
pub(super) fn arith(a: &Value, op: ArithOp, b: &Value) -> StorageResult<Value> {
    let overflow = || invalid_value(format!("{a:?} {op:?} {b:?} overflows"));
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Int(x), Value::Int(y)) => match op {
            ArithOp::Add => x.checked_add(*y),
            ArithOp::Sub => x.checked_sub(*y),
            ArithOp::Mul => x.checked_mul(*y),
        }
        .map(Value::Int)
        .ok_or_else(overflow),
        _ => {
            let (x, y) = (decimal_of(a)?, decimal_of(b)?);
            match op {
                ArithOp::Add => x.checked_add(y),
                ArithOp::Sub => x.checked_sub(y),
                ArithOp::Mul => x.checked_mul(y),
            }
            .map(Value::Decimal)
            .ok_or_else(overflow)
        }
    }
}

fn decimal_of(v: &Value) -> StorageResult<Decimal> {
    match v {
        Value::Int(i) => Ok(Decimal::from(*i)),
        Value::Decimal(d) => Ok(*d),
        other => Err(invalid_query(format!(
            "arithmetic needs numbers, found {other:?}"
        ))),
    }
}

/// Equal JSON documents compare equal whatever their formatting; others order by text.
fn json_cmp(a: &str, b: &str) -> Ordering {
    match (
        serde_json::from_str::<serde_json::Value>(a),
        serde_json::from_str::<serde_json::Value>(b),
    ) {
        (Ok(x), Ok(y)) if x == y => Ordering::Equal,
        _ => a.cmp(b),
    }
}

fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Decimal(_) => 2,
        Value::Text(_) => 3,
        Value::Date(_) => 4,
        Value::Time(_) => 5,
        Value::DateTime(_) => 6,
        Value::Id(_) => 7,
        Value::Json(_) => 8,
        Value::Bytes(_) => 9,
        Value::GeoPoint(_) => 10,
        Value::Range(_) => 11,
    }
}

/// A key under which equal values group: exact for text, numeric for `Int` and `Decimal`
/// (`1.0` and `1.00` group together), null equal to null.
pub(super) fn group_key(v: &Value) -> String {
    match v {
        Value::Null => "n".to_owned(),
        Value::Int(i) => format!("d{}", Decimal::from(*i).normalize()),
        Value::Decimal(d) => format!("d{}", d.normalize()),
        Value::Text(s) => format!("t{s}"),
        Value::Json(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(j) => format!("j{j}"),
            Err(_) => format!("j{s}"),
        },
        other => format!("o{other:?}"),
    }
}
