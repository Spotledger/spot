//! A JSON document as template data, for the preview command and for callers that hold JSON.
//!
//! Plain JSON has no exact decimal, date or time, and the template engine refuses to format an
//! amount that is not an exact decimal. So an amount, a date or a time is written as a one-key
//! object:
//!
//! - `{"$decimal": "12500.50"}` is an exact decimal (never a float);
//! - `{"$date": "2026-03-10"}` is a calendar date;
//! - `{"$datetime": "2026-03-10T14:45:00+05:00"}` is a point in time (RFC 3339).
//!
//! Every other JSON number must be an integer; one with a fraction or an exponent is refused.

use rust_decimal::Decimal;
use serde_json::Value;
use spot_template::{Context, CtxValue};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

use crate::error::RenderError;

/// Builds a template context from a JSON object.
///
/// # Errors
/// [`RenderError::InvalidContext`] when the top level is not an object, a number is not an
/// integer, or a `$decimal`, `$date` or `$datetime` value does not parse.
pub fn context_from_json(json: Value) -> Result<Context, RenderError> {
    let Value::Object(map) = json else {
        return Err(RenderError::InvalidContext {
            reason: "not_an_object",
        });
    };
    let mut context = Context::new();
    for (key, value) in map {
        context = context.insert(key, walk(value)?);
    }
    Ok(context)
}

fn invalid(reason: &'static str) -> RenderError {
    RenderError::InvalidContext { reason }
}

fn walk(value: Value) -> Result<CtxValue, RenderError> {
    match value {
        Value::Null => Ok(CtxValue::Null),
        Value::Bool(flag) => Ok(CtxValue::Bool(flag)),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Ok(CtxValue::Int(int))
            } else if let Some(uint) = number.as_u64() {
                Ok(CtxValue::UInt(uint))
            } else {
                Err(invalid("non_integer_number"))
            }
        }
        Value::String(text) => Ok(CtxValue::Str(text)),
        Value::Array(items) => items
            .into_iter()
            .map(walk)
            .collect::<Result<Vec<_>, _>>()
            .map(CtxValue::List),
        Value::Object(map) => {
            if map.len() == 1 {
                if let Some(Value::String(text)) = map.get("$decimal") {
                    return text
                        .parse::<Decimal>()
                        .map(CtxValue::Decimal)
                        .map_err(|_| invalid("bad_decimal"));
                }
                if let Some(Value::String(text)) = map.get("$date") {
                    return parse_date(text).map(CtxValue::Date);
                }
                if let Some(Value::String(text)) = map.get("$datetime") {
                    return OffsetDateTime::parse(text, &Rfc3339)
                        .map(CtxValue::DateTime)
                        .map_err(|_| invalid("bad_datetime"));
                }
            }
            let mut pairs = Vec::with_capacity(map.len());
            for (key, inner) in map {
                pairs.push((key, walk(inner)?));
            }
            Ok(CtxValue::Map(pairs))
        }
    }
}

fn parse_date(text: &str) -> Result<Date, RenderError> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    Date::parse(text, &format).map_err(|_| invalid("bad_date"))
}
