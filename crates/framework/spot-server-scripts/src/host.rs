//! The host surface: the closed, schema-checked set of functions a script may call, and the
//! bridge that carries them to the step's [`HookContext`].
//!
//! Every function takes one object argument (the engine's surface is one-argument). Nothing else
//! is reachable: there is no `fetch`, no `require`, no filesystem, no environment and no `eval`
//! (the engine closes those). `host.now()` is the engine's own deterministic clock.
//!
//! The read and write functions go through the step's context, so the acting user's permissions
//! decide: `host.get`/`host.query` see only what the user may read (the engine masks), and
//! `host.insert`/`host.update` run the target's full lifecycle as the same user.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value as Json, json};
use spot_app_api::prelude::{Expr, Name, Query, QueryPath, RecordId, Selection, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, ErrorClass, HookContext, Invocation};
use spot_money::currency::{Currency, CurrencyTable};
use spot_money::rounding::{Mode, RoundingRule};
use spot_ts::{FieldProblem, HostBridge, HostError, HostFn, HostSurface, Shape};

use crate::log::LogLevel;
use crate::view::{document_from_json, document_json, patch_from_json, value_json};

/// The most rows `host.query` may return, whatever the script asks for.
pub const MAX_QUERY_LIMIT: u32 = 1_000;

/// A whitelisted-method call a script makes with `host.call`.
///
/// The app API's `HookContext` has no method dispatch (a lifecycle step is not a method call), so
/// the host supplies this capability. It runs the method as the invocation's actor, so the
/// method's own permission checks apply.
pub trait MethodCaller: Send + Sync {
    /// Calls the whitelisted method `method` on `doctype`/`id` as `inv`'s actor.
    fn call<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
    ) -> BoxFuture<'a, AppResult<Json>>;
}

/// The closed allow-list of host functions.
pub fn host_surface() -> HostSurface {
    let object = |required: Vec<(&str, Shape)>, open: bool| Shape::Object {
        required: required
            .into_iter()
            .map(|(name, shape)| (name.to_owned(), shape))
            .collect(),
        open,
    };
    HostSurface::new(vec![
        HostFn::new(
            "log",
            object(
                vec![("level", Shape::Text), ("message", Shape::Text)],
                false,
            ),
        ),
        HostFn::new(
            "get",
            object(vec![("doctype", Shape::Text), ("id", Shape::Text)], true),
        ),
        HostFn::new("query", object(vec![("doctype", Shape::Text)], true)),
        HostFn::new(
            "insert",
            object(vec![("doctype", Shape::Text), ("values", Shape::Any)], true),
        ),
        HostFn::new(
            "update",
            object(
                vec![
                    ("doctype", Shape::Text),
                    ("id", Shape::Text),
                    ("values", Shape::Any),
                ],
                true,
            ),
        ),
        HostFn::new(
            "call",
            object(
                vec![
                    ("doctype", Shape::Text),
                    ("id", Shape::Text),
                    ("method", Shape::Text),
                ],
                true,
            ),
        ),
        HostFn::new("throw", object(vec![("code", Shape::Text)], true)),
        HostFn::new(
            "format_money",
            object(
                vec![("value", Shape::Text), ("currency", Shape::Text)],
                true,
            ),
        ),
        HostFn::new(
            "round",
            object(vec![("value", Shape::Text), ("scale", Shape::Int)], true),
        ),
    ])
}

/// A typed validation error a script raised with `host.throw`, kept by the bridge so it survives
/// the script catching the `HostError` the bridge throws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedThrow {
    /// The script's own code.
    pub code: String,
    /// The message for the user.
    pub message: String,
    /// The invalid fields.
    pub fields: Vec<FieldProblem>,
}

/// One host call waiting to be serviced on the caller's task.
pub struct HostRequest {
    /// The host function's name.
    pub name: String,
    /// Its one JSON argument.
    pub args: Json,
    /// Where the answer goes.
    pub reply: std::sync::mpsc::SyncSender<Result<Json, HostError>>,
}

/// The bridge a run's engine thread sees: it forwards the read/write calls to the caller's task
/// and answers `log`, `throw`, `format_money` and `round` itself.
pub struct ScriptBridge {
    requests: tokio::sync::mpsc::UnboundedSender<HostRequest>,
    now_millis: i64,
    seed: u64,
    host_calls: Arc<AtomicU32>,
    typed_throw: Arc<Mutex<Option<TypedThrow>>>,
    logged: Arc<Mutex<Vec<(LogLevel, String)>>>,
    currencies: Option<Arc<CurrencyTable>>,
}

impl ScriptBridge {
    /// A bridge forwarding calls over `requests`.
    pub fn new(
        requests: tokio::sync::mpsc::UnboundedSender<HostRequest>,
        now_millis: i64,
        seed: u64,
        currencies: Option<Arc<CurrencyTable>>,
    ) -> ScriptBridge {
        ScriptBridge {
            requests,
            now_millis,
            seed,
            host_calls: Arc::new(AtomicU32::new(0)),
            typed_throw: Arc::new(Mutex::new(None)),
            logged: Arc::new(Mutex::new(Vec::new())),
            currencies,
        }
    }

    /// How many host calls the run made.
    pub fn host_calls(&self) -> u32 {
        self.host_calls.load(Ordering::Relaxed)
    }

    /// The typed throw the script raised, if any.
    pub fn typed_throw(&self) -> Option<TypedThrow> {
        self.typed_throw
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The last message the script logged.
    pub fn last_message(&self) -> Option<String> {
        self.logged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .map(|(_, message)| message.clone())
    }

    fn local(&self, name: &str, args: &Json) -> Option<Result<Json, HostError>> {
        match name {
            "log" => {
                let level = args
                    .get("level")
                    .and_then(Json::as_str)
                    .map(LogLevel::parse)
                    .unwrap_or(LogLevel::Info);
                let message = args
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_owned();
                self.logged
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((level, message));
                Some(Ok(Json::Null))
            }
            "throw" => {
                let code = args
                    .get("code")
                    .and_then(Json::as_str)
                    .unwrap_or("script.validation")
                    .to_owned();
                let message = args
                    .get("message")
                    .and_then(Json::as_str)
                    .unwrap_or(&code)
                    .to_owned();
                let fields = args
                    .get("fields")
                    .and_then(Json::as_array)
                    .map(|items| items.iter().filter_map(field_problem).collect())
                    .unwrap_or_default();
                let thrown = TypedThrow {
                    code: code.clone(),
                    message: message.clone(),
                    fields,
                };
                *self
                    .typed_throw
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(thrown);
                Some(Err(HostError::new("throw", &message)))
            }
            "round" => Some(round_call(args)),
            "format_money" => Some(self.format_money_call(args)),
            _ => None,
        }
    }
}

impl HostBridge for ScriptBridge {
    fn call(&self, name: &str, args: Json) -> Result<Json, HostError> {
        self.host_calls.fetch_add(1, Ordering::Relaxed);
        if let Some(local) = self.local(name, &args) {
            return local;
        }
        let (reply, answer) = std::sync::mpsc::sync_channel(1);
        self.requests
            .send(HostRequest {
                name: name.to_owned(),
                args,
                reply,
            })
            .map_err(|_| HostError::new("internal", "the host is not listening"))?;
        answer
            .recv()
            .map_err(|_| HostError::new("internal", "the host did not answer"))?
    }

    fn now_millis(&self) -> i64 {
        self.now_millis
    }

    fn random_seed(&self) -> u64 {
        self.seed
    }
}

impl ScriptBridge {
    fn format_money_call(&self, args: &Json) -> Result<Json, HostError> {
        let Some(currencies) = &self.currencies else {
            return Err(HostError::new(
                "unsupported",
                "this host has no currency table",
            ));
        };
        let value = args.get("value").and_then(Json::as_str).ok_or_else(|| {
            HostError::new("bad_arguments", "format_money needs a decimal string")
        })?;
        let currency = args
            .get("currency")
            .and_then(Json::as_str)
            .ok_or_else(|| HostError::new("bad_arguments", "format_money needs a currency"))?;
        let code = Currency::new(currency)
            .map_err(|_| HostError::new("bad_arguments", "not a currency code"))?;
        let scale = currencies
            .exponent(&code)
            .map_err(|_| HostError::new("bad_arguments", "the currency is not in the table"))?;
        let mode = args
            .get("mode")
            .and_then(Json::as_str)
            .and_then(Mode::parse)
            .unwrap_or(Mode::HalfUp);
        Ok(Json::String(round_to(value, scale, mode)?))
    }
}

fn round_call(args: &Json) -> Result<Json, HostError> {
    let value = args
        .get("value")
        .and_then(Json::as_str)
        .ok_or_else(|| HostError::new("bad_arguments", "round needs a decimal string"))?;
    let scale = args
        .get("scale")
        .and_then(Json::as_i64)
        .and_then(|s| u32::try_from(s).ok())
        .ok_or_else(|| HostError::new("bad_arguments", "round needs a scale"))?;
    let mode = args
        .get("mode")
        .and_then(Json::as_str)
        .and_then(Mode::parse)
        .unwrap_or(Mode::HalfUp);
    Ok(Json::String(round_to(value, scale, mode)?))
}

fn round_to(value: &str, scale: u32, mode: Mode) -> Result<String, HostError> {
    let decimal = spot_money::rounding::parse_decimal(value)
        .map_err(|_| HostError::new("bad_arguments", "the value is not a decimal"))?;
    let rule = RoundingRule::at_scale(scale, mode)
        .map_err(|_| HostError::new("bad_arguments", "the scale is not usable"))?;
    rule.apply(decimal)
        .map(|rounded| rounded.to_string())
        .map_err(|_| HostError::new("bad_arguments", "the value could not be rounded"))
}

fn field_problem(value: &Json) -> Option<FieldProblem> {
    Some(FieldProblem {
        field: value.get("field")?.as_str()?.to_owned(),
        code: value.get("code")?.as_str()?.to_owned(),
        message: value
            .get("message")
            .and_then(Json::as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

/// Services one forwarded host call on the caller's task.
pub(crate) async fn service(
    host: &mut dyn HookContext,
    method_caller: Option<&dyn MethodCaller>,
    name: &str,
    args: Json,
) -> Result<Json, HostError> {
    match name {
        "get" => service_get(host, &args).await,
        "query" => service_query(host, &args).await,
        "insert" => service_insert(host, &args).await,
        "update" => service_update(host, &args).await,
        "call" => service_call(host, method_caller, &args).await,
        other => Err(HostError::new(
            "internal",
            &format!("`{other}` is not a host function"),
        )),
    }
}

async fn service_get(host: &mut dyn HookContext, args: &Json) -> Result<Json, HostError> {
    let doctype = text(args, "doctype")?;
    let id = record_id(args, "id")?;
    let project = string_list(args.get("fields"));
    let doc = host.load(doctype, id).await.map_err(host_error)?;
    Ok(document_json(&doc, project.as_deref()))
}

async fn service_query(host: &mut dyn HookContext, args: &Json) -> Result<Json, HostError> {
    let query = build_query(args)?;
    let rows = host.query(&query).await.map_err(host_error)?;
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

async fn service_insert(host: &mut dyn HookContext, args: &Json) -> Result<Json, HostError> {
    let doctype = text(args, "doctype")?;
    let values = args.get("values").cloned().unwrap_or_else(|| json!({}));
    let def = host.meta(doctype).await.map_err(host_error)?;
    let doc = document_from_json(&def, &values, RecordId::new_v7()).map_err(host_error)?;
    let saved = host.create(doc).await.map_err(host_error)?;
    Ok(document_json(&saved, None))
}

async fn service_update(host: &mut dyn HookContext, args: &Json) -> Result<Json, HostError> {
    let doctype = text(args, "doctype")?;
    let id = record_id(args, "id")?;
    let values = args.get("values").cloned().unwrap_or_else(|| json!({}));
    let def = host.meta(doctype).await.map_err(host_error)?;
    let patch = patch_from_json(&def, &values, RecordId::new_v7).map_err(host_error)?;
    let expected = match args.get("expected").and_then(Json::as_u64) {
        Some(version) => spot_app_api::prelude::Version(version),
        None => {
            let loaded = host.load(doctype, id).await.map_err(host_error)?;
            loaded
                .version()
                .ok_or_else(|| HostError::new("not_found", "the document has no version"))?
        }
    };
    let saved = host
        .update(doctype, id, patch, expected)
        .await
        .map_err(host_error)?;
    Ok(document_json(&saved, None))
}

async fn service_call(
    host: &mut dyn HookContext,
    method_caller: Option<&dyn MethodCaller>,
    args: &Json,
) -> Result<Json, HostError> {
    let doctype = text(args, "doctype")?;
    let id = record_id(args, "id")?;
    let method = text(args, "method")?;
    let call_args = args.get("args").cloned().unwrap_or_else(|| json!({}));
    let Some(caller) = method_caller else {
        return Err(HostError::new(
            "unsupported",
            "this host offers no whitelisted-method dispatch to lifecycle steps",
        ));
    };
    let invocation = host.invocation().clone();
    caller
        .call(&invocation, doctype, id, method, call_args)
        .await
        .map_err(host_error)
}

/// Builds the query `host.query` runs: an equality filter per key, the named columns (or `id`),
/// and a limit capped at [`MAX_QUERY_LIMIT`].
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
            let literal = json_to_value_lenient(value);
            filters.push(Expr::field(&alias, &name).eq(Expr::value(literal)));
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
    query.limit = Some(limit.clamp(1, MAX_QUERY_LIMIT));
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
    let text = text(args, key)?;
    crate::view::parse_record_id(text)
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

fn host_error(error: AppError) -> HostError {
    let code = match error.class {
        ErrorClass::PermissionDenied => "denied",
        ErrorClass::NotFound => "not_found",
        ErrorClass::Duplicate => "duplicate",
        ErrorClass::StaleVersion => "stale_version",
        ErrorClass::Validation => "invalid",
        ErrorClass::UnknownDocType => "unknown_doctype",
        _ => "failed",
    };
    HostError::new(code, &error.message)
}

/// The app error a `host.throw` becomes: a `Validation` error keeping the script's code, message
/// and field errors.
pub fn typed_throw_error(thrown: &TypedThrow, script: &str) -> AppError {
    let mut error = match thrown.fields.split_first() {
        None => AppError::new(ErrorClass::Validation, &thrown.code, thrown.message.clone()),
        Some((first, rest)) => {
            let path = spot_app_api::error::FieldPath::parse(&first.field)
                .unwrap_or_else(|| spot_app_api::error::FieldPath::field(&first.field));
            let mut error = AppError::field(path, &first.code, first.message.clone());
            for field in rest {
                let path = spot_app_api::error::FieldPath::parse(&field.field)
                    .unwrap_or_else(|| spot_app_api::error::FieldPath::field(&field.field));
                error = error.with_field(path, &field.code, field.message.clone());
            }
            error.message.clone_from(&thrown.message);
            error
        }
    };
    error = error.detail("script", script);
    error
}
