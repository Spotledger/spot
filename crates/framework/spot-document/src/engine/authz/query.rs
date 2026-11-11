//! List reads: the caller's query combined with the principal's read filter, checked for
//! leaks, and reduced to the columns it may see (WP-PLT-04.T1, ADR-070 sections 6 and 12).
//!
//! A `None` plan means the principal may read no row of the DocType: the caller answers an empty
//! page **without touching storage**.

use spot_app_api::prelude::{Name, Query, Rows, Value};
use spot_perm_query::{MaskedColumn, PermissionFilter, ReadFilter, ReadTarget};
use spot_permissions::{PermissionEngine, Principal, mask};
use spot_storage_contract::Expr;

use super::facts::CREATED_BY;

/// A query as the principal may run it, and what the caller must mask after the fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryPlan {
    /// The caller's query with the read filter ANDed in and the select list restricted.
    pub query: Query,
    /// The columns to mask after the fetch.
    pub masks: Vec<MaskedColumn>,
}

/// The plan for `query` under `principal`, or `None` when no row is readable.
///
/// # Errors
///
/// [`spot_perm_query::PermQueryError`] when the query filters, sorts, groups or aggregates on a
/// field the principal may not read unmasked, or the compiled filter is too large.
pub fn plan(
    engine: &PermissionEngine,
    principal: &Principal,
    query: &Query,
) -> Result<Option<QueryPlan>, spot_perm_query::PermQueryError> {
    let doctype = query.from.entity.as_str().to_owned();
    let Ok(owner_field) = Name::new(CREATED_BY) else {
        return Ok(None);
    };
    let target = ReadTarget {
        alias: query.from.alias.clone(),
        owner_field,
    };
    let filter = PermissionFilter::for_read(engine, principal, &doctype, &target)?;
    let Some(mut combined) = combine(query, filter) else {
        return Ok(None);
    };
    // The caller's own query is checked against the no-inference rule; the filter we add names
    // only fields the principal may read, so it is not re-checked on its own.
    PermissionFilter::check_query(engine, principal, &doctype, query)?;
    let restricted = PermissionFilter::restrict_columns(engine, principal, &doctype, &combined);
    if restricted.query.select.is_empty() {
        return Ok(None);
    }
    combined = restricted.query;
    Ok(Some(QueryPlan {
        query: combined,
        masks: restricted.masks.columns,
    }))
}

fn combine(query: &Query, filter: ReadFilter) -> Option<Query> {
    let mut combined = query.clone();
    match filter {
        ReadFilter::None => return None,
        ReadFilter::All => {}
        ReadFilter::Filter(expr) => {
            combined.filter = Some(match combined.filter.take() {
                Some(existing) => Expr::And(vec![existing, expr]),
                None => expr,
            });
        }
    }
    Some(combined)
}

/// An empty page with the query's own column names, without touching storage.
#[must_use]
pub fn empty_page(query: &Query) -> Rows {
    Rows {
        columns: query.select.iter().map(|s| s.alias.clone()).collect(),
        rows: Vec::new(),
    }
}

/// Replaces every masked column's value in the fetched rows with its masked form.
pub fn apply_masks(rows: &mut Rows, masks: &[MaskedColumn]) {
    for masked in masks {
        let Some(index) = rows.columns.iter().position(|c| c == &masked.column) else {
            continue;
        };
        for row in &mut rows.rows {
            if let Some(value) = row.get_mut(index) {
                *value = match value {
                    Value::Text(text) => Value::Text(mask(text, &masked.rule)),
                    _ => Value::Null,
                };
            }
        }
    }
}
