//! Condition expressions: validated on save, evaluated on an event's after-image at intake.
//!
//! A condition is text in the shared expression language ([`spot_expr`]); it reads the
//! after-image through the `doc` root (`doc.status == "Done"`) and `now()`. An evaluation
//! error is a recorded non-delivery, never a retry (the webhook's own data is wrong).

use serde_json::Value as Json;
use spot_app_api::prelude::Decimal;
use spot_expr::{DateTime, MapContext, Value};
use std::str::FromStr;
use time::OffsetDateTime;

use crate::{Error, Result};

/// Validates a condition expression's syntax without evaluating it (used on save).
pub fn validate_condition(condition: &str) -> Result<()> {
    spot_expr::parse(condition).map_err(|e| Error::InvalidCondition {
        reason: e.to_string(),
    })?;
    Ok(())
}

/// Evaluates `condition` against `after_image`, with `now` as the expression's current time.
///
/// Returns `Ok(true)` when the condition holds, `Ok(false)` when it does not.
///
/// # Errors
///
/// An invalid expression, or an expression that evaluates to a non-boolean or fails (an
/// evaluation error is a recorded non-delivery, not a retry).
pub fn evaluate_condition(
    condition: &str,
    after_image: &Json,
    now: OffsetDateTime,
) -> Result<bool> {
    let expr = spot_expr::parse(condition).map_err(|e| Error::InvalidCondition {
        reason: e.to_string(),
    })?;
    let micros = i64::try_from(now.unix_timestamp_nanos() / 1_000).unwrap_or(0);
    let datetime =
        DateTime::from_micros(micros).ok_or_else(|| Error::ConditionEvaluationFailed {
            reason: "event time is outside the language's range".to_string(),
        })?;
    let mut ctx = MapContext::new(datetime);
    ctx = insert_json(ctx, "doc", after_image, 0);
    let value = expr
        .eval(&ctx)
        .map_err(|e| Error::ConditionEvaluationFailed {
            reason: e.to_string(),
        })?;
    match value {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        other => Err(Error::ConditionEvaluationFailed {
            reason: format!("condition returned {}, not bool", other.type_name()),
        }),
    }
}

/// The deepest nested object flattened into the context; a deeper value reads as `null`.
const MAX_DEPTH: u32 = 4;

/// Flattens `value` under `prefix` into the flat context: objects become dotted paths, arrays
/// become lists of scalars, and a non-scalar inside a list reads as `null`.
fn insert_json(ctx: MapContext, prefix: &str, value: &Json, depth: u32) -> MapContext {
    match value {
        Json::Object(map) => {
            if depth > MAX_DEPTH {
                return ctx;
            }
            let mut ctx = ctx;
            for (key, child) in map {
                ctx = insert_json(ctx, &format!("{prefix}.{key}"), child, depth + 1);
            }
            ctx
        }
        Json::Array(items) => ctx.with(
            prefix,
            Value::List(items.iter().map(|item| scalar(item, 0)).collect()),
        ),
        other => ctx.with(prefix, scalar(other, 0)),
    }
}

/// A scalar value; an object (or a nested list) reads as `null`.
fn scalar(value: &Json, depth: u32) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => number(n),
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(items) => {
            if depth > MAX_DEPTH {
                Value::Null
            } else {
                Value::List(items.iter().map(|item| scalar(item, depth + 1)).collect())
            }
        }
        Json::Object(_) => Value::Null,
    }
}

/// A JSON number as an integer or an exact decimal, parsed from its text (never a float).
fn number(n: &serde_json::Number) -> Value {
    if let Some(i) = n.as_i64() {
        return Value::Int(i);
    }
    if let Some(u) = n.as_u64()
        && let Ok(i) = i64::try_from(u)
    {
        return Value::Int(i);
    }
    match Decimal::from_str(&n.to_string()) {
        Ok(d) => Value::Decimal(d),
        Err(_) => Value::Text(n.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn validate_rejects_bad_syntax() {
        assert!(validate_condition("doc.status ==").is_err());
        assert!(validate_condition("doc.status == 'Done'").is_ok());
    }

    #[test]
    fn equality_holds_on_the_after_image() {
        let after = json!({"status": "Done", "qty": 3});
        assert!(evaluate_condition("doc.status == 'Done'", &after, at()).unwrap());
        assert!(!evaluate_condition("doc.status == 'Open'", &after, at()).unwrap());
        assert!(evaluate_condition("doc.qty > 2", &after, at()).unwrap());
    }

    #[test]
    fn missing_field_reads_as_null() {
        let after = json!({"status": "Done"});
        assert!(!evaluate_condition("doc.missing == 'x'", &after, at()).unwrap());
    }

    #[test]
    fn decimal_compares_exactly() {
        let after = json!({"amount": 1234.50});
        assert!(evaluate_condition("doc.amount == 1234.50", &after, at()).unwrap());
        assert!(evaluate_condition("doc.amount >= 1234.5", &after, at()).unwrap());
    }

    #[test]
    fn non_boolean_result_is_an_error() {
        let after = json!({"status": "Done"});
        assert!(matches!(
            evaluate_condition("doc.status", &after, at()),
            Err(Error::ConditionEvaluationFailed { .. })
        ));
    }

    /// A small matrix of the odd cases the filter rules must get right.
    #[test]
    fn condition_matrix() {
        let cases: &[(&str, Json, bool)] = &[
            ("doc.status == 'Done'", json!({"status": "Done"}), true),
            ("doc.status == 'Done'", json!({"status": "Open"}), false),
            ("doc.qty > 2 and doc.qty < 10", json!({"qty": 5}), true),
            ("doc.qty > 2 and doc.qty < 10", json!({"qty": 11}), false),
            ("doc.posted == true", json!({"posted": true}), true),
            ("doc.posted == true", json!({"posted": false}), false),
            (
                "not (doc.status == 'Cancelled')",
                json!({"status": "Open"}),
                true,
            ),
            (
                "not (doc.status == 'Cancelled')",
                json!({"status": "Cancelled"}),
                false,
            ),
            ("doc.qty in [1, 2, 3]", json!({"qty": 2}), true),
            ("doc.qty in [1, 2, 3]", json!({"qty": 4}), false),
            ("doc.customer != null", json!({"customer": "c1"}), true),
            ("doc.customer != null", json!({"customer": null}), false),
            (
                "doc.name == 'a' or doc.name == 'b'",
                json!({"name": "b"}),
                true,
            ),
            (
                "doc.name == 'a' or doc.name == 'b'",
                json!({"name": "c"}),
                false,
            ),
            ("doc.qty >= 10", json!({"qty": 10}), true),
            ("doc.qty >= 10", json!({"qty": 9}), false),
            (
                "doc.status == 'Done' and doc.qty > 0",
                json!({"status": "Done", "qty": 1}),
                true,
            ),
            (
                "doc.status == 'Done' and doc.qty > 0",
                json!({"status": "Done", "qty": 0}),
                false,
            ),
            ("doc.amount == 0.00", json!({"amount": 0}), true),
            ("doc.draft == false", json!({"draft": false}), true),
            ("doc.draft == false", json!({"draft": true}), false),
            ("doc.qty * 2 == 10", json!({"qty": 5}), true),
            ("doc.qty * 2 == 10", json!({"qty": 4}), false),
            (
                "doc.customer == 'c1' and not (doc.status == 'Cancelled')",
                json!({"customer": "c1", "status": "Open"}),
                true,
            ),
            (
                "doc.customer == 'c1' and not (doc.status == 'Cancelled')",
                json!({"customer": "c1", "status": "Cancelled"}),
                false,
            ),
            ("doc.amount > 100.00", json!({"amount": 100.01}), true),
            ("doc.amount > 100.00", json!({"amount": 100.00}), false),
        ];
        for (condition, after, expected) in cases {
            assert_eq!(
                evaluate_condition(condition, after, at()).unwrap(),
                *expected,
                "{condition} on {after}"
            );
        }
    }
}
