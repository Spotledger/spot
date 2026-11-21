//! Data mapping with the shared expression language (ADR-28).
//!
//! A [`Mapping`] is a map of a target path to a `spot-expr` expression. The expression reads the
//! node's context — the node's `input`, the Case's `case` variables and the previous nodes'
//! outputs under `nodes` — flattened under the language's `doc` root, exactly as the engine's
//! own branch and map nodes read Case variables. So a mapping over `{input, case, nodes}` writes
//! `"order_id": "doc.input.order_id"` or `"total": "doc.case.qty * doc.case.price"`.
//!
//! The language's limits apply unchanged ([`spot_expr::MAX_NODES`] and [`spot_expr::MAX_DEPTH`]).
//! A mapping that cannot be right is caught at validation time — [`Mapping::validate`] parses
//! every expression and refuses an empty target — so BPM-01's process validator can reject a
//! process before a Case ever runs it. A mapping that can only fail at run time (a field that is
//! absent, a division by zero) is reported as [`IntegrationError::MappingEvaluate`].
//!
//! [`render_template`] fills `{doc.<path>}` placeholders in a URL, header value, subject or body
//! text from the same context, so a node can name a value without writing an expression for it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;

use crate::error::IntegrationError;

/// A mapping of a target path to a shared-expression-language expression.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Mapping {
    /// The target path (dotted, creating objects) and the expression that fills it.
    pub entries: BTreeMap<String, String>,
}

impl Mapping {
    /// An empty mapping.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads a mapping from a node's config value: an object of target to expression.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] when the value is not an object of string pairs.
    pub fn from_json(node: &str, value: &Json) -> Result<Self, IntegrationError> {
        serde_json::from_value(value.clone()).map_err(|e| IntegrationError::ConfigInvalid {
            node: node.to_owned(),
            detail: format!("a mapping must be an object of target-to-expression pairs: {e}"),
        })
    }

    /// Whether the mapping has no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Checks every target and expression statically: a target is non-empty, and an expression
    /// parses within the language's limits.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::MappingInvalid`] on the first target that is empty or whose expression
    /// does not parse.
    pub fn validate(&self) -> Result<(), IntegrationError> {
        for (target, expression) in &self.entries {
            if target.trim().is_empty() {
                return Err(IntegrationError::MappingInvalid {
                    target: target.clone(),
                    detail: "a mapping target must not be empty".to_owned(),
                });
            }
            spot_expr::parse(expression).map_err(|e| IntegrationError::MappingInvalid {
                target: target.clone(),
                detail: e.to_string(),
            })?;
        }
        Ok(())
    }

    /// Evaluates every expression against `context` and returns the mapped object.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::MappingEvaluate`] on the first expression that does not evaluate.
    pub fn apply(&self, context: &Json, now: OffsetDateTime) -> Result<Json, IntegrationError> {
        let mut mapped = serde_json::Map::new();
        for (target, expression) in &self.entries {
            let value =
                spot_bpm_engine::expr::evaluate(expression, context, now).map_err(|detail| {
                    IntegrationError::MappingEvaluate {
                        target: target.clone(),
                        detail,
                    }
                })?;
            let json = spot_bpm_engine::expr::to_json(&value).unwrap_or(Json::Null);
            set_at(&mut mapped, &target.split('.').collect::<Vec<_>>(), json);
        }
        Ok(Json::Object(mapped))
    }
}

/// Writes `value` at the dotted `parts` inside `map`, creating objects on the way.
fn set_at(map: &mut serde_json::Map<String, Json>, parts: &[&str], value: Json) {
    let Some((head, rest)) = parts.split_first() else {
        return;
    };
    if rest.is_empty() {
        map.insert((*head).to_owned(), value);
        return;
    }
    let child = map
        .entry((*head).to_owned())
        .or_insert_with(|| Json::Object(serde_json::Map::new()));
    if !child.is_object() {
        *child = Json::Object(serde_json::Map::new());
    }
    if let Json::Object(child_map) = child {
        set_at(child_map, rest, value);
    }
}

/// Builds the mapping context the brief names: the node's input, the Case's variables and the
/// previous nodes' outputs, under `input`, `case` and `nodes`.
#[must_use]
pub fn mapping_context(input: &Json, case: &Json, nodes: &Json) -> Json {
    Json::Object(serde_json::Map::from_iter([
        (String::from("input"), input.clone()),
        (String::from("case"), case.clone()),
        (String::from("nodes"), nodes.clone()),
    ]))
}

/// Fills `{doc.<path>}` placeholders in `template` from `context`.
///
/// A placeholder is a shared-expression-language path under `doc`, for example
/// `{doc.input.order_id}` or `{doc.case.customer}`. A missing path is an error rather than an
/// empty string, so a URL that would be wrong is refused before it is sent.
///
/// # Errors
///
/// [`IntegrationError::TemplateInvalid`] for an unbalanced brace, an empty placeholder, a path
/// that names nothing, or a value that is an object or an array (which has no single string
/// form).
pub fn render_template(template: &str, context: &Json) -> Result<String, IntegrationError> {
    let mut leaves: BTreeMap<String, String> = BTreeMap::new();
    flatten(context, String::from("doc"), &mut leaves);
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(IntegrationError::TemplateInvalid {
                detail: String::from("a `{` has no matching `}`"),
            });
        };
        let name = after[..close].trim();
        if name.is_empty() {
            return Err(IntegrationError::TemplateInvalid {
                detail: String::from("an empty placeholder `{}` names nothing"),
            });
        }
        let key = if name.starts_with("doc.") || name.starts_with("parent.") {
            name.to_owned()
        } else {
            format!("doc.{name}")
        };
        let value = leaves
            .get(&key)
            .ok_or_else(|| IntegrationError::TemplateInvalid {
                detail: format!("`{name}` names nothing in the node's context"),
            })?;
        out.push_str(value);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Collects every leaf of `value` at `prefix` as `(dotted path, string form)`.
fn flatten(value: &Json, prefix: String, out: &mut BTreeMap<String, String>) {
    match value {
        Json::Object(map) => {
            for (key, child) in map {
                flatten(child, format!("{prefix}.{key}"), out);
            }
        }
        Json::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                flatten(child, format!("{prefix}.{index}"), out);
            }
        }
        Json::Null => {
            out.insert(prefix, String::new());
        }
        Json::Bool(flag) => {
            out.insert(prefix, flag.to_string());
        }
        Json::Number(number) => {
            out.insert(prefix, number.to_string());
        }
        Json::String(text) => {
            out.insert(prefix, text.clone());
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn a_mapping_reads_input_case_and_nodes() {
        let mapping = Mapping::from_json(
            "n1",
            &json!({
                "order_id": "doc.input.order_id",
                "total": "doc.case.qty * doc.case.price",
                "from_node": "doc.nodes.n0.status",
            }),
        )
        .unwrap();
        let context = mapping_context(
            &json!({ "order_id": "SO-1" }),
            &json!({ "qty": 3, "price": 5 }),
            &json!({ "n0": { "status": "ok" } }),
        );
        let mapped = mapping.apply(&context, now()).unwrap();
        assert_eq!(
            mapped,
            json!({ "order_id": "SO-1", "total": 15, "from_node": "ok" })
        );
    }

    #[test]
    fn a_mapping_writes_nested_targets() {
        let mapping = Mapping::from_json("n1", &json!({ "a.b.c": "doc.input.x" })).unwrap();
        let context = mapping_context(&json!({ "x": 7 }), &json!({}), &json!({}));
        let mapped = mapping.apply(&context, now()).unwrap();
        assert_eq!(mapped, json!({ "a": { "b": { "c": 7 } } }));
    }

    #[test]
    fn validation_catches_a_bad_expression_statically() {
        let mapping = Mapping::from_json("n1", &json!({ "x": "doc.==" })).unwrap();
        let error = mapping.validate().unwrap_err();
        assert_eq!(error.code(), "bpm_integration.mapping_invalid");
    }

    #[test]
    fn evaluation_errors_are_reported_per_target() {
        let mapping = Mapping::from_json("n1", &json!({ "x": "1 / 0" })).unwrap();
        let context = mapping_context(&json!({}), &json!({}), &json!({}));
        let error = mapping.apply(&context, now()).unwrap_err();
        assert_eq!(error.code(), "bpm_integration.mapping_evaluate");
    }

    #[test]
    fn templates_fill_from_the_context() {
        let context = mapping_context(
            &json!({ "order_id": "SO-1", "n": 3 }),
            &json!({ "customer": "Acme" }),
            &json!({}),
        );
        assert_eq!(
            render_template("https://api.test/orders/{input.order_id}", &context).unwrap(),
            "https://api.test/orders/SO-1"
        );
        assert_eq!(
            render_template("q={input.n}", &context).unwrap(),
            "q=3",
            "a placeholder names a path in the node's context"
        );
        assert_eq!(
            render_template("no placeholders", &context).unwrap(),
            "no placeholders"
        );
    }

    #[test]
    fn a_missing_placeholder_is_refused() {
        let context = mapping_context(&json!({}), &json!({}), &json!({}));
        let error = render_template("{input.absent}", &context).unwrap_err();
        assert_eq!(error.code(), "bpm_integration.template_invalid");
        assert!(render_template("{unclosed", &context).is_err());
    }
}
