//! Tool execution: the code that runs one resolved tool as the calling principal.
//!
//! Split out of `server.rs` to keep every file under the repository's size cap. Each method
//! is one service invocation under the configured limits; none holds a connection across calls.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_app_api::context::Invocation;
use spot_app_api::document::{Document, DocumentPatch};
use spot_app_api::meta::{DocTypeDef, FieldDef};
use spot_app_api::prelude::{Name, RecordId, Value, Version};
use spot_functions::{Caller, Context, FunctionKey};
use spot_permissions::{DocFacts, FieldFacts, FieldLevel, PermissionEngine};
use spot_storage_contract::{Collation, Rows};
use time::OffsetDateTime;

use crate::catalog::{ToolKind, ToolSpec};
use crate::error::{McpError, from_app_error};
use crate::principal::McpPrincipal;
use crate::protocol::{CallToolResult, Content};
use crate::query::{apply_masks, plan};
use crate::server::{MetaState, Server};

impl Server {
    pub(crate) async fn execute(
        &self,
        principal: &McpPrincipal,
        tool: &ToolSpec,
        arguments: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        match &tool.kind {
            ToolKind::Search { doctype } => self.search(principal, doctype, arguments, now).await,
            ToolKind::Get { doctype } => self.get(principal, doctype, arguments, now).await,
            ToolKind::CreateDraft { doctype } => {
                self.create_draft(principal, doctype, arguments, now).await
            }
            ToolKind::Update { doctype } => self.update(principal, doctype, arguments, now).await,
            ToolKind::Query => self.run_query(principal, arguments, now).await,
            ToolKind::JobStatus => {
                let id = arguments
                    .get("job_id")
                    .and_then(Json::as_u64)
                    .ok_or_else(|| McpError::invalid_arguments("`job_id` is required"))?;
                let view = crate::jobs::status(self.jobs.as_ref(), id).await?;
                Ok(structured(view.to_json(), false))
            }
            ToolKind::Function {
                app,
                module,
                name,
                async_job,
            } => {
                let call = FunctionCall {
                    app,
                    module,
                    name,
                    async_job: *async_job,
                };
                self.call_function(principal, &call, arguments, now).await
            }
        }
    }

    pub(crate) fn invocation(&self, principal: &McpPrincipal, now: OffsetDateTime) -> Invocation {
        principal.invocation(self.site.clone(), None, None, now)
    }

    pub(crate) fn state(&self) -> Arc<MetaState> {
        self.meta.state()
    }

    async fn search(
        &self,
        principal: &McpPrincipal,
        doctype: &str,
        arguments: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        let state = self.state();
        let def = state
            .registry
            .effective()
            .doctype(doctype)
            .ok_or_else(|| McpError::not_found("the DocType does not exist"))?;
        let spec = search_spec(&state.engine, principal, &def, arguments)?;
        self.run_planned(principal, &state, &def, &spec, now).await
    }

    async fn run_query(
        &self,
        principal: &McpPrincipal,
        arguments: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        let doctype = arguments
            .get("doctype")
            .and_then(Json::as_str)
            .ok_or_else(|| McpError::invalid_arguments("`doctype` is required"))?;
        let state = self.state();
        let def = state
            .registry
            .effective()
            .doctype(doctype)
            .ok_or_else(|| McpError::not_found("the DocType does not exist"))?;
        self.run_planned(principal, &state, &def, arguments, now)
            .await
    }

    async fn run_planned(
        &self,
        principal: &McpPrincipal,
        state: &MetaState,
        def: &DocTypeDef,
        spec: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        let planned = plan(
            &state.engine,
            principal.permissions(),
            def,
            spec,
            &self.limits,
        )?;
        let Some(planned) = planned else {
            return Ok(structured(json!({ "columns": [], "rows": [] }), true));
        };
        let catalog = Arc::new(
            state
                .registry
                .effective()
                .catalog(Collation::new("und").map_err(|_| McpError::internal("bad collation"))?)
                .map_err(|_| McpError::internal("could not build the catalog"))?,
        );
        let inv = self.invocation(principal, now);
        let mut rows = self.query.read(&inv, catalog, &planned.query).await?;
        apply_masks(&mut rows, &planned.masks);
        Ok(structured(rows_to_json(&rows), true))
    }

    async fn get(
        &self,
        principal: &McpPrincipal,
        doctype: &str,
        arguments: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        let id = parse_id(arguments)?;
        let inv = self.invocation(principal, now);
        let outcome = self
            .service
            .load(&inv, doctype, id)
            .await
            .map_err(|e| from_app_error(&e))?;
        let state = self.state();
        let def = state
            .registry
            .effective()
            .doctype(doctype)
            .ok_or_else(|| McpError::not_found("the DocType does not exist"))?;
        let value = document_json(&state.engine, principal, &def, &outcome.value);
        Ok(structured(value, true))
    }

    async fn create_draft(
        &self,
        principal: &McpPrincipal,
        doctype: &str,
        arguments: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        let state = self.state();
        let def = state
            .registry
            .effective()
            .doctype(doctype)
            .ok_or_else(|| McpError::not_found("the DocType does not exist"))?;
        let mut doc = Document::new(def.name.clone(), RecordId::new_v7());
        for (key, value) in fields_of(arguments) {
            let Some(field) = def.field(key) else {
                return Err(McpError::invalid_arguments(format!(
                    "unknown field `{key}`"
                )));
            };
            doc.set(
                field.name.clone(),
                crate::schema::decode_value(field, value)?,
            );
        }
        let inv = self.invocation(principal, now);
        let saved = self
            .service
            .create(&inv, doc)
            .await
            .map_err(|e| from_app_error(&e))?;
        Ok(structured(document_summary(&saved.value), true))
    }

    async fn update(
        &self,
        principal: &McpPrincipal,
        doctype: &str,
        arguments: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        let id = parse_id(arguments)?;
        let state = self.state();
        let def = state
            .registry
            .effective()
            .doctype(doctype)
            .ok_or_else(|| McpError::not_found("the DocType does not exist"))?;
        let inv = self.invocation(principal, now);
        let expected = match arguments.get("version").and_then(Json::as_u64) {
            Some(v) => Version(v),
            None => self
                .service
                .load(&inv, doctype, id)
                .await
                .map_err(|e| from_app_error(&e))?
                .value
                .version()
                .ok_or_else(|| McpError::internal("the document has no version"))?,
        };
        let mut patch = DocumentPatch::default();
        for (key, value) in fields_of(arguments) {
            if key == "id" || key == "version" {
                continue;
            }
            let Some(field) = def.field(key) else {
                return Err(McpError::invalid_arguments(format!(
                    "unknown field `{key}`"
                )));
            };
            patch.fields.insert(
                field.name.clone(),
                crate::schema::decode_value(field, value)?,
            );
        }
        let saved = self
            .service
            .update(&inv, doctype, id, patch, expected)
            .await
            .map_err(|e| from_app_error(&e))?;
        Ok(structured(document_summary(&saved.value), true))
    }

    async fn call_function(
        &self,
        principal: &McpPrincipal,
        call: &FunctionCall<'_>,
        arguments: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        let FunctionCall {
            app,
            module,
            name,
            async_job,
        } = *call;
        let key = FunctionKey::parse(app, module, name)
            .map_err(|_| McpError::internal("the function key is invalid"))?;
        if async_job {
            let id = crate::jobs::enqueue(
                self.jobs.as_ref(),
                &self.site,
                &format!("{app}:{module}:{name}"),
                arguments,
                now,
                None,
            )
            .await?;
            return Ok(structured(json!({ "job_id": id }), false));
        }
        let verb = self
            .functions
            .list()
            .into_iter()
            .find(|f| f.key.app() == app && f.key.module() == module && f.key.name() == name)
            .and_then(|f| f.verbs)
            .and_then(|verbs| verbs.first().copied());
        let mut ctx = Context::new(
            self.site.as_str(),
            Caller::User(principal.user().to_owned()),
        );
        ctx.verb = verb;
        let result = self
            .functions
            .dispatch(&ctx, &key, arguments.clone())
            .await
            .map_err(|e| from_app_error(&e))?;
        Ok(structured(result, false))
    }
}

/// The parts of a whitelisted-function tool call.
#[derive(Clone, Copy, Debug)]
struct FunctionCall<'a> {
    app: &'a str,
    module: &'a str,
    name: &'a str,
    async_job: bool,
}

fn structured(value: Json, untrusted: bool) -> CallToolResult {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
    CallToolResult {
        content: vec![Content::Text(text)],
        structured: Some(value),
        is_error: false,
        untrusted,
    }
}

fn rows_to_json(rows: &Rows) -> Json {
    let columns: Vec<&str> = rows.columns.iter().map(Name::as_str).collect();
    let values: Vec<Json> = rows
        .rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (i, value) in row.iter().enumerate() {
                if let Some(column) = columns.get(i) {
                    obj.insert(
                        (*column).to_owned(),
                        crate::schema::encode_value_loose(value),
                    );
                }
            }
            Json::Object(obj)
        })
        .collect();
    json!({ "columns": columns, "rows": values })
}

pub(crate) fn doctype_schema(def: &DocTypeDef) -> Json {
    let fields: Vec<Json> = def
        .fields
        .iter()
        .map(|f| {
            json!({
                "name": f.name.as_str(),
                "label": f.label,
                "required": f.required,
                "read_only": f.read_only,
                "schema": crate::schema::field_schema(f),
            })
        })
        .collect();
    json!({ "doctype": def.name.as_str(), "label": def.label, "fields": fields })
}

fn document_summary(doc: &Document) -> Json {
    json!({
        "id": doc.id().to_string(),
        "version": doc.version().map(|v| v.0),
        "state": doc.state().map(|s| s.as_str()),
    })
}

pub(crate) fn document_json(
    engine: &PermissionEngine,
    principal: &McpPrincipal,
    def: &DocTypeDef,
    doc: &Document,
) -> Json {
    let doctype = def.name.as_str();
    let values: BTreeMap<String, Value> = doc
        .fields()
        .iter()
        .map(|(k, v)| (k.as_str().to_owned(), v.clone()))
        .collect();
    let facts = DocFacts {
        doctype: doctype.to_owned(),
        id: Some(doc.id().to_string()),
        created_by: values.get("created_by").and_then(|v| match v {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        }),
        state: doc.state().map(|s| s.as_str().to_owned()),
        values,
    };
    let mut obj = serde_json::Map::new();
    obj.insert("id".to_owned(), Json::String(doc.id().to_string()));
    if let Some(version) = doc.version() {
        obj.insert("version".to_owned(), Json::from(version.0));
    }
    for field in &def.fields {
        let facts_for_field = engine
            .snapshot()
            .facts(doctype)
            .and_then(|f| f.field(field.name.as_str()).cloned())
            .unwrap_or_else(|| FieldFacts::plain(field.name.as_str()));
        let decision =
            engine.field_access(principal.permissions(), doctype, &facts_for_field, &facts);
        let value = doc.get(field.name.as_str());
        match decision.level {
            FieldLevel::Hidden => {}
            FieldLevel::Masked(rule) => {
                obj.insert(
                    field.name.as_str().to_owned(),
                    Json::String(spot_permissions::mask(&value_text(value), &rule)),
                );
            }
            FieldLevel::Read | FieldLevel::ReadReveal => {
                obj.insert(
                    field.name.as_str().to_owned(),
                    crate::schema::encode_value(value, None),
                );
            }
        }
    }
    Json::Object(obj)
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Text(t) => t.clone(),
        other => format!("{other:?}"),
    }
}

fn fields_of(arguments: &Json) -> Vec<(&str, &Json)> {
    arguments
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.as_str(), v)).collect())
        .unwrap_or_default()
}

fn parse_id(arguments: &Json) -> Result<RecordId, McpError> {
    let id = arguments
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| McpError::invalid_arguments("`id` is required"))?;
    spot_http::api::codec::parse_record_id(id)
        .ok_or_else(|| McpError::invalid_arguments("`id` is not a record id"))
}

/// Builds the search tool's query IR spec from its arguments.
fn search_spec(
    engine: &PermissionEngine,
    principal: &McpPrincipal,
    def: &DocTypeDef,
    arguments: &Json,
) -> Result<Json, McpError> {
    let readable = engine.readable_fields(
        principal.permissions(),
        def.name.as_str(),
        &DocFacts {
            doctype: def.name.as_str().to_owned(),
            created_by: Some(principal.user().to_owned()),
            ..DocFacts::default()
        },
    );
    let requested: Vec<&FieldDef> = def
        .fields
        .iter()
        .filter(|f| readable.contains(f.name.as_str()))
        .collect();
    let mut select = vec![json!({ "expr": { "field": "id" }, "as": "id" })];
    for field in &requested {
        select.push(json!({
            "expr": { "field": field.name.as_str() },
            "as": field.name.as_str(),
        }));
    }
    let mut and = Vec::new();
    if let Some(filter) = arguments.get("filter").and_then(Json::as_object) {
        for (key, value) in filter {
            and.push(json!({
                "op": "eq",
                "lhs": { "field": key },
                "rhs": value,
            }));
        }
    }
    let mut order_by = Vec::new();
    if let Some(field) = arguments.get("order_by").and_then(Json::as_str) {
        let dir = if arguments
            .get("descending")
            .and_then(Json::as_bool)
            .unwrap_or(false)
        {
            "desc"
        } else {
            "asc"
        };
        order_by.push(json!({ "expr": { "field": field }, "dir": dir }));
    }
    let limit = arguments.get("limit").and_then(Json::as_u64);
    Ok(json!({
        "doctype": def.name.as_str(),
        "select": select,
        "filter": if and.is_empty() { Json::Null } else { json!({ "and": and }) },
        "order_by": order_by,
        "limit": limit,
    }))
}
