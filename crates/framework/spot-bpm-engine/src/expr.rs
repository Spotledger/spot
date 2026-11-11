//! The expression bridge: Case variables to the shared expression language (ADR-28).
//!
//! The engine keeps a Case's variables as canonical JSON. The expression language reads fields
//! under the `doc` root as dotted paths, so this module flattens the variables into
//! `doc.<path>` keys and evaluates an expression against them. Only JSON integers, strings,
//! booleans, null and arrays map to language values: a JSON float has no counterpart (ADR-12),
//! and an object is flattened rather than passed as one value.

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_expr::{MapContext, Value, parse};

/// Flattens `variables` into a [`MapContext`] under the `doc` root at `now`.
#[must_use]
pub fn context(variables: &Json, now: OffsetDateTime) -> MapContext {
    let mut pairs: Vec<(String, Value)> = Vec::new();
    flatten(variables, String::from("doc"), &mut pairs);
    let mut ctx = MapContext::new(datetime(now));
    for (path, value) in pairs {
        ctx = ctx.with(&path, value);
    }
    ctx
}

/// Collects every leaf of `value` at `prefix` as a `(dotted path, language value)` pair.
fn flatten(value: &Json, prefix: String, out: &mut Vec<(String, Value)>) {
    match value {
        Json::Object(map) => {
            for (key, child) in map {
                flatten(child, format!("{prefix}.{key}"), out);
            }
        }
        Json::Null => out.push((prefix, Value::Null)),
        other => {
            if let Some(mapped) = to_value(other) {
                out.push((prefix, mapped));
            }
        }
    }
}

/// The language instant for an `OffsetDateTime`.
fn datetime(at: OffsetDateTime) -> spot_expr::DateTime {
    let micros = at.unix_timestamp_nanos().div_euclid(1000);
    i64::try_from(micros)
        .ok()
        .and_then(spot_expr::DateTime::from_micros)
        .unwrap_or_else(epoch)
}

/// The Unix epoch as a language instant. `from_micros(0)` is always `Some`, so the fallback
/// branch is unreachable; it exists only so the conversion never uses `unwrap`/`expect`.
fn epoch() -> spot_expr::DateTime {
    match spot_expr::DateTime::from_micros(0) {
        Some(instant) => instant,
        None => unreachable!("the Unix epoch is always representable"),
    }
}

/// A JSON value as a language value, when it has a counterpart.
#[must_use]
pub fn to_value(value: &Json) -> Option<Value> {
    match value {
        Json::Null => Some(Value::Null),
        Json::Bool(flag) => Some(Value::Bool(*flag)),
        Json::Number(number) => number.as_i64().map(Value::Int),
        Json::String(text) => Some(Value::Text(text.clone())),
        Json::Array(items) => items
            .iter()
            .map(to_value)
            .collect::<Option<Vec<_>>>()
            .map(Value::List),
        Json::Object(_) => None,
    }
}

/// A language value as canonical JSON. A decimal becomes text (ADR-12: no floats); a date and
/// an instant become text.
#[must_use]
pub fn to_json(value: &Value) -> Option<Json> {
    Some(match value {
        Value::Null => Json::Null,
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(number) => Json::from(*number),
        Value::Decimal(decimal) => Json::String(decimal.to_string()),
        Value::Text(text) => Json::String(text.clone()),
        Value::Date(date) => {
            let (y, m, d) = date.ymd();
            Json::String(format!("{y:04}-{m:02}-{d:02}"))
        }
        Value::DateTime(instant) => Json::String(instant.micros().to_string()),
        Value::List(items) => {
            let mapped: Option<Vec<Json>> = items.iter().map(to_json).collect();
            Json::Array(mapped?)
        }
    })
}

/// Evaluates `expression` against the Case's variables.
///
/// # Errors
/// A plain-language message from the parser or the evaluator; the engine wraps it in
/// [`EngineError::ExpressionInvalid`](crate::EngineError::ExpressionInvalid).
pub fn evaluate(expression: &str, variables: &Json, now: OffsetDateTime) -> Result<Value, String> {
    let parsed = parse(expression).map_err(|error| error.to_string())?;
    let ctx = context(variables, now);
    parsed.eval(&ctx).map_err(|error| error.to_string())
}

/// Evaluates `expression` and reads it as a boolean, refusing a non-boolean result.
///
/// # Errors
/// A plain-language message when the expression does not parse, does not evaluate, or does not
/// yield a boolean.
pub fn evaluate_bool(
    expression: &str,
    variables: &Json,
    now: OffsetDateTime,
) -> Result<bool, String> {
    match evaluate(expression, variables, now)? {
        Value::Bool(flag) => Ok(flag),
        other => Err(format!(
            "expected a boolean, found a {} value",
            other.type_name()
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn evaluates_against_flattened_variables() {
        let variables = json!({ "order": { "total": 7 } });
        assert!(evaluate_bool("doc.order.total > 5", &variables, now()).unwrap());
        assert!(!evaluate_bool("doc.order.total > 9", &variables, now()).unwrap());
    }

    #[test]
    fn a_missing_field_reads_as_null() {
        let variables = json!({});
        assert!(evaluate_bool("doc.missing == null", &variables, now()).unwrap());
        assert!(!evaluate_bool("doc.missing == 1", &variables, now()).unwrap());
    }

    #[test]
    fn a_non_boolean_result_is_refused() {
        let variables = json!({});
        assert!(evaluate_bool("1 + 1", &variables, now()).is_err());
    }

    #[test]
    fn values_round_trip() {
        assert_eq!(to_value(&json!(3)), Some(Value::Int(3)));
        assert_eq!(to_json(&Value::Text("x".to_owned())), Some(json!("x")));
        assert_eq!(to_value(&json!(1.5)), None);
    }
}
