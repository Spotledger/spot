//! Data mappings: a source path becomes a target path through a `spot-expr` expression and a
//! type coercion. The same [`MappingSpec`] the BPM integration node (WP-BPM-06) uses to map
//! between an operation's input and a document's fields.
//!
//! Evaluation is pure: the input is the only outside value (flattened under the `doc` root) and
//! `now` is an argument. No floats are used for money or quantities: a decimal is carried as its
//! exact text.

use std::str::FromStr;

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::Value as Json;
use spot_expr::{MapContext, Value};

use crate::error::{ConnectorError, Result};

/// How a mapped value is coerced to its target type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Coercion {
    /// Keep the value as it is.
    #[default]
    None,
    /// Render the value as text.
    Text,
    /// Require a whole number.
    Int,
    /// Keep the exact decimal (carried as text in JSON).
    Decimal,
    /// Require a boolean.
    Bool,
    /// Require an instant (rendered as RFC 3339 text).
    DateTime,
}

/// One mapping rule: a target path, a source path or expression, and a coercion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappingRule {
    /// The dotted target path, e.g. `order.reference`.
    pub target: String,
    /// The dotted source path in the input, e.g. `payload.id`. Used when `expression` is empty.
    pub source: String,
    /// A `spot-expr` expression producing the value. When empty, `doc.<source>` is used.
    pub expression: String,
    /// The coercion applied to the evaluated value.
    pub coerce: Coercion,
}

/// A whole mapping: an ordered list of rules.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MappingSpec {
    /// The rules, applied in order.
    pub rules: Vec<MappingRule>,
}

/// What one rule produced when checked against a sample.
#[derive(Clone, Debug, PartialEq)]
pub struct RuleOutcome {
    /// The rule's target path.
    pub target: String,
    /// The value, when the rule succeeded.
    pub value: Option<Json>,
    /// Why it failed, when it did.
    pub error: Option<String>,
}

/// The result of [`check_mapping`].
#[derive(Clone, Debug, PartialEq)]
pub struct MappingReport {
    /// One outcome per rule, in order.
    pub outcomes: Vec<RuleOutcome>,
}

impl MappingReport {
    /// Whether every rule succeeded.
    pub fn all_ok(&self) -> bool {
        self.outcomes.iter().all(|o| o.error.is_none())
    }
}

impl MappingSpec {
    /// Applies every rule to `input`, producing one JSON object. Fails on the first bad rule.
    pub fn apply(&self, input: &Json, now_micros: i64) -> Result<Json> {
        let ctx = context_for(input, now_micros)?;
        let mut out = Json::Object(serde_json::Map::new());
        for rule in &self.rules {
            let value = evaluate(rule, &ctx)?;
            set_path(&mut out, &rule.target, value);
        }
        Ok(out)
    }
}

/// Checks `spec` against `sample` and reports every rule's outcome, without failing on the
/// first bad rule. This is the test-kit function a connector author runs before installing.
pub fn check_mapping(spec: &MappingSpec, sample: &Json, now_micros: i64) -> MappingReport {
    let ctx = context_for(sample, now_micros);
    let outcomes = spec
        .rules
        .iter()
        .map(|rule| match &ctx {
            Ok(ctx) => match evaluate(rule, ctx) {
                Ok(value) => RuleOutcome {
                    target: rule.target.clone(),
                    value: Some(value),
                    error: None,
                },
                Err(e) => RuleOutcome {
                    target: rule.target.clone(),
                    value: None,
                    error: Some(e.to_string()),
                },
            },
            Err(e) => RuleOutcome {
                target: rule.target.clone(),
                value: None,
                error: Some(e.to_string()),
            },
        })
        .collect();
    MappingReport { outcomes }
}

fn evaluate(rule: &MappingRule, ctx: &MapContext) -> Result<Json> {
    let source = if rule.expression.trim().is_empty() {
        format!("doc.{}", rule.source)
    } else {
        rule.expression.clone()
    };
    let parsed = spot_expr::parse(&source).map_err(|e| ConnectorError::Mapping {
        rule: rule.target.clone(),
        reason: format!("the expression does not parse: {e}"),
    })?;
    let value = parsed.eval(ctx).map_err(|e| ConnectorError::Mapping {
        rule: rule.target.clone(),
        reason: format!("the expression failed: {e}"),
    })?;
    let coerced = coerce(value, rule.coerce).map_err(|reason| ConnectorError::Mapping {
        rule: rule.target.clone(),
        reason,
    })?;
    Ok(coerced)
}

fn context_for(input: &Json, now_micros: i64) -> Result<MapContext> {
    let now =
        spot_expr::DateTime::from_micros(now_micros).ok_or_else(|| ConnectorError::BadInput {
            reason: "the invocation time is outside the representable range".into(),
        })?;
    let mut ctx = MapContext::new(now);
    let mut pairs = Vec::new();
    flatten(input, "doc", &mut pairs);
    for (path, value) in pairs {
        ctx = ctx.with(&path, value);
    }
    Ok(ctx)
}

fn flatten(value: &Json, path: &str, out: &mut Vec<(String, Value)>) {
    match value {
        Json::Object(map) => {
            for (key, child) in map {
                flatten(child, &format!("{path}.{key}"), out);
            }
        }
        other => {
            if let Some(v) = json_to_expr(other) {
                out.push((path.to_owned(), v));
            }
        }
    }
}

fn json_to_expr(value: &Json) -> Option<Value> {
    Some(match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Decimal(Decimal::from_str(&n.to_string()).ok()?)
            }
        }
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(items) => Value::List(items.iter().filter_map(json_to_expr).collect()),
        Json::Object(_) => return None,
    })
}

fn coerce(value: Value, coercion: Coercion) -> std::result::Result<Json, String> {
    match coercion {
        Coercion::None => Ok(expr_to_json(&value)),
        Coercion::Text => Ok(Json::String(to_text(&value))),
        Coercion::Int => match &value {
            Value::Int(n) => Ok(Json::from(*n)),
            Value::Decimal(d) => d
                .to_i64()
                .map(Json::from)
                .ok_or_else(|| "the value is not a whole number".to_owned()),
            Value::Text(s) => s
                .trim()
                .parse::<i64>()
                .map(Json::from)
                .map_err(|_| "the text is not a whole number".to_owned()),
            _ => Err("the value is not a whole number".to_owned()),
        },
        Coercion::Decimal => match &value {
            Value::Int(n) => Ok(Json::String(Decimal::from(*n).to_string())),
            Value::Decimal(d) => Ok(Json::String(d.to_string())),
            Value::Text(s) => Decimal::from_str(s.trim())
                .map(|d| Json::String(d.to_string()))
                .map_err(|_| "the text is not a decimal".to_owned()),
            _ => Err("the value is not a decimal".to_owned()),
        },
        Coercion::Bool => match &value {
            Value::Bool(b) => Ok(Json::Bool(*b)),
            Value::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(Json::Bool(true)),
                "false" | "0" => Ok(Json::Bool(false)),
                _ => Err("the text is not a boolean".to_owned()),
            },
            _ => Err("the value is not a boolean".to_owned()),
        },
        Coercion::DateTime => match &value {
            Value::DateTime(t) => Ok(Json::String(instant_to_rfc3339(t.micros()))),
            Value::Int(micros) => Ok(Json::String(instant_to_rfc3339(*micros))),
            Value::Text(s) => spot_expr::DateTime::parse(s.trim())
                .map(|t| Json::String(instant_to_rfc3339(t.micros())))
                .ok_or_else(|| "the text is not an instant".to_owned()),
            _ => Err("the value is not an instant".to_owned()),
        },
    }
}

fn expr_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::from(*n),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(s) => Json::String(s.clone()),
        Value::Date(d) => Json::String(d.to_string()),
        Value::DateTime(t) => Json::String(instant_to_rfc3339(t.micros())),
        Value::List(items) => Json::Array(items.iter().map(expr_to_json).collect()),
    }
}

fn to_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Text(s) => s.clone(),
        Value::Date(d) => d.to_string(),
        Value::DateTime(t) => instant_to_rfc3339(t.micros()),
        Value::List(_) => {
            let json = expr_to_json(value);
            json.to_string()
        }
    }
}

/// Renders microseconds since the epoch as RFC 3339 (UTC). No clock is read.
fn instant_to_rfc3339(micros: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1000)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_default()
}

fn set_path(root: &mut Json, path: &str, value: Json) {
    let mut parts = path.split('.').peekable();
    let mut current = root;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            if let Json::Object(map) = current {
                map.insert(part.to_owned(), value);
            }
            return;
        }
        if !current.is_object() {
            *current = Json::Object(serde_json::Map::new());
        }
        if let Json::Object(map) = current {
            current = map
                .entry(part.to_owned())
                .or_insert_with(|| Json::Object(serde_json::Map::new()));
        }
    }
}
