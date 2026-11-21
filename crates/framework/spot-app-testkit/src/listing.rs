//! `list`: the documents an actor can read, filtered and ordered.
//!
//! The kit has no HTTP and no query planner, so a list is built from what the harness knows: the
//! documents created through its actors (and `seed_json` / fixtures, which create as the
//! Administrator). Each candidate is loaded **as the actor** through the engine's read path, so a
//! document the actor may not read is left out and hidden fields are hidden, exactly as a get
//! would show them. Documents created some other way (a controller that inserts on its own, a
//! job) are not candidates.
//!
//! The query is a JSON object, every key optional:
//!
//! ```json
//! { "where": { "status": "Open" }, "order_by": "subject", "desc": false, "limit": 10 }
//! ```
//!
//! `where` is equality on scalar fields (values in the native API's encoding); an empty or `null`
//! query lists everything in creation order.

use std::cmp::Ordering;

use serde_json::Value as Json;
use spot_app_api::document::Document;
use spot_app_api::meta::DocTypeDef;
use spot_app_api::prelude::{Name, Value};

use crate::json;

/// A parsed query.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ListQuery {
    pub(crate) filters: Vec<(Name, Value)>,
    pub(crate) order_by: Option<String>,
    pub(crate) desc: bool,
    pub(crate) limit: Option<usize>,
}

/// Parses `query` against `def`.
pub(crate) fn parse(def: &DocTypeDef, query: &Json) -> Result<ListQuery, String> {
    let mut out = ListQuery::default();
    if query.is_null() {
        return Ok(out);
    }
    let object = query
        .as_object()
        .ok_or("the list query must be a JSON object")?;
    for (key, value) in object {
        match key.as_str() {
            "where" => {
                let filters = value.as_object().ok_or("`where` must be an object")?;
                for (field, wanted) in filters {
                    out.filters.push(json::field_value(def, field, wanted)?);
                }
            }
            "order_by" => {
                let field = value.as_str().ok_or("`order_by` must be a field name")?;
                if field != "id" && !def.fields.iter().any(|f| f.name.as_str() == field) {
                    return Err(format!("`{}` has no field `{field}`", def.name));
                }
                out.order_by = Some(field.to_owned());
            }
            "desc" => out.desc = value.as_bool().ok_or("`desc` must be true or false")?,
            "limit" => {
                let limit = value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or("`limit` must be a whole number")?;
                out.limit = Some(limit);
            }
            other => return Err(format!("unknown list key `{other}`")),
        }
    }
    Ok(out)
}

/// Orders two values of one field: nulls first, scalars of one kind by value, anything else equal.
fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Decimal(x), Value::Decimal(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Date(x), Value::Date(y)) => x.cmp(y),
        (Value::DateTime(x), Value::DateTime(y)) => x.cmp(y),
        (Value::Id(x), Value::Id(y)) => x.to_string().cmp(&y.to_string()),
        _ => Ordering::Equal,
    }
}

/// Filters, orders and limits `docs` (already loaded as the actor, in creation order).
pub(crate) fn apply(query: &ListQuery, docs: Vec<Document>) -> Vec<Document> {
    let mut kept: Vec<Document> = docs
        .into_iter()
        .filter(|doc| {
            query
                .filters
                .iter()
                .all(|(field, wanted)| doc.get(field.as_str()) == wanted)
        })
        .collect();
    if let Some(field) = &query.order_by {
        // A stable sort: equal keys stay in creation order.
        kept.sort_by(|a, b| {
            let ordering = if field == "id" {
                a.id().to_string().cmp(&b.id().to_string())
            } else {
                compare(a.get(field), b.get(field))
            };
            if query.desc {
                ordering.reverse()
            } else {
                ordering
            }
        });
    }
    if let Some(limit) = query.limit {
        kept.truncate(limit);
    }
    kept
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn values_order_by_kind_with_nulls_first() {
        assert_eq!(compare(&Value::Null, &Value::Int(1)), Ordering::Less);
        assert_eq!(compare(&Value::Int(2), &Value::Int(1)), Ordering::Greater);
        assert_eq!(
            compare(&Value::Text("a".into()), &Value::Text("b".into())),
            Ordering::Less
        );
        assert_eq!(
            compare(&Value::Int(1), &Value::Text("a".into())),
            Ordering::Equal
        );
    }
}
