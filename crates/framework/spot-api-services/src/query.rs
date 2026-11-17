//! Validated HTTP filters to typed storage IR, with lexicographic (not offset) cursors.
//!
//! `DocumentService` has no top-level query method. This local seam reads through the engine's
//! permission plan (`Engine::query_plan`), so the row set, `total` and cursor are the caller's.
use crate::conversion::{api_error, to_api_document};
use crate::list_guard::ListFieldGuard;
use crate::meta_resources::def_meta;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value as Json, json};
use spot_app_api::ErrorClass;
use spot_app_api::context::{DocumentService, Invocation, Outcome};
use spot_app_api::prelude::RecordId;
use spot_app_api::{AppResult, Document};
use spot_document::engine::{Engine, apply_masks, empty_page};
use spot_field_guard::Channel;
use spot_http::api::codec::{decode_scalar, encode_scalar, parse_record_id};
use spot_http::api::filter::{FilterAst, Op};
use spot_http::api::meta::{DocTypeMeta, FieldType, MetaField};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, Page, QuerySpec};
use spot_meta::EffectiveMeta;
use spot_permissions::Principal;
use spot_storage_contract::{
    AggFunc, Catalog, CmpOp, Collation, Expr, Name, Nulls, Order, OrderDir, Query, QueryPath,
    ReadSource, Rows, Selection, StorageBackend, Value,
};
use std::sync::Arc;

/// Site-bound query execution, supplied alongside `DocumentService`.
/// Implementations must enforce the same site and read policy as document loads.
pub trait QueryService: Send + Sync {
    /// Execute validated IR in the invocation's scope.
    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> BoxFuture<'a, Result<Rows, ApiError>>;

    /// Loads the documents of one page, one answer per id in order, each as a single
    /// [`DocumentService::load`] would answer. The default loads them one by one; an
    /// implementation that can resolve the principal and read the rows once overrides it.
    fn load_many<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        ids: &'a [RecordId],
    ) -> BoxFuture<'a, Vec<AppResult<Outcome<Document>>>>
    where
        Self: DocumentService + Sized,
    {
        Box::pin(async move {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                out.push(self.load(inv, doctype, *id).await);
            }
            out
        })
    }
}

/// The list path of the engine, under the caller's read rights (WP-SEC-04.T15).
///
/// The query is planned through the engine's permission filter
/// ([`Engine::query_plan`]): the principal's read filter is ANDed into it, so the rows, a
/// `count(*)` over it and the keyset cursor built from the last row all concern only rows the
/// caller may read, and a row the caller may not read cannot fail the page, move the cursor or
/// change `total`. A caller who may read no row gets an empty page without a storage read, and
/// masked columns are masked after the fetch. A query on a field the caller may not use
/// unmasked is refused as an unknown field (the no-inference rule of ADR-070 section 12), the
/// same answer as a field that does not exist. With the compatibility authorizer (no
/// permission model installed) the query is unchanged, as before.
impl<B: StorageBackend> QueryService for Engine<B> {
    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> BoxFuture<'a, Result<Rows, ApiError>> {
        Box::pin(async move {
            let Some(plan) = self.query_plan(inv, query).await.map_err(plan_refusal)? else {
                return Ok(empty_page(query));
            };
            let mut rows = self
                .backend()
                .read(catalog, ReadSource::Primary, &plan.query)
                .await
                .map_err(|e| {
                    api_error(spot_app_api::error::map_storage_error(
                        &e,
                        spot_app_api::error::StorageOp::Other,
                    ))
                })?;
            apply_masks(&mut rows, &plan.masks);
            Ok(rows)
        })
    }

    fn load_many<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        ids: &'a [RecordId],
    ) -> BoxFuture<'a, Vec<AppResult<Outcome<Document>>>> {
        Box::pin(Engine::load_many(self, inv, doctype, ids))
    }
}

/// A refused plan as the API answers it: a field the caller may not use is an unknown field
/// (never a hint that it exists), anything else keeps its own mapping.
fn plan_refusal(error: spot_app_api::AppError) -> ApiError {
    if error.code == "permission.field_denied" {
        return ApiError::from_code(Code::DocumentUnknownField);
    }
    api_error(error)
}

/// Whether the caller may read at least some rows of `doctype` (WP-SEC-04.T15): the check the
/// lookup makes on the DocType of the **form** that holds the link field, before it lists
/// candidates for it. A caller who may not read the form's DocType is refused
/// (`permission.denied`); asking a lookup for a form one cannot open must not work as a way to
/// browse the target DocType.
///
/// # Errors
///
/// `permission.denied` when no row of `doctype` is readable by the caller, and the refusal
/// or internal error the engine's plan gives otherwise.
pub async fn require_readable<B: StorageBackend>(
    engine: &Engine<B>,
    inv: &Invocation,
    doctype: &str,
) -> Result<(), ApiError> {
    let alias = name("r")?;
    let mut q = Query::from(&name(doctype)?, &alias, QueryPath::Transactional);
    q.select.push(Selection {
        expr: Expr::field(&alias, &name("id")?),
        alias: name("id")?,
    });
    q.limit = Some(1);
    match engine.query_plan(inv, &q).await.map_err(plan_refusal)? {
        Some(_) => Ok(()),
        None => Err(ApiError::from_code(Code::PermissionDenied)),
    }
}

fn invalid() -> ApiError {
    ApiError::from_code(Code::ApiInvalidRequest)
}
fn name(s: &str) -> Result<Name, ApiError> {
    Name::new(s).map_err(|_| invalid())
}
fn field(meta: &DocTypeMeta, key: &Name) -> Result<MetaField, ApiError> {
    if key.as_str() == "id" {
        return Ok(MetaField {
            name: key.clone(),
            label: "ID".into(),
            field_type: FieldType::Link,
            required: true,
            read_only: true,
            scale: None,
            precision: None,
            link_to: None,
            link_type_field: None,
            child_doctype: None,
            link_field: None,
            select: None,
            max_chars: None,
            default: None,
            format: None,
            format_params: Default::default(),
            lookup: None,
            hints: Default::default(),
            has_hints: false,
            multiple: false,
        });
    }
    meta.field(key.as_str())
        .cloned()
        .ok_or_else(|| ApiError::from_code(Code::DocumentUnknownField))
}

fn filter(ast: &FilterAst, meta: &DocTypeMeta, alias: &Name) -> Result<Expr, ApiError> {
    Ok(match ast {
        FilterAst::And(xs) => Expr::And(
            xs.iter()
                .map(|x| filter(x, meta, alias))
                .collect::<Result<_, _>>()?,
        ),
        FilterAst::Or(xs) => Expr::Or(
            xs.iter()
                .map(|x| filter(x, meta, alias))
                .collect::<Result<_, _>>()?,
        ),
        FilterAst::Not(x) => Expr::Not(Box::new(filter(x, meta, alias)?)),
        FilterAst::Cmp(c) => {
            let f = field(meta, &c.field)?;
            let e = Expr::field(alias, &c.field);
            match c.op {
                Op::IsNull => Expr::IsNull(Box::new(e)),
                Op::IsNotNull => Expr::Not(Box::new(Expr::IsNull(Box::new(e)))),
                Op::In => Expr::In(
                    Box::new(e),
                    c.values
                        .iter()
                        .map(|v| decode_scalar(&v.to_json(), &f, c.field.as_str()))
                        .collect::<Result<_, _>>()?,
                ),
                op => {
                    let v = c.values.first().ok_or_else(invalid)?;
                    if let Some(mode) = op.text_mode() {
                        let Json::String(pattern) = v.to_json() else {
                            return Err(invalid());
                        };
                        Expr::Text(
                            Box::new(e),
                            spot_storage_contract::TextMatch {
                                mode,
                                pattern,
                                case_insensitive: c.case_insensitive,
                            },
                        )
                    } else {
                        e.cmp(
                            op.cmp().ok_or_else(invalid)?,
                            Expr::value(decode_scalar(&v.to_json(), &f, c.field.as_str())?),
                        )
                    }
                }
            }
        }
    })
}

/// Compile a validated query to storage IR. Always select id independently of projection and
/// append it to ordering so duplicate sort values cannot skip or duplicate rows.
pub fn compile(doctype: &DocTypeMeta, spec: &QuerySpec) -> Result<Query, ApiError> {
    if !(1..=500).contains(&spec.limit) {
        return Err(invalid());
    }
    let alias = name("r")?;
    let mut q = Query::from(&doctype.doctype, &alias, QueryPath::Transactional);
    q.select.push(Selection {
        expr: Expr::field(&alias, &name("id")?),
        alias: name("id")?,
    });
    q.filter = spec
        .filter
        .as_ref()
        .map(|f| filter(f, doctype, &alias))
        .transpose()?;
    for o in &spec.order {
        field(doctype, &o.field)?;
        q.order_by.push(Order {
            expr: Expr::field(&alias, &o.field),
            dir: o.dir,
            nulls: o.nulls.unwrap_or(match o.dir {
                OrderDir::Asc => Nulls::Last,
                OrderDir::Desc => Nulls::First,
            }),
        });
    }
    if !spec.order.iter().any(|o| o.field.as_str() == "id") {
        q.order_by.push(Order {
            expr: Expr::field(&alias, &name("id")?),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        });
    }
    for (i, o) in q.order_by.iter().enumerate() {
        q.select.push(Selection {
            expr: o.expr.clone(),
            alias: name(&format!("key_{i}"))?,
        });
    }
    q.limit = Some(spec.limit + 1);
    Ok(q)
}

fn scope(inv: &Invocation, meta: &DocTypeMeta, spec: &QuerySpec) -> String {
    let mut original = spec.clone();
    original.cursor = None;
    format!(
        "{}:{:?}:{}:{}:{original:?}",
        inv.site.as_str(),
        inv.actor,
        meta.doctype,
        meta.version
    )
}

fn order_field(meta: &DocTypeMeta, order: &Order) -> Result<MetaField, ApiError> {
    let Expr::Field(f) = &order.expr else {
        return Err(invalid());
    };
    field(meta, &f.field)
}

fn cursor_values(
    text: &str,
    binding: &str,
    meta: &DocTypeMeta,
    orders: &[Order],
) -> Result<Vec<Value>, ApiError> {
    if text.len() > 65_536 {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| invalid())?;
    let payload: Json = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if payload["scope"].as_str() != Some(binding) {
        return Err(invalid());
    }
    let values = payload["values"].as_array().ok_or_else(invalid)?;
    if values.len() != orders.len() {
        return Err(invalid());
    }
    values
        .iter()
        .zip(orders)
        .map(|(v, o)| {
            if let Expr::Field(f) = &o.expr
                && f.field.as_str() == "id"
            {
                return v
                    .as_str()
                    .and_then(parse_record_id)
                    .map(Value::Id)
                    .ok_or_else(invalid);
            }
            decode_scalar(v, &order_field(meta, o)?, "cursor")
        })
        .collect()
}

// SQL three-valued comparisons need explicit null branches for both equality prefixes and
// 'after'. For a null last key there is nothing after; null first admits every non-null key.
fn after(orders: &[Order], values: &[Value]) -> Expr {
    let mut branches = Vec::new();
    let mut equal = Vec::new();
    for (o, v) in orders.iter().zip(values) {
        let is_null = Expr::IsNull(Box::new(o.expr.clone()));
        let greater = if v.is_null() {
            match o.nulls {
                Nulls::First => Expr::Not(Box::new(is_null.clone())),
                Nulls::Last => Expr::Or(vec![]),
            }
        } else {
            let cmp = o.expr.clone().cmp(
                match o.dir {
                    OrderDir::Asc => CmpOp::Gt,
                    OrderDir::Desc => CmpOp::Lt,
                },
                Expr::value(v.clone()),
            );
            match o.nulls {
                Nulls::Last => Expr::Or(vec![cmp, is_null.clone()]),
                Nulls::First => cmp,
            }
        };
        let mut branch = equal.clone();
        branch.push(greater);
        branches.push(Expr::And(branch));
        equal.push(if v.is_null() {
            is_null
        } else {
            o.expr.clone().eq(Expr::value(v.clone()))
        });
    }
    Expr::Or(branches)
}

/// One page of loaded documents, before any outbound channel has seen them.
pub(crate) struct RawPage {
    pub(crate) documents: Vec<Document>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) total: Option<u64>,
    /// The caller, resolved when a guard was given.
    pub(crate) principal: Option<Principal>,
}

pub(crate) async fn list<S: DocumentService + QueryService>(
    service: &S,
    effective: &EffectiveMeta,
    inv: &Invocation,
    doctype: &str,
    spec: &QuerySpec,
    guard: Option<&ListFieldGuard>,
) -> Result<Page, ApiError> {
    let raw = list_documents(
        service,
        effective,
        inv,
        doctype,
        spec,
        guard.map(|g| (g, Channel::ListColumn)),
    )
    .await?;
    let mut documents = Vec::new();
    for loaded in raw.documents {
        let loaded = match (guard, &raw.principal) {
            (Some(g), Some(p)) => g.redact(Channel::ListColumn, p, doctype, loaded),
            _ => loaded,
        };
        let mut doc = to_api_document(loaded);
        if !spec.fields.is_empty() {
            doc.fields.retain(|key, _| spec.fields.contains(key));
        }
        documents.push(doc);
    }
    Ok(Page {
        documents,
        next_cursor: raw.next_cursor,
        total: raw.total,
    })
}

/// The query, its no-inference check for `channel`, and the loaded documents (each as the
/// engine's own read path lets the caller see it). The caller applies the channel's redaction.
pub(crate) async fn list_documents<S: DocumentService + QueryService>(
    service: &S,
    effective: &EffectiveMeta,
    inv: &Invocation,
    doctype: &str,
    spec: &QuerySpec,
    guard: Option<(&ListFieldGuard, Channel)>,
) -> Result<RawPage, ApiError> {
    let def = service.meta(doctype).await.map_err(api_error)?;
    let meta = def_meta(&def)?;
    let catalog = Arc::new(
        effective
            .catalog(Collation::new("und").map_err(|_| invalid())?)
            .map_err(|_| ApiError::from_code(Code::HttpInternal))?,
    );
    let mut q = compile(&meta, spec)?;
    // No-inference rule: a filter or sort on a field the caller may not report is refused as an
    // unknown field before anything is read (so `total` and the order leak nothing either).
    let principal = match guard {
        Some((g, channel)) => {
            let principal = g.principal(inv).await;
            g.check_query(channel, &principal, doctype, spec)?;
            Some(principal)
        }
        None => None,
    };
    let total = if spec.with_total {
        let mut count = q.clone();
        count.select = vec![Selection {
            expr: Expr::Agg {
                func: AggFunc::Count,
                arg: None,
                distinct: false,
            },
            alias: name("count")?,
        }];
        count.order_by.clear();
        count.limit = None;
        let rows = service.read(inv, catalog.clone(), &count).await?;
        match rows.rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Some(u64::try_from(*n).map_err(|_| invalid())?),
            // A caller who may read no row gets an empty page, not a count row: zero.
            None if rows.rows.is_empty() => Some(0),
            _ => return Err(ApiError::from_code(Code::HttpInternal)),
        }
    } else {
        None
    };
    let binding = scope(inv, &meta, spec);
    if let Some(cursor) = &spec.cursor {
        let values = cursor_values(cursor, &binding, &meta, &q.order_by)?;
        let boundary = after(&q.order_by, &values);
        q.filter = Some(match q.filter.take() {
            Some(f) => Expr::And(vec![f, boundary]),
            None => boundary,
        });
    }
    let rows = service.read(inv, catalog, &q).await?;
    let has_more = rows.rows.len() > spec.limit as usize;
    let kept = &rows.rows[..rows.rows.len().min(spec.limit as usize)];
    let next_cursor = if has_more {
        let last = kept.last().ok_or_else(invalid)?;
        let values = q
            .order_by
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let v = last.get(i + 1).ok_or_else(invalid)?;
                encode_scalar(v, &order_field(&meta, o)?)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Some(
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&json!({"scope": binding, "values": values}))
                    .map_err(|_| invalid())?,
            ),
        )
    } else {
        None
    };
    let mut ids = Vec::with_capacity(kept.len());
    for row in kept {
        let Some(Value::Id(id)) = row.first() else {
            return Err(ApiError::from_code(Code::HttpInternal));
        };
        ids.push(*id);
    }
    // One principal resolution and one read transaction for the page, not one per row.
    let loaded = service.load_many(inv, doctype, &ids).await;
    let mut documents = Vec::new();
    for result in loaded {
        match result {
            Ok(outcome) => documents.push(outcome.value),
            // The row set is already filtered by the caller's read rights. A row that is still
            // refused (a grant that depends on the document's state) or gone (removed while
            // paging) is left out of the page; failing the whole page would tell the caller
            // that a row it cannot see exists. The cursor was built from the key rows, so
            // the page after this one starts in the right place either way.
            Err(e) if matches!(e.class, ErrorClass::PermissionDenied | ErrorClass::NotFound) => {}
            Err(e) => return Err(api_error(e)),
        }
    }
    Ok(RawPage {
        documents,
        next_cursor,
        total,
        principal,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_app_api::AppError;
    use spot_http::api::service::ErrorCode;

    #[test]
    fn a_field_the_caller_may_not_use_is_answered_as_an_unknown_field() {
        let refused = AppError::new(
            ErrorClass::PermissionDenied,
            "permission.field_denied",
            "The user may not filter, sort, group or aggregate on that field.",
        );
        let answer = plan_refusal(refused);
        assert_eq!(answer.code, ErrorCode::Registry(Code::DocumentUnknownField));
        assert!(answer.fields.is_empty(), "no field path in the answer");
        assert!(answer.details.is_empty(), "no reason in the answer");
    }

    #[test]
    fn any_other_refusal_keeps_its_own_mapping() {
        let refused = AppError::new(
            ErrorClass::PermissionDenied,
            "permission.denied",
            "The query cannot be answered under the user's permissions.",
        );
        assert_eq!(
            plan_refusal(refused).code,
            ErrorCode::Registry(Code::PermissionDenied)
        );
    }
}
