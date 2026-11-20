//! The where-used query: which documents refer to one document, grouped and counted.
//!
//! The query is the one-hop incoming traversal of the graph derived from Link fields: every
//! reference edge that points at the target is followed once. The edge set comes from the
//! derived [`GraphModel`](spot_graph_model::GraphModel); the reference edges the model omits —
//! a link field inside a child DocType, which reports its **parent** document instead of the
//! row — are walked from the definitions.
//!
//! # Cost model
//!
//! Each reference edge costs **two small queries, independent of how many documents refer to
//! the target**: one aggregate (`count(distinct <referring id>)`) and one sample query that
//! groups by the referring id, orders it ascending and carries `limit`, so no more than
//! `limit` ids per edge ever leave storage. An edge with no referrer costs the count query
//! only. A document referred to by 500,000 rows therefore costs two queries per edge, not
//! 500,000 ids in memory. The delete and cancel refusals run this on the hot path of every
//! delete, which is why the cost matters.
//!
//! The one exception is a group fed by **several edges** (two child tables of one parent that
//! both have a link field of the same name, or such a field next to a top-level one): two
//! edges can then refer to the same referring document, and a sum of counts would count it
//! twice. For that group only, the ids of its edges are fetched and deduplicated, as before.
//! Such groups are rare (a DocType whose child tables reuse a field name).
//!
//! # Permissions
//!
//! Permission filtering is the caller's: this takes a [`LinkChecks`] whose `query` the caller
//! (the engine's authorizer) has already scoped. The counts come from that scoped repository
//! too, so neither a count nor a sample ever reveals a document the caller may not read.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::error::AppError;
use spot_app_api::prelude::{
    AggFunc, Expr, Name, Nulls, Order, OrderDir, Query, QueryPath, RecordId, Selection, Value,
};
use spot_app_api::{AppResult, ErrorClass};
use spot_graph_model::{EdgeSource, GraphModel, Ownership, TargetShape};
use spot_meta::{EffectiveMeta, FieldType};

use crate::LinkChecks;
use crate::name;
use crate::validate::dynamic_target;

/// How many referencing ids a group carries by default.
pub const DEFAULT_SAMPLE: usize = 20;

/// The most referencing ids a group carries, whatever `limit` asks for: a larger `limit` is
/// clamped to this, not refused.
pub const MAX_SAMPLE: usize = 1_000;

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
/// and field, with counts and at most `limit` ids per group (`0` means [`DEFAULT_SAMPLE`]; more
/// than [`MAX_SAMPLE`] is clamped to it).
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
    let limit = if limit == 0 {
        DEFAULT_SAMPLE
    } else {
        limit.min(MAX_SAMPLE)
    };
    let plans = referring_plans(&meta, doctype);

    let mut by_group: BTreeMap<(String, String), Vec<&Plan>> = BTreeMap::new();
    for plan in &plans {
        by_group
            .entry((plan.reporting.clone(), plan.field_name.clone()))
            .or_default()
            .push(plan);
    }

    let mut out = Vec::with_capacity(by_group.len());
    let mut total = 0u64;
    for ((reporting, field), group_plans) in by_group {
        let (count, sample) = match group_plans.as_slice() {
            [only] => count_and_sample(checks, only, id, limit).await?,
            many => shared_group(checks, many, id, limit).await?,
        };
        if count == 0 {
            continue;
        }
        total = total.saturating_add(count);
        out.push(ReferrerGroup {
            doctype: reporting,
            field,
            count,
            sample,
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

/// The base query of one edge: the edge's entity filtered to rows that refer to `id`.
fn edge_query(plan: &Plan, id: RecordId) -> AppResult<(Query, Expr)> {
    let alias = name("referrer")?;
    let id_field = name(if plan.parent { "parent" } else { "id" })?;
    let id_expr = Expr::field(&alias, &id_field);
    let mut query = Query::from(&plan.entity, &alias, QueryPath::Transactional);
    let mut filters = vec![Expr::field(&alias, &plan.field).eq(Expr::value(Value::Id(id)))];
    if let (Some(type_field), Some(type_value)) = (&plan.type_field, &plan.type_value) {
        filters
            .push(Expr::field(&alias, type_field).eq(Expr::value(Value::Text(type_value.clone()))));
    }
    query.filter = Some(Expr::And(filters));
    Ok((query, id_expr))
}

/// One edge, the cheap way: an aggregate for the exact count, then (only when something
/// refers) a sample query that returns at most `limit` ids, smallest first.
async fn count_and_sample(
    checks: &mut dyn LinkChecks,
    plan: &Plan,
    id: RecordId,
    limit: usize,
) -> AppResult<(u64, Vec<RecordId>)> {
    let (mut count_query, id_expr) = edge_query(plan, id)?;
    // `distinct`: a child-table edge counts its parent document once however many rows refer.
    count_query.select.push(Selection {
        expr: Expr::Agg {
            func: AggFunc::Count,
            arg: Some(Box::new(id_expr.clone())),
            distinct: true,
        },
        alias: name("n")?,
    });
    let rows = checks.query(&count_query).await?;
    let count = match rows.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => u64::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    if count == 0 {
        return Ok((0, Vec::new()));
    }

    let (mut sample_query, _) = edge_query(plan, id)?;
    sample_query.select.push(Selection {
        expr: id_expr.clone(),
        alias: name("id")?,
    });
    // Group by the id to deduplicate parents of several matching rows; order by the key.
    sample_query.group_by.push(id_expr.clone());
    sample_query.order_by.push(Order {
        expr: id_expr,
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    });
    sample_query.limit = Some(u32::try_from(limit).unwrap_or(u32::MAX));
    let rows = checks.query(&sample_query).await?;
    let mut sample = Vec::new();
    for row in &rows.rows {
        if let Some(Value::Id(found)) = row.first() {
            sample.push(*found);
        }
    }
    sample.truncate(limit);
    Ok((count, sample))
}

/// A group fed by several edges, where two edges can refer to the same referring document: the
/// ids of all its edges are fetched and deduplicated, so the count stays exact. This is the
/// case that needs it: a parent with two child tables that each have a link field of the same
/// name, or such a child field next to a top-level field of that name.
async fn shared_group(
    checks: &mut dyn LinkChecks,
    plans: &[&Plan],
    id: RecordId,
    limit: usize,
) -> AppResult<(u64, Vec<RecordId>)> {
    let mut ids = Vec::new();
    for plan in plans {
        ids.extend(all_ids(checks, plan, id).await?);
    }
    ids.sort_unstable();
    ids.dedup();
    let count = u64::try_from(ids.len()).unwrap_or(u64::MAX);
    ids.truncate(limit);
    Ok((count, ids))
}

/// Every referring document id of one edge, smallest first (the shared-group path only).
async fn all_ids(
    checks: &mut dyn LinkChecks,
    plan: &Plan,
    id: RecordId,
) -> AppResult<Vec<RecordId>> {
    let (mut query, id_expr) = edge_query(plan, id)?;
    query.select.push(Selection {
        expr: id_expr.clone(),
        alias: name("id")?,
    });
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
