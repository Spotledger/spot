//! The function library (specification section 8): about 30 pure functions.

use std::cmp::Ordering;

use rust_decimal::Decimal;

use crate::decimal::{self, make, parts, trunc};
use crate::error::{EvalError, EvalResult, codes};
use crate::lexer::decimal_from_digits;
use crate::value::{Date, DateTime, Value};

/// How many arguments a function accepts.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Arity {
    Exact(usize),
    AtLeast(usize),
}

impl Arity {
    pub(crate) fn accepts(self, n: usize) -> bool {
        match self {
            Arity::Exact(k) => n == k,
            Arity::AtLeast(k) => n >= k,
        }
    }
}

/// The arity of a library function, or `None` if there is no such function.
pub(crate) fn arity(name: &str) -> Option<Arity> {
    Some(match name {
        "len" | "lower" | "upper" | "trim" | "is_empty" | "abs" | "int" | "decimal" | "text"
        | "date" | "datetime" | "year" | "month" | "day" => Arity::Exact(1),
        "contains" | "starts_with" | "ends_with" | "round" | "date_add" | "days_between" => {
            Arity::Exact(2)
        }
        "substr" | "replace" => Arity::Exact(3),
        "now" => Arity::Exact(0),
        "coalesce" => Arity::AtLeast(1),
        "min" | "max" => Arity::AtLeast(2),
        _ => return None,
    })
}

fn type_error(function: &str, what: &str) -> EvalError {
    EvalError::new(codes::TYPE, format!("`{function}`: {what}"))
}

/// A decimal view of a numeric value.
pub(crate) fn as_decimal(v: &Value) -> Option<Decimal> {
    match v {
        Value::Int(n) => Some(Decimal::from(*n)),
        Value::Decimal(d) => Some(*d),
        _ => None,
    }
}

/// Unicode whitespace the language trims: the lexical whitespace and U+00A0.
fn is_trim_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{a0}')
}

/// Simple (one scalar to one scalar) case mapping; characters with a longer mapping stay.
fn map_case(c: char, upper: bool) -> char {
    let mut one = if upper {
        c.to_uppercase().collect::<Vec<_>>()
    } else {
        c.to_lowercase().collect::<Vec<_>>()
    };
    if one.len() == 1 { one.remove(0) } else { c }
}

/// Canonical text of a value (the function `text`). `nested` is true inside a list.
pub(crate) fn to_text(v: &Value, nested: bool) -> String {
    match v {
        Value::Null if nested => "null".to_owned(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Text(s) if nested => crate::print::quote(s),
        Value::Text(s) => s.clone(),
        Value::Date(d) => d.to_string(),
        Value::DateTime(d) => d.to_string(),
        Value::List(items) => {
            let parts: Vec<String> = items.iter().map(|i| to_text(i, true)).collect();
            format!("[{}]", parts.join(", "))
        }
    }
}

/// Parses an optionally signed decimal literal: `[+-]? digits ( '.' digits )?`.
fn parse_decimal(text: &str) -> EvalResult<Decimal> {
    let bad = || EvalError::new(codes::BAD_NUMBER, "the text is not a decimal number");
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (int_part, frac_part) = match body.split_once('.') {
        Some((i, f)) => (i, f),
        None => (body, ""),
    };
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if int_part.is_empty() || !digits(int_part) || !digits(frac_part) {
        return Err(bad());
    }
    if body.contains('.') && frac_part.is_empty() {
        return Err(bad());
    }
    let d = decimal_from_digits(int_part, frac_part).ok_or_else(decimal::overflow)?;
    let (m, s) = parts(&d);
    make(if negative { -m } else { m }, s)
}

fn parse_int(text: &str) -> EvalResult<i64> {
    let body = text.strip_prefix('+').unwrap_or(text);
    let digits = body.strip_prefix('-').unwrap_or(body);
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(EvalError::new(
            codes::BAD_NUMBER,
            "the text is not an integer",
        ));
    }
    body.parse::<i64>()
        .map_err(|_| EvalError::new(codes::OVERFLOW, "the integer is out of range"))
}

/// The order of two comparable values (numeric, text, date, datetime), or `None`.
pub(crate) fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::DateTime(x), Value::DateTime(y)) => Some(x.cmp(y)),
        _ => match (as_decimal(a), as_decimal(b)) {
            (Some(x), Some(y)) => Some(x.cmp(&y)),
            _ => None,
        },
    }
}

fn date_of(function: &str, v: &Value) -> EvalResult<Option<Date>> {
    match v {
        Value::Null => Ok(None),
        Value::Date(d) => Ok(Some(*d)),
        Value::DateTime(t) => Ok(Some(t.date())),
        _ => Err(type_error(function, "expects a date")),
    }
}

/// Calls a library function on evaluated arguments.
pub(crate) fn call(name: &str, args: &[Value], now: DateTime) -> EvalResult<Value> {
    let arg = |i: usize| args.get(i).unwrap_or(&Value::Null);
    match name {
        "len" => match arg(0) {
            Value::Null => Ok(Value::Int(0)),
            Value::Text(s) => Ok(Value::Int(
                i64::try_from(s.chars().count()).unwrap_or(i64::MAX),
            )),
            Value::List(l) => Ok(Value::Int(i64::try_from(l.len()).unwrap_or(i64::MAX))),
            _ => Err(type_error(name, "expects text or a list")),
        },
        "lower" | "upper" | "trim" => match arg(0) {
            Value::Null => Ok(Value::Null),
            Value::Text(s) => Ok(Value::Text(match name {
                "lower" => s.chars().map(|c| map_case(c, false)).collect(),
                "upper" => s.chars().map(|c| map_case(c, true)).collect(),
                _ => s.trim_matches(is_trim_space).to_owned(),
            })),
            _ => Err(type_error(name, "expects text")),
        },
        "contains" => match (arg(0), arg(1)) {
            (Value::Null, _) => Ok(Value::Bool(false)),
            (Value::Text(s), Value::Text(p)) => Ok(Value::Bool(s.contains(p.as_str()))),
            (Value::Text(_), Value::Null) => Ok(Value::Bool(false)),
            (Value::List(l), y) => Ok(Value::Bool(
                l.iter().any(|x| crate::eval::values_equal(x, y)),
            )),
            _ => Err(type_error(
                name,
                "expects text and text, or a list and a value",
            )),
        },
        "starts_with" | "ends_with" => match (arg(0), arg(1)) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Bool(false)),
            (Value::Text(s), Value::Text(p)) => Ok(Value::Bool(if name == "starts_with" {
                s.starts_with(p.as_str())
            } else {
                s.ends_with(p.as_str())
            })),
            _ => Err(type_error(name, "expects two texts")),
        },
        "substr" => match (arg(0), arg(1), arg(2)) {
            (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
            (Value::Text(s), Value::Int(start), Value::Int(count)) => {
                let (Ok(start), Ok(count)) = (usize::try_from(*start), usize::try_from(*count))
                else {
                    return Err(type_error(name, "start and count cannot be negative"));
                };
                Ok(Value::Text(s.chars().skip(start).take(count).collect()))
            }
            _ => Err(type_error(name, "expects text, an int and an int")),
        },
        "replace" => match (arg(0), arg(1), arg(2)) {
            (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
            (Value::Text(s), Value::Text(from), Value::Text(to)) => {
                if from.is_empty() {
                    return Err(type_error(name, "the text to replace cannot be empty"));
                }
                Ok(Value::Text(s.replace(from.as_str(), to)))
            }
            _ => Err(type_error(name, "expects three texts")),
        },
        "is_empty" => Ok(Value::Bool(match arg(0) {
            Value::Null => true,
            Value::Text(s) => s.is_empty(),
            Value::List(l) => l.is_empty(),
            _ => false,
        })),
        "coalesce" => Ok(args
            .iter()
            .find(|v| !matches!(v, Value::Null))
            .cloned()
            .unwrap_or(Value::Null)),
        "round" => match (arg(0), arg(1)) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (x, Value::Int(n)) => {
                let d = as_decimal(x).ok_or_else(|| type_error(name, "expects a number"))?;
                let scale = u32::try_from(*n)
                    .ok()
                    .filter(|s| *s <= 28)
                    .ok_or_else(|| type_error(name, "the scale is from 0 to 28"))?;
                Ok(Value::Decimal(decimal::round(parts(&d), scale)?))
            }
            _ => Err(type_error(name, "expects a number and an int")),
        },
        "abs" => match arg(0) {
            Value::Null => Ok(Value::Null),
            Value::Int(n) => n
                .checked_abs()
                .map(Value::Int)
                .ok_or_else(|| EvalError::new(codes::OVERFLOW, "the integer is out of range")),
            Value::Decimal(d) => {
                let (m, s) = parts(d);
                Ok(Value::Decimal(make(m.abs(), s)?))
            }
            _ => Err(type_error(name, "expects a number")),
        },
        "min" | "max" => {
            if args.iter().any(|v| matches!(v, Value::Null)) {
                return Ok(Value::Null);
            }
            let mut best = arg(0);
            for v in args.iter().skip(1) {
                let ord = compare(v, best)
                    .ok_or_else(|| type_error(name, "expects values of one comparable kind"))?;
                let better = if name == "min" {
                    ord == Ordering::Less
                } else {
                    ord == Ordering::Greater
                };
                if better {
                    best = v;
                }
            }
            Ok(best.clone())
        }
        "int" => match arg(0) {
            Value::Null => Ok(Value::Null),
            Value::Int(n) => Ok(Value::Int(*n)),
            Value::Decimal(d) => i64::try_from(trunc(parts(d)))
                .map(Value::Int)
                .map_err(|_| EvalError::new(codes::OVERFLOW, "the integer is out of range")),
            Value::Text(s) => parse_int(s).map(Value::Int),
            _ => Err(type_error(name, "expects a number or text")),
        },
        "decimal" => match arg(0) {
            Value::Null => Ok(Value::Null),
            Value::Int(n) => Ok(Value::Decimal(Decimal::from(*n))),
            Value::Decimal(d) => Ok(Value::Decimal(*d)),
            Value::Text(s) => parse_decimal(s).map(Value::Decimal),
            _ => Err(type_error(name, "expects a number or text")),
        },
        "text" => Ok(Value::Text(to_text(arg(0), false))),
        "date" => match arg(0) {
            Value::Null => Ok(Value::Null),
            Value::Date(d) => Ok(Value::Date(*d)),
            Value::DateTime(t) => Ok(Value::Date(t.date())),
            Value::Text(s) => Date::parse(s)
                .map(Value::Date)
                .ok_or_else(|| EvalError::new(codes::BAD_DATE, "the text is not a date")),
            _ => Err(type_error(name, "expects a date, datetime or text")),
        },
        "datetime" => match arg(0) {
            Value::Null => Ok(Value::Null),
            Value::DateTime(t) => Ok(Value::DateTime(*t)),
            Value::Date(d) => Ok(Value::DateTime(DateTime::from_date(*d))),
            Value::Text(s) => DateTime::parse(s)
                .map(Value::DateTime)
                .ok_or_else(|| EvalError::new(codes::BAD_DATE, "the text is not a datetime")),
            _ => Err(type_error(name, "expects a date, datetime or text")),
        },
        "now" => Ok(Value::DateTime(now)),
        "date_add" => match (date_of(name, arg(0))?, arg(1)) {
            (None, _) | (_, Value::Null) => Ok(Value::Null),
            (Some(d), Value::Int(n)) => {
                let days = i64::from(d.days())
                    .checked_add(*n)
                    .and_then(|x| i32::try_from(x).ok());
                days.and_then(Date::from_days)
                    .map(Value::Date)
                    .ok_or_else(|| EvalError::new(codes::OVERFLOW, "the date is out of range"))
            }
            _ => Err(type_error(name, "expects a date and an int")),
        },
        "days_between" => match (date_of(name, arg(0))?, date_of(name, arg(1))?) {
            (Some(a), Some(b)) => Ok(Value::Int(i64::from(b.days()) - i64::from(a.days()))),
            _ => Ok(Value::Null),
        },
        "year" | "month" | "day" => match date_of(name, arg(0))? {
            None => Ok(Value::Null),
            Some(d) => {
                let (y, m, day) = d.ymd();
                Ok(Value::Int(match name {
                    "year" => i64::from(y),
                    "month" => i64::from(m),
                    _ => i64::from(day),
                }))
            }
        },
        _ => Err(EvalError::new(
            codes::UNKNOWN_FUNCTION,
            format!("`{name}` is not a function"),
        )),
    }
}
