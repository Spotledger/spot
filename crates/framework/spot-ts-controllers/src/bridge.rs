//! The host surface a controller handler sees, and the bridge that services its calls against the
//! step's or method's context.
//!
//! The surface is **the same closed set** Server Scripts expose (WP-EXT-06.T1): `log`, `get`,
//! `query`, `insert`, `update`, `call`, `throw`, `format_money`, `round`, plus the engine's
//! `now`/`random`. This crate adds one function, `call_method`, so a handler can call another
//! document's whitelisted method as the triggering user. Nothing else is reachable: no network, no
//! filesystem, no process, no environment, no `eval`.
//!
//! Every read and write goes through the step's [`Repository`], so the acting user's permissions
//! decide: `get`/`query` see only what the user may read, and `insert`/`update`/`call_method` run
//! the target's full lifecycle as the same user.

use serde_json::{Map, Value as Json, json};
use spot_app_api::prelude::{Expr, Name, Query, QueryPath, RecordId, Selection, Value, Version};
use spot_app_api::{AppResult, DocTypeDef, Document, Invocation, Repository};
use spot_server_scripts::{MethodCaller, ScriptBridge, document_json, patch_from_json, value_json};
use spot_ts::{HostError, HostFn, HostSurface, Shape};

/// The closed allow-list of host functions a controller handler may call: the Server Script
/// surface plus `call_method`.
pub fn ts_host_surface() -> HostSurface {
    let mut functions = spot_server_scripts::host_surface().functions;
    functions.push(HostFn::new(
        "call_method",
        Shape::Object {
            required: vec![
                ("doctype".to_owned(), Shape::Text),
                ("id".to_owned(), Shape::Text),
                ("method".to_owned(), Shape::Text),
            ],
            open: true,
        },
    ));
    HostSurface::new(functions)
}

/// The bridge a controller run's engine thread sees.
///
/// It is the Server Scripts bridge (which answers `log`, `throw`, `round` and `format_money`
/// itself and forwards the rest) under this crate's name, so a host wires the same object.
pub type TsBridge = ScriptBridge;

/// Services one forwarded host call on the caller's task, against `repo` (a step's
/// [`HookContext`](spot_app_api::HookContext) or a method's
/// [`MethodContext`](spot_app_api::MethodContext)).
///
/// # Errors
///
/// A [`HostError`] the handler sees as a thrown `HostError` it may catch.
pub async fn service_repo(
    repo: &mut dyn Repository,
    inv: &Invocation,
    caller: Option<&dyn MethodCaller>,
    name: &str,
    args: Json,
) -> Result<Json, HostError> {
    match name {
        "get" => service_get(repo, &args).await,
        "query" => service_query(repo, &args).await,
        "insert" => service_insert(repo, &args).await,
        "update" => service_update(repo, &args).await,
        "call" | "call_method" => service_call(repo, inv, caller, &args).await,
        other => Err(HostError::new(
            "internal",
            &format!("`{other}` is not a host function"),
        )),
    }
}

async fn service_get(repo: &mut dyn Repository, args: &Json) -> Result<Json, HostError> {
    let doctype = text(args, "doctype")?;
    let id = record_id(args, "id")?;
    let project = string_list(args.get("fields"));
    let doc = repo.load(doctype, id).await.map_err(host_error)?;
    Ok(document_json(&doc, project.as_deref()))
}

async fn service_query(repo: &mut dyn Repository, args: &Json) -> Result<Json, HostError> {
    let query = build_query(args)?;
    let rows = repo.query(&query).await.map_err(host_error)?;
    let mut out = Vec::with_capacity(rows.rows.len());
    for row in &rows.rows {
        let mut object = Map::new();
        for (index, column) in rows.columns.iter().enumerate() {
            if let Some(value) = row.get(index) {
                object.insert(column.as_str().to_owned(), value_json(value));
            }
        }
        out.push(Json::Object(object));
    }
    Ok(Json::Array(out))
}

async fn service_insert(repo: &mut dyn Repository, args: &Json) -> Result<Json, HostError> {
    let doctype = text(args, "doctype")?;
    let values = args.get("values").cloned().unwrap_or_else(|| json!({}));
    let def = repo.meta(doctype).await.map_err(host_error)?;
    let doc = document_from_values(&def, &values, RecordId::new_v7()).map_err(host_error)?;
    let saved = repo.create(doc).await.map_err(host_error)?;
    Ok(document_json(&saved, None))
}

async fn service_update(repo: &mut dyn Repository, args: &Json) -> Result<Json, HostError> {
    let doctype = text(args, "doctype")?;
    let id = record_id(args, "id")?;
    let values = args.get("values").cloned().unwrap_or_else(|| json!({}));
    let def = repo.meta(doctype).await.map_err(host_error)?;
    let patch = patch_from_json(&def, &values, RecordId::new_v7).map_err(host_error)?;
    let expected = match args.get("expected").and_then(Json::as_u64) {
        Some(version) => Version(version),
        None => repo
            .load(doctype, id)
            .await
            .map_err(host_error)?
            .version()
            .ok_or_else(|| HostError::new("not_found", "the document has no version"))?,
    };
    let saved = repo
        .update(doctype, id, patch, expected)
        .await
        .map_err(host_error)?;
    Ok(document_json(&saved, None))
}

async fn service_call(
    repo: &mut dyn Repository,
    inv: &Invocation,
    caller: Option<&dyn MethodCaller>,
    args: &Json,
) -> Result<Json, HostError> {
    let _ = repo;
    let doctype = text(args, "doctype")?;
    let id = record_id(args, "id")?;
    let method = text(args, "method")?;
    let call_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
    let Some(caller) = caller else {
        return Err(HostError::new(
            "unsupported",
            "this host offers no whitelisted-method dispatch to a controller",
        ));
    };
    caller
        .call(inv, doctype, id, method, call_args)
        .await
        .map_err(host_error)
}

/// A new document from the JSON a handler passed to `host.insert`.
///
/// # Errors
///
/// `document.unknown_field` and `document.invalid_value`, as field errors.
pub fn document_from_values(def: &DocTypeDef, values: &Json, id: RecordId) -> AppResult<Document> {
    let mut doc = Document::new(def.name.clone(), id);
    let patch = patch_from_json(def, values, RecordId::new_v7)?;
    for (name, value) in patch.fields {
        doc.set(name, value);
    }
    for (table, rows) in patch.tables {
        let target = doc.rows_mut(table);
        for row in rows {
            target.push(spot_app_api::ChildRow {
                id: row.id.unwrap_or_else(RecordId::new_v7),
                fields: row.fields,
            });
        }
    }
    Ok(doc)
}

/// The query `host.query` runs: an equality filter per key, the named columns (or `id`) and a
/// limit capped at [`spot_server_scripts::MAX_QUERY_LIMIT`].
fn build_query(args: &Json) -> Result<Query, HostError> {
    let doctype = text(args, "doctype")?;
    let entity = Name::new(doctype)
        .map_err(|_| HostError::new("bad_arguments", "`doctype` is not a DocType name"))?;
    let alias =
        Name::new("t").map_err(|_| HostError::new("internal", "the query alias is fixed"))?;
    let mut query = Query::from(&entity, &alias, QueryPath::Transactional);
    let fields = string_list(args.get("fields")).unwrap_or_else(|| vec!["id".to_owned()]);
    for field in &fields {
        let name = Name::new(field)
            .map_err(|_| HostError::new("bad_arguments", "a field name is not a storage name"))?;
        query.select.push(Selection {
            expr: Expr::field(&alias, &name),
            alias: name,
        });
    }
    let mut filters = Vec::new();
    if let Some(filter) = args.get("filter").and_then(Json::as_object) {
        for (key, value) in filter {
            let name = Name::new(key)
                .map_err(|_| HostError::new("bad_arguments", "a filter name is not a field"))?;
            filters.push(Expr::field(&alias, &name).eq(Expr::value(json_to_value_lenient(value))));
        }
    }
    if !filters.is_empty() {
        query.filter = Some(Expr::And(filters));
    }
    let limit = args
        .get("limit")
        .and_then(Json::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(100);
    query.limit = Some(limit.clamp(1, spot_server_scripts::MAX_QUERY_LIMIT));
    Ok(query)
}

fn json_to_value_lenient(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Text(n.to_string()),
        },
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Json(other.to_string()),
    }
}

fn text<'a>(args: &'a Json, key: &str) -> Result<&'a str, HostError> {
    args.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| HostError::new("bad_arguments", &format!("`{key}` is required")))
}

fn record_id(args: &Json, key: &str) -> Result<RecordId, HostError> {
    parse_record_id(text(args, key)?)
        .ok_or_else(|| HostError::new("bad_arguments", &format!("`{key}` is not a record id")))
}

fn string_list(value: Option<&Json>) -> Option<Vec<String>> {
    value.and_then(Json::as_array).map(|items| {
        items
            .iter()
            .filter_map(Json::as_str)
            .map(str::to_owned)
            .collect()
    })
}

fn host_error(error: spot_app_api::AppError) -> HostError {
    let code = match error.class {
        spot_app_api::ErrorClass::PermissionDenied => "denied",
        spot_app_api::ErrorClass::NotFound => "not_found",
        spot_app_api::ErrorClass::Duplicate => "duplicate",
        spot_app_api::ErrorClass::StaleVersion => "stale_version",
        spot_app_api::ErrorClass::Validation => "invalid",
        spot_app_api::ErrorClass::UnknownDocType => "unknown_doctype",
        _ => "failed",
    };
    HostError::new(code, &error.message)
}

/// Parses a UUID string into a record id. Only version 7 ids are accepted (ADR-33).
pub fn parse_record_id(text: &str) -> Option<RecordId> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    RecordId::from_bytes(bytes).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_surface_is_the_server_script_surface_plus_call_method() {
        let surface = ts_host_surface();
        let names: Vec<&str> = surface.functions.iter().map(|f| f.name.as_str()).collect();
        for expected in [
            "log",
            "get",
            "query",
            "insert",
            "update",
            "throw",
            "call_method",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
        }
    }

    #[test]
    fn a_record_id_round_trips() {
        let id = RecordId::new_v7();
        assert_eq!(parse_record_id(&id.to_string()), Some(id));
        assert_eq!(parse_record_id("not-an-id"), None);
    }
}
