//! Validated HTTP filters to typed storage IR, with lexicographic (not offset) cursors.
//!
//! `DocumentService` has no top-level query method. This local seam can be replaced by a
//! permission-filtering repository implementation without changing the HTTP adapter.
use crate::conversion::{api_error, to_api_document};
use crate::meta_resources::def_meta;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value as Json, json};
use spot_app_api::context::{DocumentService, Invocation};
use spot_document::engine::Engine;
use spot_http::api::codec::{decode_scalar, encode_scalar, parse_record_id};
use spot_http::api::filter::{FilterAst, Op};
use spot_http::api::meta::{DocTypeMeta, FieldType, MetaField};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, Page, QuerySpec};
use spot_meta::EffectiveMeta;
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
}

/// The current engine has no permission checks on either loads or queries. Like that engine,
/// this implementation is for a caller-authorized, single-site backend only. Do not mount it
/// on an untrusted multi-site router without a site/policy-checking actor resolver or wrapper.
impl<B: StorageBackend> QueryService for Engine<B> {
    fn read<'a>(
        &'a self,
        _inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> BoxFuture<'a, Result<Rows, ApiError>> {
        Box::pin(async move {
            self.backend()
                .read(catalog, ReadSource::Primary, query)
                .await
                .map_err(|e| {
                    api_error(spot_app_api::error::map_storage_error(
                        &e,
                        spot_app_api::error::StorageOp::Other,
                    ))
                })
        })
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

pub(crate) async fn list<S: DocumentService + QueryService>(
    service: &S,
    effective: &EffectiveMeta,
    inv: &Invocation,
    doctype: &str,
    spec: &QuerySpec,
) -> Result<Page, ApiError> {
    let def = service.meta(doctype).await.map_err(api_error)?;
    let meta = def_meta(&def)?;
    let catalog = Arc::new(
        effective
            .catalog(Collation::new("und").map_err(|_| invalid())?)
            .map_err(|_| ApiError::from_code(Code::HttpInternal))?,
    );
    let mut q = compile(&meta, spec)?;
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
    let mut documents = Vec::new();
    for row in kept {
        let Some(Value::Id(id)) = row.first() else {
            return Err(ApiError::from_code(Code::HttpInternal));
        };
        let mut doc = to_api_document(
            service
                .load(inv, doctype, *id)
                .await
                .map_err(api_error)?
                .value,
        );
        if !spec.fields.is_empty() {
            doc.fields.retain(|key, _| spec.fields.contains(key));
        }
        documents.push(doc);
    }
    Ok(Page {
        documents,
        next_cursor,
        total,
    })
}
