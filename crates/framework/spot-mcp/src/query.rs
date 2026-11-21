//! The read-only `query` tool over the storage query IR.
//!
//! # The JSON form of the query IR
//!
//! The storage contract defines the IR as Rust types; this crate fixes the JSON encoding an
//! MCP client sends, one to one with [`spot_storage_contract::Query`]:
//!
//! ```json
//! {
//!   "doctype": "sales_invoice",
//!   "alias": "r",
//!   "select": [ { "expr": { "field": "id" }, "as": "id" } ],
//!   "filter": { "op": "eq", "lhs": { "field": "status" }, "rhs": "draft" },
//!   "group_by": [], "having": null,
//!   "order_by": [ { "expr": { "field": "id" }, "dir": "asc", "nulls": "last" } ],
//!   "limit": 50, "offset": 0,
//!   "joins": [ { "kind": "left", "entity": "customer", "alias": "c",
//!                "on": { "op": "eq", "lhs": { "field": "customer" }, "rhs": { "field": "id" } } } ]
//! }
//! ```
//!
//! An expression is one of: `{"field": "<name>"}`, `{"value": <scalar>}`,
//! `{"op": "eq|ne|lt|le|gt|ge", "lhs": <expr>, "rhs": <expr>}`,
//! `{"and": [<expr>…]}`, `{"or": [<expr>…]}`, `{"not": <expr>}`, `{"is_null": <expr>}`,
//! `{"in": <expr>, "values": [<scalar>…]}`,
//! `{"text": <expr>, "mode": "equals|prefix|contains|suffix", "pattern": "<literal>",
//!   "case_insensitive": false}`,
//! `{"agg": "count|sum|min|max", "arg": <expr>?, "distinct": false}`,
//! `{"arith": "add|sub|mul", "lhs": <expr>, "rhs": <expr>}` and `{"coalesce": [<expr>…]}`.
//!
//! A literal that is compared with a field is decoded with that field's type (so a link is a
//! record id and a decimal is an exact decimal string); every other literal is inferred.
//! Floats are refused: money, quantities and time are never binary floating point (ADR-12).
//!
//! The query is compiled through [`spot_perm_query`]: the principal's read rights are ANDed in
//! as one filter, a filter, sort, group or aggregate on a field the principal may not read
//! unmasked is refused with `permission.field_denied`, and the select list is reduced to the
//! columns the principal may see (masked ones fetched, then masked).

use std::sync::Arc;

use serde_json::Value as Json;
use spot_api_services::query::QueryService;
use spot_app_api::context::Invocation;
use spot_app_api::meta::{DocTypeDef, FieldDef};
use spot_app_api::prelude::{Name, Value};
use spot_perm_query::{MaskedColumn, PermissionFilter, ReadFilter, ReadTarget};
use spot_permissions::{PermissionEngine, Principal, mask};
use spot_storage_contract::{
    AggFunc, ArithOp, Catalog, CmpOp, Expr, Join, JoinKind, Nulls, Order, OrderDir, Query,
    QueryPath, Rows, Selection, Source, TextMatch, TextMode,
};

use crate::error::McpError;
use crate::limits::Limits;
use crate::schema::decode_value;

/// The field that holds a document's creator, the column the owner rules compare with the
/// user (the same name the document engine uses).
pub const CREATED_BY: &str = "created_by";

/// The query seam the host injects: a site-bound, read-only query run in the caller's scope.
pub trait QueryPort: Send + Sync {
    /// Runs `query` in the invocation's scope and returns its rows.
    ///
    /// # Errors
    ///
    /// [`McpError`] when the backend fails.
    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> crate::BoxFuture<'a, Result<Rows, McpError>>;
}

/// A [`QueryPort`] over a [`QueryService`] (the same seam `spot-api-services` exposes).
pub struct ServiceQuery<S: QueryService> {
    service: Arc<S>,
}

impl<S: QueryService> ServiceQuery<S> {
    /// Wraps a query service.
    pub fn new(service: Arc<S>) -> Self {
        Self { service }
    }
}

impl<S: QueryService + 'static> QueryPort for ServiceQuery<S> {
    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> crate::BoxFuture<'a, Result<Rows, McpError>> {
        Box::pin(async move {
            self.service
                .read(inv, catalog, query)
                .await
                .map_err(|e| McpError::internal(format!("the query failed: {}", e.message)))
        })
    }
}

/// A query as the principal may run it, and the columns to mask after the fetch.
#[derive(Clone, Debug)]
pub struct PlannedQuery {
    /// The caller's query with the read filter ANDed in and the select list restricted.
    pub query: Query,
    /// The columns to mask after the fetch.
    pub masks: Vec<MaskedColumn>,
}

/// Plans `spec` for `principal`, or `None` when no row is readable.
///
/// # Errors
///
/// [`McpError`] when the spec is malformed, exceeds a limit, or filters on a field the
/// principal may not read unmasked.
pub fn plan(
    engine: &PermissionEngine,
    principal: &Principal,
    def: &DocTypeDef,
    spec: &Json,
    limits: &Limits,
) -> Result<Option<PlannedQuery>, McpError> {
    let query = decode(def, spec, limits)?;
    let doctype = query.from.entity.as_str().to_owned();
    let owner_field = Name::new(CREATED_BY)
        .map_err(|_| McpError::internal("the creator column has an invalid name"))?;
    let target = ReadTarget {
        alias: query.from.alias.clone(),
        owner_field,
    };
    let filter =
        PermissionFilter::for_read(engine, principal, &doctype, &target).map_err(map_perm)?;
    let mut combined = match filter {
        ReadFilter::None => return Ok(None),
        ReadFilter::All => query.clone(),
        ReadFilter::Filter(expr) => {
            let mut q = query.clone();
            q.filter = Some(match q.filter.take() {
                Some(existing) => Expr::And(vec![existing, expr]),
                None => expr,
            });
            q
        }
    };
    PermissionFilter::check_query(engine, principal, &doctype, &query).map_err(map_perm)?;
    let restricted = PermissionFilter::restrict_columns(engine, principal, &doctype, &combined);
    if restricted.query.select.is_empty() {
        return Ok(None);
    }
    combined = restricted.query;
    Ok(Some(PlannedQuery {
        query: combined,
        masks: restricted.masks.columns,
    }))
}

/// An empty result with the query's own column names, without touching storage.
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

fn map_perm(e: spot_perm_query::PermQueryError) -> McpError {
    match e {
        spot_perm_query::PermQueryError::FieldNotFilterable { .. } => McpError {
            code: crate::error::INVALID_PARAMS,
            kind: spot_permissions::FIELD_DENIED,
            message: e.to_string(),
            data: None,
        },
        spot_perm_query::PermQueryError::TooManyShares { .. }
        | spot_perm_query::PermQueryError::FilterTooLarge { .. }
        | spot_perm_query::PermQueryError::QueryTooLarge => McpError::limit_exceeded(e.to_string()),
    }
}

fn decode(def: &DocTypeDef, spec: &Json, limits: &Limits) -> Result<Query, McpError> {
    let obj = spec
        .as_object()
        .ok_or_else(|| McpError::invalid_arguments("a query spec must be an object"))?;
    if let Some(doctype) = obj.get("doctype").and_then(Json::as_str)
        && doctype != def.name.as_str()
    {
        return Err(McpError::invalid_arguments(format!(
            "the query names `{doctype}` but the tool is for `{}`",
            def.name
        )));
    }
    let alias = match obj.get("alias").and_then(Json::as_str) {
        Some(alias) => name(alias)?,
        None => name("r")?,
    };
    let mut decoder = Decoder {
        def,
        alias: alias.clone(),
        depth: 0,
        nodes: 0,
        limits,
    };
    let mut query = Query::from(&def.name, &alias, QueryPath::Transactional);

    let select = obj.get("select").and_then(Json::as_array);
    match select {
        Some(items) => {
            for (i, item) in items.iter().enumerate() {
                let expr = item
                    .get("expr")
                    .ok_or_else(|| McpError::invalid_arguments("a select column needs `expr`"))?;
                let expr = decoder.expr(expr)?;
                let alias = match item.get("as").and_then(Json::as_str) {
                    Some(a) => name(a)?,
                    None => name(&format!("col_{i}"))?,
                };
                query.select.push(Selection { expr, alias });
            }
        }
        None => {
            query.select.push(Selection {
                expr: Expr::field(&alias, &name("id")?),
                alias: name("id")?,
            });
        }
    }

    query.filter = match obj.get("filter") {
        None | Some(Json::Null) => None,
        Some(expr) => Some(decoder.expr(expr)?),
    };
    if let Some(groups) = obj.get("group_by").and_then(Json::as_array) {
        for g in groups {
            query.group_by.push(decoder.expr(g)?);
        }
    }
    query.having = match obj.get("having") {
        None | Some(Json::Null) => None,
        Some(expr) => Some(decoder.expr(expr)?),
    };
    if let Some(orders) = obj.get("order_by").and_then(Json::as_array) {
        for o in orders {
            let expr =
                decoder
                    .expr(o.get("expr").ok_or_else(|| {
                        McpError::invalid_arguments("an order key needs `expr`")
                    })?)?;
            let dir = match o.get("dir").and_then(Json::as_str) {
                None | Some("asc") => OrderDir::Asc,
                Some("desc") => OrderDir::Desc,
                Some(other) => {
                    return Err(McpError::invalid_arguments(format!(
                        "unknown sort direction `{other}`"
                    )));
                }
            };
            let nulls = match o.get("nulls").and_then(Json::as_str) {
                None => match dir {
                    OrderDir::Asc => Nulls::Last,
                    OrderDir::Desc => Nulls::First,
                },
                Some("first") => Nulls::First,
                Some("last") => Nulls::Last,
                Some(other) => {
                    return Err(McpError::invalid_arguments(format!(
                        "unknown nulls placement `{other}`"
                    )));
                }
            };
            query.order_by.push(Order { expr, dir, nulls });
        }
    }
    if let Some(joins) = obj.get("joins").and_then(Json::as_array) {
        for j in joins {
            let kind = match j.get("kind").and_then(Json::as_str) {
                None | Some("inner") => JoinKind::Inner,
                Some("left") => JoinKind::Left,
                Some(other) => {
                    return Err(McpError::invalid_arguments(format!(
                        "unknown join kind `{other}`"
                    )));
                }
            };
            let entity = name(
                j.get("entity")
                    .and_then(Json::as_str)
                    .ok_or_else(|| McpError::invalid_arguments("a join needs `entity`"))?,
            )?;
            let alias = name(
                j.get("alias")
                    .and_then(Json::as_str)
                    .ok_or_else(|| McpError::invalid_arguments("a join needs `alias`"))?,
            )?;
            let on = decoder.expr(
                j.get("on")
                    .ok_or_else(|| McpError::invalid_arguments("a join needs `on`"))?,
            )?;
            query.joins.push(Join {
                kind,
                source: Source { entity, alias },
                on,
            });
        }
    }
    let requested = obj.get("limit").and_then(Json::as_u64);
    query.limit = Some(limits.clamp_rows(requested.map(|l| u32::try_from(l).unwrap_or(u32::MAX))));
    query.offset = obj.get("offset").and_then(Json::as_u64).unwrap_or(0);
    Ok(query)
}

struct Decoder<'a> {
    def: &'a DocTypeDef,
    alias: Name,
    depth: usize,
    nodes: usize,
    limits: &'a Limits,
}

impl Decoder<'_> {
    fn enter(&mut self) -> Result<(), McpError> {
        self.depth += 1;
        if self.depth > self.limits.max_depth {
            return Err(McpError::limit_exceeded("the query nests too deeply"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn node(&mut self) -> Result<(), McpError> {
        self.nodes += 1;
        if self.nodes > self.limits.max_query_nodes {
            return Err(McpError::limit_exceeded("the query has too many nodes"));
        }
        Ok(())
    }

    fn field(&self, name: &str) -> Result<Expr, McpError> {
        Ok(Expr::field(&self.alias, &name_ref(name)?))
    }

    fn expr(&mut self, json: &Json) -> Result<Expr, McpError> {
        self.enter()?;
        let result = self.expr_inner(json);
        self.leave();
        result
    }

    fn expr_inner(&mut self, json: &Json) -> Result<Expr, McpError> {
        self.node()?;
        let obj = json
            .as_object()
            .ok_or_else(|| McpError::invalid_arguments("an expression must be an object"))?;
        if let Some(field) = obj.get("field").and_then(Json::as_str) {
            return self.field(field);
        }
        if let Some(value) = obj.get("value") {
            return Ok(Expr::Value(infer_value(value)?));
        }
        if let Some(op) = obj.get("op").and_then(Json::as_str) {
            let lhs = obj
                .get("lhs")
                .ok_or_else(|| McpError::invalid_arguments("a comparison needs `lhs`"))?;
            let rhs = obj
                .get("rhs")
                .ok_or_else(|| McpError::invalid_arguments("a comparison needs `rhs`"))?;
            let lhs_expr = self.expr(lhs)?;
            let field = as_field(&lhs_expr, self.def);
            let rhs_expr = self.operand(rhs, field.as_ref())?;
            let op = cmp_op(op)?;
            return Ok(Expr::Cmp(Box::new(lhs_expr), op, Box::new(rhs_expr)));
        }
        if let Some(items) = obj.get("and").and_then(Json::as_array) {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(self.expr(item)?);
            }
            return Ok(Expr::And(out));
        }
        if let Some(items) = obj.get("or").and_then(Json::as_array) {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(self.expr(item)?);
            }
            return Ok(Expr::Or(out));
        }
        if let Some(inner) = obj.get("not") {
            return Ok(Expr::Not(Box::new(self.expr(inner)?)));
        }
        if let Some(inner) = obj.get("is_null") {
            return Ok(Expr::IsNull(Box::new(self.expr(inner)?)));
        }
        if let Some(inner) = obj.get("in") {
            let expr = self.expr(inner)?;
            let field = as_field(&expr, self.def);
            let values = obj
                .get("values")
                .and_then(Json::as_array)
                .ok_or_else(|| McpError::invalid_arguments("`in` needs `values`"))?;
            let mut decoded = Vec::with_capacity(values.len());
            for v in values {
                decoded.push(decode_literal(field.as_ref(), v)?);
            }
            return Ok(Expr::In(Box::new(expr), decoded));
        }
        if let Some(inner) = obj.get("text") {
            let expr = self.expr(inner)?;
            let mode = match obj.get("mode").and_then(Json::as_str) {
                None | Some("contains") => TextMode::Contains,
                Some("equals") => TextMode::Equals,
                Some("prefix") => TextMode::Prefix,
                Some("suffix") => TextMode::Suffix,
                Some(other) => {
                    return Err(McpError::invalid_arguments(format!(
                        "unknown text mode `{other}`"
                    )));
                }
            };
            let pattern = obj
                .get("pattern")
                .and_then(Json::as_str)
                .ok_or_else(|| McpError::invalid_arguments("`text` needs a `pattern`"))?;
            let case_insensitive = obj
                .get("case_insensitive")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            return Ok(Expr::Text(
                Box::new(expr),
                TextMatch {
                    mode,
                    pattern: pattern.to_owned(),
                    case_insensitive,
                },
            ));
        }
        if let Some(func) = obj.get("agg").and_then(Json::as_str) {
            let arg = match obj.get("arg") {
                None | Some(Json::Null) => None,
                Some(a) => Some(Box::new(self.expr(a)?)),
            };
            let distinct = obj.get("distinct").and_then(Json::as_bool).unwrap_or(false);
            return Ok(Expr::Agg {
                func: agg_func(func)?,
                arg,
                distinct,
            });
        }
        if let Some(op) = obj.get("arith").and_then(Json::as_str) {
            let lhs = self.expr(
                obj.get("lhs")
                    .ok_or_else(|| McpError::invalid_arguments("arithmetic needs `lhs`"))?,
            )?;
            let rhs = self.expr(
                obj.get("rhs")
                    .ok_or_else(|| McpError::invalid_arguments("arithmetic needs `rhs`"))?,
            )?;
            return Ok(Expr::Arith(Box::new(lhs), arith_op(op)?, Box::new(rhs)));
        }
        if let Some(items) = obj.get("coalesce").and_then(Json::as_array) {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(self.expr(item)?);
            }
            return Ok(Expr::Coalesce(out));
        }
        Err(McpError::invalid_arguments("unknown expression form"))
    }

    /// The right-hand side of a comparison: a nested expression, or a literal decoded with
    /// the left-hand field's type.
    fn operand(&mut self, json: &Json, field: Option<&FieldDef>) -> Result<Expr, McpError> {
        if is_expr(json) {
            self.expr(json)
        } else {
            Ok(Expr::Value(decode_literal(field, json)?))
        }
    }
}

fn is_expr(json: &Json) -> bool {
    let Some(obj) = json.as_object() else {
        return false;
    };
    [
        "field", "value", "op", "and", "or", "not", "is_null", "in", "text", "agg", "arith",
        "coalesce",
    ]
    .iter()
    .any(|k| obj.contains_key(*k))
}

fn as_field(expr: &Expr, def: &DocTypeDef) -> Option<FieldDef> {
    match expr {
        Expr::Field(r) => def.field(r.field.as_str()).cloned(),
        _ => None,
    }
}

fn decode_literal(field: Option<&FieldDef>, json: &Json) -> Result<Value, McpError> {
    match field {
        Some(field) => decode_value(field, json),
        None => infer_value(json),
    }
}

fn infer_value(json: &Json) -> Result<Value, McpError> {
    Ok(match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => {
            let i = n
                .as_i64()
                .ok_or_else(|| McpError::invalid_arguments("floats are not allowed"))?;
            Value::Int(i)
        }
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(_) | Json::Object(_) => Value::Json(
            serde_json::to_string(json)
                .map_err(|_| McpError::invalid_arguments("a JSON literal could not be encoded"))?,
        ),
    })
}

fn cmp_op(op: &str) -> Result<CmpOp, McpError> {
    Ok(match op {
        "eq" => CmpOp::Eq,
        "ne" => CmpOp::Ne,
        "lt" => CmpOp::Lt,
        "le" => CmpOp::Le,
        "gt" => CmpOp::Gt,
        "ge" => CmpOp::Ge,
        other => {
            return Err(McpError::invalid_arguments(format!(
                "unknown comparison `{other}`"
            )));
        }
    })
}

fn agg_func(func: &str) -> Result<AggFunc, McpError> {
    Ok(match func {
        "count" => AggFunc::Count,
        "sum" => AggFunc::Sum,
        "min" => AggFunc::Min,
        "max" => AggFunc::Max,
        other => {
            return Err(McpError::invalid_arguments(format!(
                "unknown aggregate `{other}`"
            )));
        }
    })
}

fn arith_op(op: &str) -> Result<ArithOp, McpError> {
    Ok(match op {
        "add" => ArithOp::Add,
        "sub" => ArithOp::Sub,
        "mul" => ArithOp::Mul,
        other => {
            return Err(McpError::invalid_arguments(format!(
                "unknown arithmetic `{other}`"
            )));
        }
    })
}

fn name(s: &str) -> Result<Name, McpError> {
    Name::new(s).map_err(|_| McpError::invalid_arguments(format!("`{s}` is not a valid name")))
}

fn name_ref(s: &str) -> Result<Name, McpError> {
    name(s)
}
