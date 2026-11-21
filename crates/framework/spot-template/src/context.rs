//! Rendering contexts: typed, float-free values handed to templates.

use std::collections::BTreeMap;
use std::sync::Arc;

use minijinja::Value;
use minijinja::value::{Enumerator, Object, ObjectRepr};
use rust_decimal::Decimal;
use time::{Date, OffsetDateTime};

use crate::error::TemplateError;
use crate::value::{DateObject, DateTimeObject, DecimalObject};

/// A map that iterates in insertion order (MiniJinja's own map values are
/// keyed by a `Value` and iterate sorted, which would break the order the
/// context was built in).
#[derive(Debug)]
struct OrderedMapObject {
    pairs: Vec<(String, Value)>,
}

impl Object for OrderedMapObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let key = key.as_str()?;
        self.pairs
            .iter()
            .find(|(k, _)| k.as_str() == key)
            .map(|(_, v)| v.clone())
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        // a map iterates its keys, in insertion order
        Enumerator::Values(
            self.pairs
                .iter()
                .map(|(k, _)| Value::from(k.clone()))
                .collect(),
        )
    }

    fn is_true(self: &Arc<Self>) -> bool {
        !self.pairs.is_empty()
    }
}

/// A value in a rendering context.
///
/// Contexts are built with [`Context::insert`] or deserialized from JSON with
/// [`Context::from_json`]. Money always arrives as [`CtxValue::Decimal`] —
/// never as a float (ADR-12) — and points in time as [`CtxValue::Date`] /
/// [`CtxValue::DateTime`], so dates keep an explicit zone and decimals keep
/// their exact scale.
#[derive(Debug, Clone, PartialEq)]
pub enum CtxValue {
    /// A signed count (money must be [`CtxValue::Decimal`]).
    Int(i64),
    /// An unsigned count above [`i64::MAX`].
    UInt(u64),
    /// A boolean flag.
    Bool(bool),
    /// Text; it auto-escapes per output kind.
    Str(String),
    /// An exact decimal amount (ADR-12).
    Decimal(Decimal),
    /// A calendar date without a time or zone.
    Date(Date),
    /// A point in time with a UTC offset.
    DateTime(OffsetDateTime),
    /// Nothing; it renders as empty text and fails lookups under strict mode.
    Null,
    /// A list of values.
    List(Vec<CtxValue>),
    /// A map of values; insertion order is kept.
    Map(Vec<(String, CtxValue)>),
}

impl From<i64> for CtxValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<u64> for CtxValue {
    fn from(value: u64) -> Self {
        Self::UInt(value)
    }
}

impl From<i32> for CtxValue {
    fn from(value: i32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<u32> for CtxValue {
    fn from(value: u32) -> Self {
        Self::Int(i64::from(value))
    }
}

impl From<bool> for CtxValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<&str> for CtxValue {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<String> for CtxValue {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<Decimal> for CtxValue {
    fn from(value: Decimal) -> Self {
        Self::Decimal(value)
    }
}

impl From<Date> for CtxValue {
    fn from(value: Date) -> Self {
        Self::Date(value)
    }
}

impl From<OffsetDateTime> for CtxValue {
    fn from(value: OffsetDateTime) -> Self {
        Self::DateTime(value)
    }
}

impl From<Vec<CtxValue>> for CtxValue {
    fn from(value: Vec<CtxValue>) -> Self {
        Self::List(value)
    }
}

impl From<Vec<(String, CtxValue)>> for CtxValue {
    fn from(value: Vec<(String, CtxValue)>) -> Self {
        Self::Map(value)
    }
}

impl From<Context> for CtxValue {
    fn from(value: Context) -> Self {
        Self::Map(value.values.into_iter().collect())
    }
}

impl CtxValue {
    fn into_value(self) -> Value {
        match self {
            Self::Int(value) => Value::from(value),
            Self::UInt(value) => Value::from(value),
            Self::Bool(value) => Value::from(value),
            Self::Str(value) => Value::from(value),
            Self::Decimal(amount) => Value::from_object(DecimalObject::new(amount)),
            Self::Date(date) => Value::from_object(DateObject::new(date)),
            Self::DateTime(datetime) => Value::from_object(DateTimeObject::new(datetime)),
            Self::Null => Value::from(()),
            Self::List(items) => Value::from(
                items
                    .into_iter()
                    .map(CtxValue::into_value)
                    .collect::<Vec<_>>(),
            ),
            Self::Map(pairs) => Value::from_dyn_object(Arc::new(OrderedMapObject {
                pairs: pairs
                    .into_iter()
                    .map(|(key, value)| (key, value.into_value()))
                    .collect(),
            })),
        }
    }
}

/// The data handed to a [`crate::TemplateEngine::render`] call.
///
/// Top-level keys are unique; a later insert replaces an earlier one. Nested
/// maps keep insertion order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    values: BTreeMap<String, CtxValue>,
}

impl Context {
    /// An empty context.
    #[must_use]
    pub fn new() -> Self {
        Self {
            values: BTreeMap::new(),
        }
    }

    /// Inserts (or replaces) a top-level value; builder style.
    #[must_use]
    pub fn insert(mut self, key: impl Into<String>, value: impl Into<CtxValue>) -> Self {
        self.values.insert(key.into(), value.into());
        self
    }

    /// Builds the engine value a template renders.
    pub(crate) fn as_value(&self) -> Value {
        let map = self
            .values
            .iter()
            .map(|(key, value)| (key.clone(), value.clone().into_value()))
            .collect::<BTreeMap<String, Value>>();
        Value::from(map)
    }

    /// Builds a context from JSON.
    ///
    /// Mapping is 1:1 and exact: integers become counts, objects become maps
    /// (keys sorted), strings stay strings. Numbers with a fraction or an
    /// exponent are rejected — JSON money must be a decimal string mapped by
    /// the caller, or inserted directly as a `Decimal`.
    ///
    /// # Errors
    /// Returns [`TemplateError::InvalidInput`] on float/exponent numbers, and
    /// when the JSON top level is not an object.
    pub fn from_json(json: serde_json::Value) -> Result<Self, TemplateError> {
        match walk_json(json)? {
            CtxValue::Map(pairs) => {
                let mut context = Context::new();
                for (key, value) in pairs {
                    context.values.insert(key, value);
                }
                Ok(context)
            }
            _ => Err(TemplateError::InvalidInput {
                message: "JSON context must be an object at the top level".to_owned(),
            }),
        }
    }
}

fn walk_json(json: serde_json::Value) -> Result<CtxValue, TemplateError> {
    match json {
        serde_json::Value::Null => Ok(CtxValue::Null),
        serde_json::Value::Bool(value) => Ok(CtxValue::Bool(value)),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(CtxValue::Int(value))
            } else if let Some(value) = number.as_u64() {
                Ok(CtxValue::UInt(value))
            } else {
                Err(TemplateError::InvalidInput {
                    message: format!(
                        "JSON number {number} has a fraction or an exponent; \
                         the platform only carries exact numbers (ADR-12)"
                    ),
                })
            }
        }
        serde_json::Value::String(value) => Ok(CtxValue::Str(value)),
        serde_json::Value::Array(items) => {
            let walked = items
                .into_iter()
                .map(walk_json)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CtxValue::List(walked))
        }
        serde_json::Value::Object(map) => {
            let walked = map
                .into_iter()
                .map(|(key, value)| walk_json(value).map(|value| (key, value)))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CtxValue::Map(walked))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_replaces_last_wins() {
        let context = Context::new().insert("a", 1).insert("a", "one");
        match context.values.get("a") {
            Some(CtxValue::Str(text)) => assert_eq!(text, "one"),
            other => panic!("expected replaced string, got {other:?}"),
        }
    }

    #[test]
    fn context_builds_engine_value() {
        let context = Context::new()
            .insert("count", 3_u32)
            .insert("name", "Acme")
            .insert("total", Decimal::new(123_450, 2));
        let value = context.as_value();
        match value.get_attr("total") {
            Ok(total) => assert_eq!(total.to_string(), "1234.50"),
            Err(error) => panic!("attr failed: {error:#}"),
        }
    }

    #[test]
    fn map_context_preserves_insertion_order() {
        let context = Context::new().insert(
            "row",
            CtxValue::Map(vec![
                ("z".to_owned(), CtxValue::from(1_u32)),
                ("a".to_owned(), CtxValue::from(2_u32)),
                ("m".to_owned(), CtxValue::from(3_u32)),
            ]),
        );
        let value = context.as_value();
        match value.get_attr("row") {
            Ok(row) => {
                let keys: Vec<String> = match row.try_iter() {
                    Ok(iter) => iter.filter_map(|k| k.as_str().map(str::to_owned)).collect(),
                    Err(error) => panic!("iterate: {error}"),
                };
                assert_eq!(keys, ["z", "a", "m"]);
                match row.get_attr("m") {
                    Ok(m) => assert_eq!(m.to_string(), "3"),
                    Err(error) => panic!("attr: {error}"),
                }
            }
            Err(error) => panic!("attr failed: {error}"),
        }
    }

    #[test]
    fn json_float_is_rejected() {
        let json = serde_json::json!({ "amount": 1.5 });
        match Context::from_json(json) {
            Err(TemplateError::InvalidInput { message }) => {
                assert!(message.contains("ADR-12"), "{message}");
            }
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn json_maps_1_to_1() {
        let json = serde_json::json!({
            "count": 42,
            "big": 18_446_744_073_709_551_615u64,
            "name": "Acme",
            "tags": ["a"],
            "meta": {"k": true},
            "price": "1.50",
            "nothing": null,
        });
        let context = match Context::from_json(json) {
            Ok(context) => context,
            Err(error) => panic!("json rejected: {error}"),
        };
        assert!(matches!(
            context.values.get("count"),
            Some(CtxValue::Int(42))
        ));
        assert!(matches!(context.values.get("big"), Some(CtxValue::UInt(_))));
        assert!(matches!(context.values.get("name"), Some(CtxValue::Str(_))));
        assert!(matches!(
            context.values.get("tags"),
            Some(CtxValue::List(_))
        ));
        assert!(matches!(context.values.get("meta"), Some(CtxValue::Map(_))));
        assert!(matches!(
            context.values.get("nothing"),
            Some(CtxValue::Null)
        ));
    }

    #[test]
    fn json_non_object_is_rejected() {
        match Context::from_json(serde_json::json!([1])) {
            Err(error) => assert!(error.to_string().contains("object"), "{error}"),
            Ok(context) => panic!("accepted non-object: {context:?}"),
        }
    }
}
