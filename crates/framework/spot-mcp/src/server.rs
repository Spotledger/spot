//! The sans-I/O server core: it parses messages, dispatches methods and returns messages.
//!
//! The core holds no connection and no transaction: each call is one service invocation, run
//! strictly as the calling principal, under the configured limits. A transport (stdio or
//! Streamable HTTP) hands it a message and a credential and writes back what it returns.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_agent_tiers::Tier;
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{RecordId, SiteId};
use spot_coordinator::{Admit, Coordinator};
use spot_functions::FunctionRegistry;
use spot_jobs::JobStore;
use spot_meta::Registry;
use spot_permissions::{Action, DocFacts, PermissionEngine};
use time::OffsetDateTime;

use crate::audit::{
    AuditAlert, AuditAlerts, AuditFailurePolicy, AuditFailures, AuditPhase, AuditSink,
    TelemetryAlerts, call_record, intent_record, required_tier,
};
use crate::catalog::{ToolCatalog, ToolSpec};
use crate::error::{McpError, from_app_error};
use crate::execute::{doctype_schema, document_json};
use crate::jsonrpc::{
    Id, Incoming, Notification, Request, error_response, error_response_no_id, notification,
    parse_message, result_response,
};
use crate::limits::Limits;
use crate::principal::{Authenticator, Credential, McpPrincipal};
use crate::protocol::{CallToolResult, Prompt, PromptArgument, RequestMeta, ServerCapabilities};
use crate::query::QueryPort;
use crate::{LEGACY_PROTOCOL_VERSION, PROTOCOL_VERSION, SERVER_NAME, SERVER_VERSION};

/// The current metadata and the permission engine that reads it, swapped together.
pub struct MetaState {
    /// The effective metadata registry.
    pub registry: Arc<Registry>,
    /// The permission engine over that metadata.
    pub engine: Arc<PermissionEngine>,
}

/// Supplies the current metadata and permission engine, so a runtime-created DocType is picked
/// up on the next request.
pub trait MetadataSource: Send + Sync {
    /// The current state.
    fn state(&self) -> Arc<MetaState>;
}

/// A metadata source that never changes.
pub struct FixedMeta {
    state: Arc<MetaState>,
}

impl FixedMeta {
    /// Wraps a fixed state.
    #[must_use]
    pub fn new(state: Arc<MetaState>) -> Self {
        Self { state }
    }
}

impl MetadataSource for FixedMeta {
    fn state(&self) -> Arc<MetaState> {
        self.state.clone()
    }
}

/// A metadata source that can be swapped, for the live registry swap.
pub struct SwappableMeta {
    state: Mutex<Arc<MetaState>>,
}

impl SwappableMeta {
    /// Wraps an initial state.
    #[must_use]
    pub fn new(state: Arc<MetaState>) -> Self {
        Self {
            state: Mutex::new(state),
        }
    }

    /// Replaces the state.
    pub fn swap(&self, state: Arc<MetaState>) {
        if let Ok(mut current) = self.state.lock() {
            *current = state;
        }
    }
}

impl MetadataSource for SwappableMeta {
    fn state(&self) -> Arc<MetaState> {
        match self.state.lock() {
            Ok(current) => current.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// Everything the server needs from its host.
pub struct ServerParts {
    /// The current metadata and permission engine.
    pub meta: Arc<dyn MetadataSource>,
    /// The site (tenant) this server serves.
    pub site: SiteId,
    /// The document engine, the same seam the native API uses.
    pub service: Arc<dyn DocumentService>,
    /// The read-only query seam.
    pub query: Arc<dyn QueryPort>,
    /// The whitelisted functions.
    pub functions: Arc<FunctionRegistry>,
    /// The job store asynchronous tools enqueue to.
    pub jobs: Arc<dyn JobStore>,
    /// The coordinator rate limits go through.
    pub coordinator: Arc<dyn Coordinator>,
    /// The AI audit chain.
    pub audit: Arc<dyn AuditSink>,
    /// The credential authenticator.
    pub auth: Arc<dyn Authenticator>,
    /// The limits.
    pub limits: Limits,
    /// Natural-language guidance returned by `server/discover`.
    pub instructions: String,
}

/// The native MCP server core.
pub struct Server {
    pub(crate) meta: Arc<dyn MetadataSource>,
    pub(crate) site: SiteId,
    pub(crate) service: Arc<dyn DocumentService>,
    pub(crate) query: Arc<dyn QueryPort>,
    pub(crate) functions: Arc<FunctionRegistry>,
    pub(crate) jobs: Arc<dyn JobStore>,
    pub(crate) coordinator: Arc<dyn Coordinator>,
    pub(crate) audit: Arc<dyn AuditSink>,
    pub(crate) audit_policy: AuditFailurePolicy,
    pub(crate) audit_alerts: Arc<dyn AuditAlerts>,
    pub(crate) audit_failures: AuditFailures,
    pub(crate) auth: Arc<dyn Authenticator>,
    pub(crate) limits: Limits,
    pub(crate) instructions: String,
    pub(crate) cancels: Mutex<BTreeSet<Id>>,
}

impl Server {
    /// Builds a server from its parts.
    #[must_use]
    pub fn new(parts: ServerParts) -> Self {
        Self {
            meta: parts.meta,
            site: parts.site,
            service: parts.service,
            query: parts.query,
            functions: parts.functions,
            jobs: parts.jobs,
            coordinator: parts.coordinator,
            audit: parts.audit,
            audit_policy: AuditFailurePolicy::default(),
            audit_alerts: Arc::new(TelemetryAlerts::new()),
            audit_failures: AuditFailures::default(),
            auth: parts.auth,
            limits: parts.limits,
            instructions: parts.instructions,
            cancels: Mutex::new(BTreeSet::new()),
        }
    }

    /// Sets what happens when the audit sink refuses a record (default
    /// [`AuditFailurePolicy::FailClosedForWrites`]).
    #[must_use]
    pub fn with_audit_policy(mut self, policy: AuditFailurePolicy) -> Self {
        self.audit_policy = policy;
        self
    }

    /// Sets where audit-failure alerts go.
    #[must_use]
    pub fn with_audit_alerts(mut self, alerts: Arc<dyn AuditAlerts>) -> Self {
        self.audit_alerts = alerts;
        self
    }

    /// Sets how many failed audit records wait in memory for a retry.
    #[must_use]
    pub fn with_audit_retry_capacity(mut self, capacity: usize) -> Self {
        self.audit_failures = AuditFailures::with_capacity(capacity);
        self
    }

    /// The audit-failure counters (`mcp_audit_failures_total`) and retry queue.
    #[must_use]
    pub fn audit_failures(&self) -> &AuditFailures {
        &self.audit_failures
    }

    /// The capabilities this server declares.
    #[must_use]
    pub fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities {
            tools: true,
            resources: true,
            prompts: true,
        }
    }

    /// Handles one message text, returning the messages to write back.
    pub async fn handle_text(
        &self,
        text: &str,
        credential: Option<&Credential>,
        now: OffsetDateTime,
    ) -> Vec<Json> {
        if let Err(e) = self.limits.check_input(text.len()) {
            return vec![error_response_no_id(&e)];
        }
        match parse_message(text) {
            Ok(Incoming::Request(req)) => self.handle_request(req, credential, now).await,
            Ok(Incoming::Notification(note)) => {
                self.handle_notification(note);
                Vec::new()
            }
            Err(e) => vec![error_response_no_id(&e)],
        }
    }

    async fn handle_request(
        &self,
        req: Request,
        credential: Option<&Credential>,
        now: OffsetDateTime,
    ) -> Vec<Json> {
        let progress = progress_token(req.params.as_ref());
        let mut out = Vec::new();
        if let Some(token) = progress.clone() {
            out.push(notification(
                "notifications/progress",
                Some(json!({ "progressToken": token, "progress": 0, "total": 1 })),
            ));
        }
        let result = self.dispatch(&req, credential, now).await;
        out.push(match result {
            Ok(result) => result_response(&req.id, result),
            Err(e) => error_response(&req.id, &e),
        });
        if let Some(token) = progress {
            out.push(notification(
                "notifications/progress",
                Some(json!({ "progressToken": token, "progress": 1, "total": 1 })),
            ));
        }
        out
    }

    fn handle_notification(&self, note: Notification) {
        if note.method == "notifications/cancelled"
            && let Some(id) = note
                .params
                .as_ref()
                .and_then(|p| p.get("requestId"))
                .and_then(Id::from_json)
            && let Ok(mut cancels) = self.cancels.lock()
        {
            cancels.insert(id);
        }
    }

    fn is_cancelled(&self, id: &Id) -> bool {
        self.cancels.lock().map(|c| c.contains(id)).unwrap_or(false)
    }

    async fn principal(
        &self,
        credential: Option<&Credential>,
        now: OffsetDateTime,
    ) -> Result<McpPrincipal, McpError> {
        let credential =
            credential.ok_or_else(|| McpError::unauthorized("a credential is required"))?;
        let principal = self.auth.authenticate(credential, now.unix_timestamp())?;
        principal.assert_not_system()?;
        Ok(principal)
    }

    async fn dispatch(
        &self,
        req: &Request,
        credential: Option<&Credential>,
        now: OffsetDateTime,
    ) -> Result<Json, McpError> {
        if req.method != "initialize" {
            self.check_meta(req.params.as_ref())?;
        }
        match req.method.as_str() {
            "server/discover" => Ok(self.discover()),
            "initialize" => self.initialize(req.params.as_ref()),
            "ping" => Ok(json!({ "resultType": "complete" })),
            "tools/list" => {
                let principal = self.principal(credential, now).await?;
                self.tools_list(&principal, req.params.as_ref())
            }
            "tools/call" => {
                let principal = self.principal(credential, now).await?;
                self.tools_call(&principal, req, now).await
            }
            "resources/list" => {
                let principal = self.principal(credential, now).await?;
                self.resources_list(&principal, req.params.as_ref())
            }
            "resources/templates/list" => {
                let principal = self.principal(credential, now).await?;
                self.resource_templates(&principal, req.params.as_ref())
            }
            "resources/read" => {
                let principal = self.principal(credential, now).await?;
                self.resources_read(&principal, req.params.as_ref(), now)
                    .await
            }
            "prompts/list" => Ok(self.prompts_list(req.params.as_ref())),
            "prompts/get" => self.prompts_get(req.params.as_ref()),
            other => Err(McpError::method_not_found(other)),
        }
    }

    fn check_meta(&self, params: Option<&Json>) -> Result<(), McpError> {
        let Some(meta) = params.and_then(|p| p.get("_meta")) else {
            return Ok(());
        };
        if !meta.is_object() {
            return Err(McpError::invalid_params("`_meta` must be an object"));
        }
        let meta = RequestMeta::from_params(params);
        let Some(version) = meta.protocol_version.as_deref() else {
            return Err(McpError::invalid_params(
                "a request must carry its protocol version in `_meta`",
            ));
        };
        if version != PROTOCOL_VERSION && version != LEGACY_PROTOCOL_VERSION {
            return Err(McpError::unsupported_version(version));
        }
        if !meta.has_client_capabilities {
            return Err(McpError::invalid_params(
                "a request must carry its client capabilities in `_meta`",
            ));
        }
        Ok(())
    }

    fn discover(&self) -> Json {
        json!({
            "resultType": "complete",
            "supportedVersions": [PROTOCOL_VERSION, LEGACY_PROTOCOL_VERSION],
            "capabilities": self.capabilities().to_json(),
            "_meta": { crate::META_SERVER_INFO: { "name": SERVER_NAME, "version": SERVER_VERSION } },
            "instructions": self.instructions,
        })
    }

    fn initialize(&self, params: Option<&Json>) -> Result<Json, McpError> {
        let requested = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Json::as_str)
            .ok_or_else(|| McpError::invalid_params("`initialize` needs a protocolVersion"))?;
        if requested != PROTOCOL_VERSION && requested != LEGACY_PROTOCOL_VERSION {
            return Err(McpError::unsupported_version(requested));
        }
        Ok(json!({
            "protocolVersion": requested,
            "capabilities": self.capabilities().to_json(),
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
        }))
    }

    fn catalog(&self, principal: &McpPrincipal) -> ToolCatalog {
        let state = self.meta.state();
        ToolCatalog::build(&state.registry, &state.engine, principal, &self.functions)
    }

    fn tools_list(
        &self,
        principal: &McpPrincipal,
        params: Option<&Json>,
    ) -> Result<Json, McpError> {
        let catalog = self.catalog(principal);
        let cursor = parse_cursor(params)?;
        let (page, next) = catalog.page(cursor, 100);
        let tools: Vec<Json> = page.iter().map(|t| t.to_tool().to_json()).collect();
        let mut out = serde_json::Map::new();
        out.insert("resultType".to_owned(), Json::String("complete".to_owned()));
        out.insert("tools".to_owned(), Json::Array(tools));
        if let Some(next) = next {
            out.insert("nextCursor".to_owned(), Json::String(next.to_string()));
        }
        Ok(Json::Object(out))
    }

    async fn tools_call(
        &self,
        principal: &McpPrincipal,
        req: &Request,
        now: OffsetDateTime,
    ) -> Result<Json, McpError> {
        if self.is_cancelled(&req.id) {
            return Err(McpError::cancelled());
        }
        let params = req
            .params
            .as_ref()
            .ok_or_else(|| McpError::invalid_params("`tools/call` needs params"))?;
        let name = params
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| McpError::invalid_params("`tools/call` needs a tool name"))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let catalog = self.catalog(principal);
        let tool = catalog
            .get(name)
            .ok_or_else(|| McpError::unknown_tool(name))?
            .clone();
        let at = now.unix_timestamp() * 1_000_000;
        self.audit_failures.flush(self.audit.as_ref(), at).await;
        let write_capable = tool.kind.writes() || tool.kind.async_job();
        let policy = self.audit_policy;
        // Write-capable tools are recorded before they run, so a change is never unrecorded;
        // under `FailClosed` so is every read.
        if write_capable || policy == AuditFailurePolicy::FailClosed {
            let intent = intent_record(principal, &tool.name, at);
            if self.audit.record(intent.clone()).await.is_err() {
                let refuse = policy != AuditFailurePolicy::AlertOnly;
                self.audit_failed(&tool.name, AuditPhase::Intent, refuse, intent, at);
                if refuse {
                    return Err(McpError::audit_unavailable());
                }
            }
        }
        let result = self.call_tool(principal, &tool, &arguments, now).await;
        let record = call_record(principal, &tool.name, result.is_ok(), at);
        if self.audit.record(record.clone()).await.is_err() {
            // The call already ran. A write cannot be undone, so its result stands; a read's
            // result is withheld under `FailClosed`.
            let refuse = policy == AuditFailurePolicy::FailClosed && !write_capable;
            self.audit_failed(&tool.name, AuditPhase::Completion, refuse, record, at);
            if refuse {
                return Err(McpError::audit_unavailable());
            }
        }
        result.map(|r| r.to_json())
    }

    fn audit_failed(
        &self,
        tool: &str,
        phase: AuditPhase,
        refused: bool,
        record: spot_audit_chain::AuditRecord,
        at_micros: i64,
    ) {
        if refused {
            self.audit_failures.note_refused();
        } else {
            self.audit_failures.note_failure(record, at_micros);
        }
        self.audit_alerts.alert(&AuditAlert {
            tool: tool.to_owned(),
            phase,
            policy: self.audit_policy,
            refused,
            at_micros,
        });
    }

    async fn call_tool(
        &self,
        principal: &McpPrincipal,
        tool: &ToolSpec,
        arguments: &Json,
        now: OffsetDateTime,
    ) -> Result<CallToolResult, McpError> {
        principal.assert_not_system()?;
        if let Err(e) = spot_functions::validate_arguments(&tool.input_schema, arguments) {
            return Err(McpError::invalid_arguments(e.to_string()));
        }
        if let Some(tier) = principal.tier() {
            let required = required_tier(tool.kind.writes(), tool.kind.async_job());
            if tier < required {
                return Err(McpError::permission_denied(format!(
                    "the tool needs tier {required}, the agent is {tier}"
                )));
            }
        }
        let key = format!("mcp:{}", principal.user());
        match self.coordinator.take(&key, self.limits.rate, 1).await {
            Ok(Admit::Allowed { .. }) => {}
            Ok(Admit::Denied { retry_after }) => {
                let ms = u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX);
                return Err(McpError::rate_limited(ms));
            }
            Err(_) => return Err(McpError::internal("the rate limiter failed")),
        }
        let future = self.execute(principal, tool, arguments, now);
        match tokio::time::timeout(self.limits.call_timeout, future).await {
            Err(_) => Err(McpError::timeout(self.limits.timeout_ms())),
            Ok(result) => result,
        }
    }

    fn resources_list(
        &self,
        principal: &McpPrincipal,
        params: Option<&Json>,
    ) -> Result<Json, McpError> {
        let state = self.state();
        let cursor = parse_cursor(params)?.unwrap_or(0);
        let limit = 100;
        let mut items: Vec<Json> = Vec::new();
        let mut index = 0usize;
        let mut next = None;
        for view in state.registry.effective().doctypes() {
            if view.is_child {
                continue;
            }
            let doc = DocFacts {
                doctype: view.name.as_str().to_owned(),
                created_by: Some(principal.user().to_owned()),
                ..DocFacts::default()
            };
            if !principal.may(&state.engine, Action::Read, &doc).allowed {
                continue;
            }
            if index < cursor {
                index += 1;
                continue;
            }
            if items.len() == limit {
                next = Some(index);
                break;
            }
            items.push(
                crate::protocol::Resource {
                    uri: format!("spot://{}/{}", self.site.as_str(), view.name),
                    name: view.name.as_str().to_owned(),
                    title: Some(view.label.clone()),
                    description: Some(format!("{} documents", view.label)),
                    mime_type: Some("application/json".to_owned()),
                }
                .to_json(),
            );
            index += 1;
        }
        let mut out = serde_json::Map::new();
        out.insert("resultType".to_owned(), Json::String("complete".to_owned()));
        out.insert("resources".to_owned(), Json::Array(items));
        if let Some(next) = next {
            out.insert("nextCursor".to_owned(), Json::String(next.to_string()));
        }
        Ok(Json::Object(out))
    }

    fn resource_templates(
        &self,
        principal: &McpPrincipal,
        _params: Option<&Json>,
    ) -> Result<Json, McpError> {
        let state = self.state();
        let mut items = Vec::new();
        for view in state.registry.effective().doctypes() {
            if view.is_child {
                continue;
            }
            let doc = DocFacts {
                doctype: view.name.as_str().to_owned(),
                created_by: Some(principal.user().to_owned()),
                ..DocFacts::default()
            };
            if !principal.may(&state.engine, Action::Read, &doc).allowed {
                continue;
            }
            items.push(json!({
                "uriTemplate": format!("spot://{}/{}/{{name}}", self.site.as_str(), view.name),
                "name": view.label,
                "mimeType": "application/json",
            }));
        }
        Ok(json!({ "resultType": "complete", "resourceTemplates": items }))
    }

    async fn resources_read(
        &self,
        principal: &McpPrincipal,
        params: Option<&Json>,
        now: OffsetDateTime,
    ) -> Result<Json, McpError> {
        let uri = params
            .and_then(|p| p.get("uri"))
            .and_then(Json::as_str)
            .ok_or_else(|| McpError::invalid_params("`resources/read` needs a uri"))?;
        let (doctype, id) = parse_resource_uri(uri, self.site.as_str())?;
        let state = self.state();
        let def = state
            .registry
            .effective()
            .doctype(&doctype)
            .ok_or_else(|| McpError::not_found("the resource does not exist"))?;
        let text = match id {
            None => serde_json::to_string(&doctype_schema(&def))
                .map_err(|_| McpError::internal("could not encode the schema"))?,
            Some(id) => {
                let inv = self.invocation(principal, now);
                let outcome = self
                    .service
                    .load(&inv, &doctype, id)
                    .await
                    .map_err(|e| from_app_error(&e))?;
                serde_json::to_string(&document_json(
                    &state.engine,
                    principal,
                    &def,
                    &outcome.value,
                ))
                .map_err(|_| McpError::internal("could not encode the document"))?
            }
        };
        Ok(json!({
            "resultType": "complete",
            "contents": [ { "uri": uri, "mimeType": "application/json", "text": text } ],
            "_meta": { "spot/untrusted": true },
        }))
    }

    fn prompts_list(&self, _params: Option<&Json>) -> Json {
        let prompts: Vec<Json> = builtin_prompts().iter().map(Prompt::to_json).collect();
        json!({ "resultType": "complete", "prompts": prompts })
    }

    fn prompts_get(&self, params: Option<&Json>) -> Result<Json, McpError> {
        let name = params
            .and_then(|p| p.get("name"))
            .and_then(Json::as_str)
            .ok_or_else(|| McpError::invalid_params("`prompts/get` needs a name"))?;
        let prompt = builtin_prompts()
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| McpError::not_found("no such prompt"))?;
        let args = params
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let text = match name {
            "explain_document" => format!(
                "Explain the `{}` document `{}` and what it means.",
                args.get("doctype")
                    .and_then(Json::as_str)
                    .unwrap_or("<doctype>"),
                args.get("id").and_then(Json::as_str).unwrap_or("<id>"),
            ),
            _ => format!(
                "Find `{}` records that answer: {}",
                args.get("doctype")
                    .and_then(Json::as_str)
                    .unwrap_or("<doctype>"),
                args.get("question")
                    .and_then(Json::as_str)
                    .unwrap_or("<question>"),
            ),
        };
        Ok(json!({
            "resultType": "complete",
            "description": prompt.description,
            "messages": [ { "role": "user", "content": { "type": "text", "text": text } } ],
        }))
    }
}

fn builtin_prompts() -> Vec<Prompt> {
    vec![
        Prompt {
            name: "explain_document".to_owned(),
            title: Some("Explain a document".to_owned()),
            description: Some("Explain one document and what it means.".to_owned()),
            arguments: vec![
                PromptArgument {
                    name: "doctype".to_owned(),
                    description: Some("The DocType.".to_owned()),
                    required: true,
                },
                PromptArgument {
                    name: "id".to_owned(),
                    description: Some("The document id.".to_owned()),
                    required: true,
                },
            ],
        },
        Prompt {
            name: "find_records".to_owned(),
            title: Some("Find records".to_owned()),
            description: Some("Find records that answer a question.".to_owned()),
            arguments: vec![
                PromptArgument {
                    name: "doctype".to_owned(),
                    description: Some("The DocType.".to_owned()),
                    required: true,
                },
                PromptArgument {
                    name: "question".to_owned(),
                    description: Some("What to find.".to_owned()),
                    required: true,
                },
            ],
        },
    ]
}

fn parse_cursor(params: Option<&Json>) -> Result<Option<usize>, McpError> {
    match params.and_then(|p| p.get("cursor")).and_then(Json::as_str) {
        None => Ok(None),
        Some(text) => text
            .parse::<usize>()
            .map(Some)
            .map_err(|_| McpError::invalid_params("the cursor is malformed")),
    }
}

fn progress_token(params: Option<&Json>) -> Option<Json> {
    params
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get("progressToken"))
        .cloned()
}

fn parse_resource_uri(uri: &str, site: &str) -> Result<(String, Option<RecordId>), McpError> {
    let rest = uri
        .strip_prefix("spot://")
        .ok_or_else(|| McpError::not_found("unknown resource uri"))?;
    let mut parts = rest.splitn(3, '/');
    let tenant = parts.next().unwrap_or_default();
    let doctype = parts
        .next()
        .filter(|d| !d.is_empty())
        .ok_or_else(|| McpError::not_found("the resource uri names no DocType"))?;
    if tenant != site {
        return Err(McpError::not_found("the resource does not exist"));
    }
    let id = match parts.next() {
        None | Some("") => None,
        Some(id) => Some(
            spot_http::api::codec::parse_record_id(id)
                .ok_or_else(|| McpError::not_found("the resource does not exist"))?,
        ),
    };
    Ok((doctype.to_owned(), id))
}

/// The tier a tool needs, exposed for the transport's own checks.
#[must_use]
pub fn tool_required_tier(writes: bool, async_job: bool) -> Tier {
    required_tier(writes, async_job)
}

/// A helper the transport uses to map an error to a JSON-RPC error object.
#[must_use]
pub fn error_json(error: &McpError) -> Json {
    error.to_json()
}
