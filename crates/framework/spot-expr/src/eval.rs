//! Evaluation (specification sections 5, 6 and 9).

use std::cmp::Ordering;

use rust_decimal::Decimal;

use crate::ast::{BinaryOp, Expr, UnaryOp};
use crate::context::Context;
use crate::decimal::{self, make, parts};
use crate::error::{EvalError, EvalResult, codes};
use crate::functions::{self, as_decimal, compare};
use crate::value::Value;

impl Expr {
    /// Evaluates the expression against a context. Pure and total: the only outside reads are
    /// the context's fields and `now`.
    pub fn eval(&self, ctx: &dyn Context) -> Result<Value, EvalError> {
        eval(self, ctx)
    }
}

fn type_error(message: impl Into<String>) -> EvalError {
    EvalError::new(codes::TYPE, message)
}

/// `==` of the language: numbers by value across `int` and `decimal`, lists elementwise,
/// everything else by type and value; `null` equals only `null`.
pub(crate) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_equal(p, q))
        }
        _ => match (as_decimal(a), as_decimal(b)) {
            (Some(x), Some(y)) => x == y,
            (None, None) => std::mem::discriminant(a) == std::mem::discriminant(b) && a == b,
            _ => false,
        },
    }
}

/// The truth of a condition operand: `null` is false; anything but a bool is a type error.
fn truth(v: &Value, what: &str) -> EvalResult<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Null => Ok(false),
        other => Err(type_error(format!(
            "{what} must be a bool, not {}",
            other.type_name()
        ))),
    }
}

fn eval(e: &Expr, ctx: &dyn Context) -> EvalResult<Value> {
    match e {
        Expr::Null => Ok(Value::Null),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Int(n) => Ok(Value::Int(*n)),
        Expr::Decimal(d) => Ok(Value::Decimal(*d)),
        Expr::Text(s) => Ok(Value::Text(s.clone())),
        Expr::List(items) => items
            .iter()
            .map(|i| eval(i, ctx))
            .collect::<EvalResult<Vec<_>>>()
            .map(Value::List),
        Expr::Path(root, path) => Ok(ctx.field(*root, path).unwrap_or(Value::Null)),
        Expr::Unary(op, inner) => {
            let v = eval(inner, ctx)?;
            match op {
                UnaryOp::Not => Ok(Value::Bool(!truth(&v, "the operand of `not`")?)),
                UnaryOp::Neg => negate(&v),
            }
        }
        Expr::Binary(op, l, r) => binary(*op, l, r, ctx),
        Expr::Ternary(c, a, b) => {
            if truth(&eval(c, ctx)?, "the condition of `?`")? {
                eval(a, ctx)
            } else {
                eval(b, ctx)
            }
        }
        Expr::Call(name, args) => {
            let values = args
                .iter()
                .map(|a| eval(a, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            functions::call(name, &values, ctx.now())
        }
    }
}

fn negate(v: &Value) -> EvalResult<Value> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Int(n) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| EvalError::new(codes::OVERFLOW, "the integer is out of range")),
        Value::Decimal(d) => {
            let (m, s) = parts(d);
            Ok(Value::Decimal(make(-m, s)?))
        }
        other => Err(type_error(format!("cannot negate {}", other.type_name()))),
    }
}

fn binary(op: BinaryOp, l: &Expr, r: &Expr, ctx: &dyn Context) -> EvalResult<Value> {
    match op {
        BinaryOp::And => {
            if !truth(&eval(l, ctx)?, "the operand of `and`")? {
                return Ok(Value::Bool(false));
            }
            Ok(Value::Bool(truth(&eval(r, ctx)?, "the operand of `and`")?))
        }
        BinaryOp::Or => {
            if truth(&eval(l, ctx)?, "the operand of `or`")? {
                return Ok(Value::Bool(true));
            }
            Ok(Value::Bool(truth(&eval(r, ctx)?, "the operand of `or`")?))
        }
        _ => {
            let a = eval(l, ctx)?;
            let b = eval(r, ctx)?;
            match op {
                BinaryOp::Eq => Ok(Value::Bool(values_equal(&a, &b))),
                BinaryOp::Ne => Ok(Value::Bool(!values_equal(&a, &b))),
                BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => ordering(op, &a, &b),
                BinaryOp::In => match &b {
                    Value::Null => Ok(Value::Bool(false)),
                    Value::List(items) => {
                        Ok(Value::Bool(items.iter().any(|i| values_equal(&a, i))))
                    }
                    other => Err(type_error(format!(
                        "the right side of `in` must be a list, not {}",
                        other.type_name()
                    ))),
                },
                _ => arithmetic(op, &a, &b),
            }
        }
    }
}

fn ordering(op: BinaryOp, a: &Value, b: &Value) -> EvalResult<Value> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Bool(false));
    }
    let ord = compare(a, b).ok_or_else(|| {
        type_error(format!(
            "cannot order {} and {}",
            a.type_name(),
            b.type_name()
        ))
    })?;
    Ok(Value::Bool(match op {
        BinaryOp::Lt => ord == Ordering::Less,
        BinaryOp::Le => ord != Ordering::Greater,
        BinaryOp::Gt => ord == Ordering::Greater,
        _ => ord != Ordering::Less,
    }))
}

fn int_overflow() -> EvalError {
    EvalError::new(codes::OVERFLOW, "the integer is out of range")
}

fn division_by_zero() -> EvalError {
    EvalError::new(codes::DIVISION_BY_ZERO, "division by zero")
}

fn arithmetic(op: BinaryOp, a: &Value, b: &Value) -> EvalResult<Value> {
    if matches!(a, Value::Null) || matches!(b, Value::Null) {
        return Ok(Value::Null);
    }
    if let (BinaryOp::Add, Value::Text(x), Value::Text(y)) = (op, a, b) {
        return Ok(Value::Text(format!("{x}{y}")));
    }
    if let (Value::Int(x), Value::Int(y)) = (a, b) {
        match op {
            BinaryOp::Add => return x.checked_add(*y).map(Value::Int).ok_or_else(int_overflow),
            BinaryOp::Sub => return x.checked_sub(*y).map(Value::Int).ok_or_else(int_overflow),
            BinaryOp::Mul => return x.checked_mul(*y).map(Value::Int).ok_or_else(int_overflow),
            BinaryOp::Rem => {
                if *y == 0 {
                    return Err(division_by_zero());
                }
                return x.checked_rem(*y).map(Value::Int).ok_or_else(int_overflow);
            }
            _ => {}
        }
    }
    let (Some(x), Some(y)) = (as_decimal(a), as_decimal(b)) else {
        return Err(type_error(format!(
            "`{}` does not apply to {} and {}",
            op.as_str(),
            a.type_name(),
            b.type_name()
        )));
    };
    let (px, py) = (parts(&x), parts(&y));
    let out: Decimal = match op {
        BinaryOp::Add => decimal::add(px, py)?,
        BinaryOp::Sub => decimal::sub(px, py)?,
        BinaryOp::Mul => decimal::mul(px, py)?,
        BinaryOp::Div | BinaryOp::Rem if py.0 == 0 => return Err(division_by_zero()),
        BinaryOp::Div => decimal::div(px, py)?,
        _ => decimal::rem(px, py)?,
    };
    Ok(Value::Decimal(out))
}
