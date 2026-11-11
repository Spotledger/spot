//! Typed JSON values of the corpus, and the context a case runs in.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use rust_decimal::Decimal;
use serde::Deserialize;
use spot_expr::{Date, DateTime, MapContext, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub expr: String,
    #[serde(default)]
    pub context: BTreeMap<String, serde_json::Value>,
    pub now: String,
    pub expect: Expect,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum Expect {
    Ok {
        ok: serde_json::Value,
    },
    Error {
        error: String,
        offset: Option<usize>,
    },
}

/// A typed JSON value (`{"decimal": "12.50"}`) as a language value.
pub fn typed_to_value(v: &serde_json::Value) -> Value {
    let obj = v
        .as_object()
        .unwrap_or_else(|| panic!("typed value expected: {v}"));
    assert_eq!(obj.len(), 1, "a typed value has exactly one key: {v}");
    let (kind, inner) = obj.iter().next().unwrap();
    let text = || {
        inner
            .as_str()
            .unwrap_or_else(|| panic!("text expected in {v}"))
    };
    match kind.as_str() {
        "null" => Value::Null,
        "bool" => Value::Bool(
            inner
                .as_bool()
                .unwrap_or_else(|| panic!("bool expected in {v}")),
        ),
        "int" => Value::Int(text().parse().unwrap_or_else(|_| panic!("bad int {v}"))),
        "decimal" => Value::Decimal(
            text()
                .parse::<Decimal>()
                .unwrap_or_else(|_| panic!("bad decimal {v}")),
        ),
        "text" => Value::Text(text().to_owned()),
        "date" => Value::Date(Date::parse(text()).unwrap_or_else(|| panic!("bad date {v}"))),
        "datetime" => {
            Value::DateTime(DateTime::parse(text()).unwrap_or_else(|| panic!("bad datetime {v}")))
        }
        "list" => Value::List(
            inner
                .as_array()
                .unwrap_or_else(|| panic!("list expected in {v}"))
                .iter()
                .map(typed_to_value)
                .collect(),
        ),
        other => panic!("unknown type tag `{other}`"),
    }
}

pub fn context_of(case: &Case) -> MapContext {
    let now = DateTime::parse(&case.now).unwrap_or_else(|| panic!("{}: bad now", case.id));
    case.context
        .iter()
        .fold(MapContext::new(now), |ctx, (path, v)| {
            ctx.with(path, typed_to_value(v))
        })
}

/// A short rendering of a value with its type, for failure messages.
pub fn describe(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("bool {b}"),
        Value::Int(n) => format!("int {n}"),
        Value::Decimal(d) => format!("decimal {d}"),
        Value::Text(s) => format!("text {s:?}"),
        Value::Date(d) => format!("date {d}"),
        Value::DateTime(d) => format!("datetime {d}"),
        Value::List(items) => format!(
            "list [{}]",
            items.iter().map(describe).collect::<Vec<_>>().join(", ")
        ),
    }
}
