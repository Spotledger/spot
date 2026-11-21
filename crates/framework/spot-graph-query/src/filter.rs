//! Node and edge filters: the storage query IR's boolean expressions, evaluated over the values
//! of one record.
//!
//! **Filters see only what the caller may read.** The values a filter is evaluated on are the
//! caller's view of the record (see `exec.rs`): a hidden field is absent, so it reads as null; a
//! masked field reads as its masked text; a readable field reads as its stored value. The outcome
//! of a filter therefore never depends on a value the caller cannot see, and a filter cannot be
//! used to search a hidden value.
//!
//! The subset is fixed: field references, literals, comparisons, `in`, null tests, and `and`,
//! `or`, `not`. Anything else is refused at build time with `graphq.unsupported_filter`, so no
//! filter can silently mean something other than what it says. Null follows SQL three-valued
//! logic: a comparison with a null is unknown, and only a true result keeps a record. The
//! `alias` of a field reference is ignored: a filter speaks about the record it is applied to,
//! except an alias equal to the query's edge variable, which reads the crossed edge's properties.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use spot_storage_contract::{CmpOp, Expr, Name, Value};

use crate::error::{QueryProblem, problem};

/// Refuses an expression outside the supported subset, naming the first operator found.
pub(crate) fn validate(expr: &Expr) -> Result<(), QueryProblem> {
    match expr {
        Expr::Field(_) | Expr::Value(_) => Ok(()),
        Expr::Cmp(a, _, b) => validate(a).and(validate(b)),
        Expr::And(items) | Expr::Or(items) => items.iter().try_for_each(validate),
        Expr::Not(inner) | Expr::IsNull(inner) => validate(inner),
        Expr::In(inner, _) => validate(inner),
        Expr::Text(..) => Err(unsupported("text match")),
        Expr::Arith(..) => Err(unsupported("arithmetic")),
        Expr::Agg { .. } => Err(unsupported("aggregate")),
        Expr::Coalesce(_) => Err(unsupported("coalesce")),
        Expr::Geo(..) | Expr::Range(..) | Expr::Search(_) | Expr::SearchRank(_) => {
            Err(unsupported("geo, range or search test"))
        }
    }
}

fn unsupported(operator: &str) -> QueryProblem {
    problem(
        "graphq.unsupported_filter",
        format!("a filter uses {operator}, which the graph query API cannot evaluate"),
    )
}

/// Whether the record's values keep it under `expr`.
pub(crate) fn keeps(expr: &Expr, values: &BTreeMap<Name, Value>) -> bool {
    matches!(eval(expr, &Env::of(values)), Value::Bool(true))
}

/// Like [`keeps`], with the edge variable bound: a field whose alias is `var` reads `edge`, any
/// other field reads the record's `values`.
pub(crate) fn keeps_on_edge(
    expr: &Expr,
    values: &BTreeMap<Name, Value>,
    var: &Name,
    edge: &BTreeMap<Name, Value>,
) -> bool {
    let env = Env {
        values,
        edge: Some((var, edge)),
    };
    matches!(eval(expr, &env), Value::Bool(true))
}

/// The properties `expr` reads through the edge variable `var`, in order of appearance.
pub(crate) fn edge_refs(expr: &Expr, var: &Name) -> Vec<Name> {
    fn walk(expr: &Expr, var: &Name, out: &mut Vec<Name>) {
        match expr {
            Expr::Field(f) => {
                if f.alias == *var && !out.contains(&f.field) {
                    out.push(f.field.clone());
                }
            }
            Expr::Cmp(a, _, b) => {
                walk(a, var, out);
                walk(b, var, out);
            }
            Expr::And(items) | Expr::Or(items) => items.iter().for_each(|i| walk(i, var, out)),
            Expr::Not(inner) | Expr::IsNull(inner) | Expr::In(inner, _) => walk(inner, var, out),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(expr, var, &mut out);
    out
}

/// What a field reference reads: the record's values and, when the edge variable is bound, the
/// crossed edge's properties.
struct Env<'a> {
    values: &'a BTreeMap<Name, Value>,
    edge: Option<(&'a Name, &'a BTreeMap<Name, Value>)>,
}

impl<'a> Env<'a> {
    fn of(values: &'a BTreeMap<Name, Value>) -> Self {
        Self { values, edge: None }
    }
}

/// Three-valued evaluation: `Bool` or `Null` (unknown).
fn eval(expr: &Expr, values: &Env<'_>) -> Value {
    match expr {
        Expr::Field(field) => match values.edge {
            Some((var, edge)) if field.alias == *var => edge.get(&field.field),
            _ => values.values.get(&field.field),
        }
        .cloned()
        .unwrap_or(Value::Null),
        Expr::Value(value) => value.clone(),
        Expr::Cmp(a, op, b) => compare(&eval(a, values), *op, &eval(b, values)),
        Expr::And(items) => {
            let mut unknown = false;
            for item in items {
                match eval(item, values) {
                    Value::Bool(false) => return Value::Bool(false),
                    Value::Bool(true) => {}
                    _ => unknown = true,
                }
            }
            if unknown {
                Value::Null
            } else {
                Value::Bool(true)
            }
        }
        Expr::Or(items) => {
            let mut unknown = false;
            for item in items {
                match eval(item, values) {
                    Value::Bool(true) => return Value::Bool(true),
                    Value::Bool(false) => {}
                    _ => unknown = true,
                }
            }
            if unknown {
                Value::Null
            } else {
                Value::Bool(false)
            }
        }
        Expr::Not(inner) => match eval(inner, values) {
            Value::Bool(b) => Value::Bool(!b),
            _ => Value::Null,
        },
        Expr::IsNull(inner) => Value::Bool(eval(inner, values).is_null()),
        Expr::In(inner, list) => {
            let operand = eval(inner, values);
            if operand.is_null() {
                Value::Null
            } else {
                Value::Bool(list.contains(&operand))
            }
        }
        // Refused by `validate`; unknown if one slips through.
        _ => Value::Null,
    }
}

fn compare(a: &Value, op: CmpOp, b: &Value) -> Value {
    if a.is_null() || b.is_null() {
        return Value::Null;
    }
    match op {
        CmpOp::Eq => Value::Bool(a == b),
        CmpOp::Ne => Value::Bool(a != b),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => match ordering(a, b) {
            Some(ord) => Value::Bool(match op {
                CmpOp::Lt => ord == Ordering::Less,
                CmpOp::Le => ord != Ordering::Greater,
                CmpOp::Gt => ord == Ordering::Greater,
                _ => ord != Ordering::Less,
            }),
            None => Value::Null,
        },
    }
}

/// Ordering between two values of one comparable type; `None` across types or for types with
/// no order here.
fn ordering(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Decimal(x), Value::Decimal(y)) => Some(x.cmp(y)),
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::DateTime(x), Value::DateTime(y)) => Some(x.cmp(y)),
        _ => None,
    }
}
