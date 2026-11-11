//! Turning a reference into the typed `links.*` error a user can act on, and the rename check.
//!
//! Storage reports a `ReferenceViolation` (ADR-053) and the engine a bare "other documents link
//! to this one" (`document.linked`). This module turns either into `links.referenced`, whose
//! details carry the grouped referrers (DocType, count and sample ids per field) so the UI can
//! render a sentence and a list instead of a constraint name. The messages never contain raw
//! SQL, and the text follows the `spot-errors` explanation-key pattern.

use spot_app_api::AppResult;
use spot_app_api::error::{AppError, ErrorClass};
use spot_app_api::meta::DocTypeDef;
use spot_app_api::prelude::{
    Expr, Nulls, Order, OrderDir, Query, QueryPath, RecordId, Selection, Value,
};
use spot_storage_contract::StorageError;

use crate::LinkChecks;
use crate::codes;
use crate::name;
use crate::where_used::{WhereUsed, parent_map};

/// The typed error for a refused delete: `links.referenced`, class `Linked`, carrying the
/// grouped referrers and their count so the UI can render a sentence and a list.
pub fn explain_delete(def: &DocTypeDef, id: RecordId, used: &WhereUsed) -> AppError {
    referenced(def, id, used, false)
}

/// The typed error for a refused cancel: `links.referenced`, class `Linked`, with the referrers
/// ordered **newest first** and the rule "cancel the referencing documents first".
pub fn explain_cancel(def: &DocTypeDef, id: RecordId, used: &WhereUsed) -> AppError {
    referenced(def, id, used, true)
}

/// The typed error for a delete or cancel refused by a **bare** `ReferenceViolation`, when no
/// where-used result is at hand: `links.referenced` with the offending field named, so the
/// caller can run [`where_used`](crate::where_used) and retry for the full list.
pub fn explain_reference_violation(
    def: &DocTypeDef,
    id: RecordId,
    error: &StorageError,
) -> AppError {
    let label = label(def);
    let field = error
        .field
        .as_ref()
        .map_or_else(|| "a link".to_owned(), |f| format!("`{f}`"));
    AppError::new(
        ErrorClass::Linked,
        codes::REFERENCED,
        format!("Other documents link to this {label} through {field}, so it cannot be deleted or cancelled."),
    )
    .detail("field", field)
    .detail("doctype", def.name.as_str())
    .detail("id", id.to_string())
    .explained(format!(
        "Storage reported a reference violation (ADR-053). Run the where-used query on this \
         {label} to list the documents that link to it, then remove or repoint those links."
    ))
}

/// Builds the `links.referenced` error, newest first when `newest_first`.
fn referenced(def: &DocTypeDef, id: RecordId, used: &WhereUsed, newest_first: bool) -> AppError {
    let label = label(def);
    let what = if newest_first {
        "cancel the documents that refer to it first, newest first"
    } else {
        "remove or repoint their links first"
    };
    let message = format!(
        "{total} document(s) link to this {label}; {what}.",
        total = used.total
    );
    AppError::new(ErrorClass::Linked, codes::REFERENCED, message)
        .detail("count", used.total)
        .detail("referrers", referrers_json(used, newest_first))
        .detail("doctype", used.doctype.clone())
        .detail("id", id.to_string())
        .explained(format!(
            "The {label} is referenced by other documents, so it cannot be deleted or cancelled \
             (ADR-053, ADR-42). Open the listed documents and {what}; links are record ids, so \
             renaming the target never breaks them, only deleting or cancelling it does."
        ))
}

/// The grouped referrers as JSON for the error's details. Ids are ordered newest first when
/// `newest_first` (record ids are time-ordered UUIDv7).
pub(crate) fn referrers_json(used: &WhereUsed, newest_first: bool) -> serde_json::Value {
    let groups: Vec<serde_json::Value> = used
        .groups
        .iter()
        .map(|group| {
            let mut ids: Vec<String> = group.sample.iter().map(ToString::to_string).collect();
            if newest_first {
                ids.reverse();
            }
            serde_json::json!({
                "doctype": group.doctype,
                "field": group.field,
                "count": group.count,
                "ids": ids,
            })
        })
        .collect();
    serde_json::Value::Array(groups)
}

/// The DocType's label, falling back to its name.
fn label(def: &DocTypeDef) -> String {
    if def.label.is_empty() {
        def.name.as_str().to_owned()
    } else {
        def.label.clone()
    }
}

/// Checks that renaming `def`/`id`'s title field to `new_name` would not collide with another
/// document's name. Returns `links.name_taken` (class `Duplicate`) when another document of the
/// same DocType already holds the name; a DocType without a title field is never renamed.
///
/// # Errors
///
/// `links.name_taken`, or the mapped storage error if the lookup query fails.
pub async fn check_rename_available(
    def: &DocTypeDef,
    id: RecordId,
    new_name: &str,
    checks: &mut dyn LinkChecks,
) -> AppResult<()> {
    let Some(title_field) = def.title_field.as_ref() else {
        return Ok(());
    };
    let alias = name("renamed")?;
    let mut query = Query::from(&def.name, &alias, QueryPath::Transactional);
    query.select.push(Selection {
        expr: Expr::field(&alias, &name("id")?),
        alias: name("id")?,
    });
    query.filter =
        Some(Expr::field(&alias, title_field).eq(Expr::value(Value::Text(new_name.to_owned()))));
    query.order_by.push(Order {
        expr: Expr::field(&alias, &name("id")?),
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    });
    let rows = checks.query(&query).await?;
    let clash = rows.rows.iter().any(|row| match row.first() {
        Some(Value::Id(found)) => *found != id,
        _ => false,
    });
    if clash {
        return Err(AppError::new(
            ErrorClass::Duplicate,
            codes::NAME_TAKEN,
            format!("Another {} already uses the name `{new_name}`.", label(def)),
        )
        .detail("name", new_name));
    }
    Ok(())
}

/// The parent of `doctype` when it is a child DocType, for callers that need it.
pub fn parent_of(def: &spot_meta::EffectiveMeta, doctype: &str) -> Option<String> {
    parent_map(def).get(doctype).cloned()
}
