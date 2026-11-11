//! The where-used query: which documents refer to one document, grouped and counted.
//!
//! The query is the one-hop incoming traversal of the graph derived from Link fields: every
//! reference edge that points at the target is followed once. The edge set comes from the
//! derived [`GraphModel`](spot_graph_model::GraphModel); the reference edges the model omits —
//! a link field inside a child DocType, which reports its **parent** document instead of the
//! row — are walked from the definitions. Each edge is one storage query.
//!
//! Permission filtering is the caller's: this takes a [`LinkChecks`] whose `query` the caller
//! (the engine's authorizer) has already scoped, so it never sees a document the caller may not
//! read, and it says so rather than leaking a count.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::error::AppError;
use spot_app_api::prelude::{
    Expr, Name, Nulls, Order, OrderDir, Query, QueryPath, RecordId, Selection, Value,
};
use spot_app_api::{AppResult, ErrorClass};
use spot_graph_model::{EdgeSource, GraphModel, Ownership, TargetShape};
use spot_meta::{EffectiveMeta, FieldType};

use crate::LinkChecks;
use crate::name;
use crate::validate::dynamic_target;

/// How many referencing ids a group carries by default.
pub const DEFAULT_SAMPLE: usize = 20;

/// The documents that refer to one document, grouped by referring DocType and field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhereUsed {
    /// The DocType of the document asked about.
    pub doctype: String,
    /// The document asked about.
    pub id: RecordId,
    /// The referring groups, ordered by DocType then field. Empty when nothing refers.
    pub groups: Vec<ReferrerGroup>,
    /// The total number of references across every group.
    pub total: u64,
}

/// One group of referrers: the same referring DocType and field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferrerGroup {
    /// The DocType of the referring **document** (a child row reports its parent's DocType).
    pub doctype: String,
    /// The field holding the link.
    pub field: String,
    /// How many referring documents there are.
    pub count: u64,
    /// The first `limit` referring ids, smallest first (for a cancel, read them in reverse).
    pub sample: Vec<RecordId>,
}

/// Which documents refer to `doctype`/`id`: the one-hop incoming traversal, grouped by DocType
/// and field, with counts and at most `limit` ids per group (`0` means [`DEFAULT_SAMPLE`]).
///
/// `checks` must be scoped by the caller: its query sees only documents the caller may read, so
/// neither the groups nor the total reveal a document outside the caller's reach. A child row's
/// reference is reported under its **parent** document.
///
/// # Errors
///
/// `meta.unknown_doctype` when `doctype` is not defined, or the mapped storage error if a query
/// fails.
pub async fn where_used(
    checks: &mut dyn LinkChecks,
    doctype: &str,
    id: RecordId,
    limit: usize,
) -> AppResult<WhereUsed> {
    let meta = checks.effective_meta();
    if meta.definition(doctype).is_none() {
        return Err(AppError::new(
            ErrorClass::UnknownDocType,
            "meta.unknown_doctype",
            format!("The DocType `{doctype}` does not exist."),
        ));
    }
    let limit = if limit == 0 { DEFAULT_SAMPLE } else { limit };
    let plans = referring_plans(&meta, doctype);

    let mut groups: BTreeMap<(String, String), Vec<RecordId>> = BTreeMap::new();
    for plan in &plans {
        let ids = run_plan(checks, plan, id).await?;
        if ids.is_empty() {
            continue;
        }
        groups
            .entry((plan.reporting.clone(), plan.field_name.clone()))
            .or_default()
            .extend(ids);
    }

    let mut out = Vec::with_capacity(groups.len());
    let mut total = 0u64;
    for ((reporting, field), mut ids) in groups {
        ids.sort_unstable();
        ids.dedup();
        let count = u64::try_from(ids.len()).unwrap_or(u64::MAX);
        total = total.saturating_add(count);
        ids.truncate(limit);
        out.push(ReferrerGroup {
            doctype: reporting,
            field,
            count,
            sample: ids,
        });
    }
    Ok(WhereUsed {
        doctype: doctype.to_owned(),
        id,
        groups: out,
        total,
    })
}

/// One incoming reference edge: the entity to query, the field holding the link, an optional
/// dynamic type filter, and the referring document's DocType.
struct Plan {
    entity: Name,
    field: Name,
    /// The type field to filter by the target DocType, for a dynamic link.
    type_field: Option<Name>,
    /// The DocType the dynamic type field must name (the target).
    type_value: Option<String>,
    /// The DocType of the referring **document**.
    reporting: String,
    /// The field's name, for grouping.
    field_name: String,
    /// Whether the row's `parent` is the referring document (a child row).
    parent: bool,
}

/// Every incoming reference edge to `target`, from the graph model plus the child links the
/// model omits.
fn referring_plans(meta: &EffectiveMeta, target: &str) -> Vec<Plan> {
    let model = GraphModel::derive(meta);
    let mut plans = Vec::new();
    let mut seen: BTreeSet<(String, String, bool)> = BTreeSet::new();

    // The model's reference edges: Link and DynamicLink fields of top-level DocTypes.
    for edge in model.edges.values() {
        if edge.ownership != Ownership::Ref {
            continue;
        }
        if !matches!(edge.source, EdgeSource::LinkField | EdgeSource::DynamicLink) {
            continue;
        }
        if !edge_reaches(edge, target) {
            continue;
        }
        let Some((source, field)) = edge.name.split_once('.') else {
            continue;
        };
        if source != edge.from {
            continue;
        }
        let Some(def) = meta.definition(source) else {
            continue;
        };
        if def.is_child {
            continue;
        }
        let type_field = match def.fields.iter().find(|f| f.name == field) {
            Some(f) if f.ty == FieldType::DynamicLink => {
                // A type the link may not point at can never produce this edge.
                if dynamic_target(meta, target).is_err() {
                    continue;
                }
                f.options.clone()
            }
            Some(_) => None,
            None => continue,
        };
        let is_dynamic = type_field.is_some();
        if seen.insert((source.to_owned(), field.to_owned(), false))
            && let Some(plan) = plan(
                source,
                field,
                type_field,
                source,
                false,
                is_dynamic.then(|| target.to_owned()),
            )
        {
            plans.push(plan);
        }
    }

    // Child links: the model gives children no node, so their references are walked from the
    // definitions and reported under the parent that owns the row.
    let parents = parent_map(meta);
    for def in meta.definitions() {
        if !def.is_child {
            continue;
        }
        let reporting = parents
            .get(&def.name)
            .cloned()
            .unwrap_or_else(|| def.name.clone());
        for f in &def.fields {
            if f.ty == FieldType::Link
                && f.options.as_deref() == Some(target)
                && seen.insert((def.name.clone(), f.name.clone(), true))
                && let Some(plan) = plan(&def.name, &f.name, None, &reporting, true, None)
            {
                plans.push(plan);
            }
        }
    }
    plans
}

/// Builds a plan, or `None` when a name is not a valid storage name (metadata is validated, so
/// this only skips an already-broken definition).
fn plan(
    entity: &str,
    field: &str,
    type_field: Option<String>,
    reporting: &str,
    parent: bool,
    type_value: Option<String>,
) -> Option<Plan> {
    Some(Plan {
        entity: Name::new(entity).ok()?,
        field: Name::new(field).ok()?,
        type_field: type_field.and_then(|t| Name::new(&t).ok()),
        type_value,
        reporting: reporting.to_owned(),
        field_name: field.to_owned(),
        parent,
    })
}

/// Whether an edge can reach `target`.
fn edge_reaches(edge: &spot_graph_model::EdgeType, target: &str) -> bool {
    match &edge.to {
        TargetShape::One(name) => name == target,
        TargetShape::AnyOf(set) => set.contains(target),
        TargetShape::Dynamic => true,
    }
}

/// The parent DocType of every child DocType, from the parent's table fields.
pub(crate) fn parent_map(meta: &EffectiveMeta) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for def in meta.definitions() {
        for f in &def.fields {
            if f.ty.is_table()
                && let Some(child) = &f.options
            {
                map.insert(child.clone(), def.name.clone());
            }
        }
    }
    map
}

/// Runs one incoming-edge query and returns the referring document ids, smallest first.
async fn run_plan(
    checks: &mut dyn LinkChecks,
    plan: &Plan,
    id: RecordId,
) -> AppResult<Vec<RecordId>> {
    let alias = name("referrer")?;
    let id_field = name(if plan.parent { "parent" } else { "id" })?;
    let id_expr = Expr::field(&alias, &id_field);
    let mut query = Query::from(&plan.entity, &alias, QueryPath::Transactional);
    query.select.push(Selection {
        expr: id_expr.clone(),
        alias: name("id")?,
    });
    let mut filters = vec![Expr::field(&alias, &plan.field).eq(Expr::value(Value::Id(id)))];
    if let (Some(type_field), Some(type_value)) = (&plan.type_field, &plan.type_value) {
        filters
            .push(Expr::field(&alias, type_field).eq(Expr::value(Value::Text(type_value.clone()))));
    }
    query.filter = Some(Expr::And(filters));
    query.order_by.push(Order {
        expr: id_expr,
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    });

    let rows = checks.query(&query).await?;
    let mut ids = Vec::new();
    for row in &rows.rows {
        if let Some(Value::Id(found)) = row.first() {
            ids.push(*found);
        }
    }
    Ok(ids)
}
