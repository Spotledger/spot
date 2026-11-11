//! Tool generation from effective metadata.
//!
//! [`ToolCatalog::build`] turns the effective metadata, the calling principal's rights and the
//! host's whitelisted functions into the tool list `tools/list` returns. Tool names are
//! stable and deterministic. A write tool is generated only when the principal may write, and
//! a hidden or forbidden field never appears in an input schema. Because the catalog is built
//! from the *effective* metadata on every request, a runtime-created DocType or custom field
//! appears on the next `tools/list` with no restart.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use spot_app_api::lifecycle::Operation;
use spot_app_api::meta::{DocTypeDef, FieldDef};
use spot_functions::{FunctionInfo, FunctionRegistry};
use spot_meta::Registry;
use spot_permissions::{Action, DocFacts, PermissionEngine};

use crate::limits::MAX_ROWS;
use crate::principal::McpPrincipal;
use crate::schema::{field_schema, tool_input_schema};

/// What a tool does when it is called.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolKind {
    /// Search a DocType: `search_<doctype>`.
    Search {
        /// The DocType.
        doctype: String,
    },
    /// Load one document: `get_<doctype>`.
    Get {
        /// The DocType.
        doctype: String,
    },
    /// Create a draft: `create_<doctype>_draft`.
    CreateDraft {
        /// The DocType.
        doctype: String,
    },
    /// Update a draft: `update_<doctype>`.
    Update {
        /// The DocType.
        doctype: String,
    },
    /// Run a read-only query over the storage query IR.
    Query,
    /// Read one job's status.
    JobStatus,
    /// Call a whitelisted function.
    Function {
        /// The function's app.
        app: String,
        /// The function's module.
        module: String,
        /// The function's name.
        name: String,
        /// Whether the call enqueues a job and returns its id.
        async_job: bool,
    },
}

impl ToolKind {
    /// Whether the tool changes anything.
    #[must_use]
    pub fn writes(&self) -> bool {
        matches!(
            self,
            ToolKind::CreateDraft { .. } | ToolKind::Update { .. } | ToolKind::Function { .. }
        )
    }

    /// Whether the tool returns a job id instead of running to completion.
    #[must_use]
    pub fn async_job(&self) -> bool {
        matches!(
            self,
            ToolKind::Function {
                async_job: true,
                ..
            }
        )
    }

    /// The tool's DocType, when it is about one.
    #[must_use]
    pub fn doctype(&self) -> Option<&str> {
        match self {
            ToolKind::Search { doctype }
            | ToolKind::Get { doctype }
            | ToolKind::CreateDraft { doctype }
            | ToolKind::Update { doctype } => Some(doctype),
            _ => None,
        }
    }
}

/// One generated tool.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    /// The stable tool name.
    pub name: String,
    /// A human-readable title.
    pub title: String,
    /// A description derived from the DocType and field labels.
    pub description: String,
    /// The JSON Schema of the arguments.
    pub input_schema: Json,
    /// What the tool does.
    pub kind: ToolKind,
}

impl ToolSpec {
    /// The tool as the wire carries it.
    #[must_use]
    pub fn to_tool(&self) -> crate::protocol::Tool {
        crate::protocol::Tool {
            name: self.name.clone(),
            title: Some(self.title.clone()),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            output_schema: None,
        }
    }
}

/// The tools a principal may see, built from the effective metadata and the function registry.
#[derive(Clone, Debug)]
pub struct ToolCatalog {
    tools: Vec<ToolSpec>,
    index: BTreeMap<String, usize>,
    meta_version: u64,
}

impl ToolCatalog {
    /// Builds the catalog for `principal` from the registry's current effective metadata.
    ///
    /// The catalog is versioned by the metadata it builds the tools from
    /// ([`Registry::effective`]'s version), and never by an input the caller passes: a caller
    /// holding an older [`Customisations`](spot_meta::Customisations) could otherwise stamp the
    /// catalogue with a version that does not describe the tools a client is shown, and that
    /// stale stamp is what a client or a proxy caches past a runtime customisation.
    #[must_use]
    pub fn build(
        registry: &Registry,
        engine: &PermissionEngine,
        principal: &McpPrincipal,
        functions: &FunctionRegistry,
    ) -> ToolCatalog {
        let mut tools = Vec::new();
        let effective = registry.effective();
        for view in effective.doctypes() {
            if view.is_child {
                continue;
            }
            build_doctype_tools(view, engine, principal, &mut tools);
        }
        tools.push(query_tool());
        tools.push(job_status_tool());
        for function in functions.list() {
            tools.push(function_tool(&function));
        }
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        let index = tools
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.clone(), i))
            .collect();
        ToolCatalog {
            tools,
            index,
            meta_version: effective.version(),
        }
    }

    /// Every tool, in stable name order.
    #[must_use]
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    /// The tool with `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.index.get(name).map(|i| &self.tools[*i])
    }

    /// The metadata version the catalog was built at.
    #[must_use]
    pub fn meta_version(&self) -> u64 {
        self.meta_version
    }

    /// A page of the tool list from `cursor`, at most `limit` tools.
    #[must_use]
    pub fn page(&self, cursor: Option<usize>, limit: usize) -> (Vec<&ToolSpec>, Option<usize>) {
        let start = cursor.unwrap_or(0).min(self.tools.len());
        let end = start.saturating_add(limit).min(self.tools.len());
        let page = self.tools[start..end].iter().collect();
        let next = (end < self.tools.len()).then_some(end);
        (page, next)
    }
}

fn probe(principal: &McpPrincipal, doctype: &str) -> DocFacts {
    DocFacts {
        doctype: doctype.to_owned(),
        created_by: Some(principal.user().to_owned()),
        ..DocFacts::default()
    }
}

fn build_doctype_tools(
    view: &DocTypeDef,
    engine: &PermissionEngine,
    principal: &McpPrincipal,
    tools: &mut Vec<ToolSpec>,
) {
    let doctype = view.name.as_str();
    let doc = probe(principal, doctype);
    if !principal.may(engine, Action::Read, &doc).allowed {
        return;
    }
    let readable = engine.readable_fields(principal.permissions(), doctype, &doc);
    let readable_fields: Vec<&FieldDef> = view
        .fields
        .iter()
        .filter(|f| readable.contains(f.name.as_str()))
        .collect();

    tools.push(search_tool(view, engine, principal, &readable_fields));
    tools.push(get_tool(view));

    if principal
        .may(engine, Action::Op(Operation::Create), &doc)
        .allowed
    {
        let writable =
            engine.writable_fields(principal.permissions(), doctype, Operation::Create, &doc);
        let fields: Vec<&FieldDef> = view
            .fields
            .iter()
            .filter(|f| writable.contains(f.name.as_str()))
            .collect();
        tools.push(create_tool(view, &fields));
    }
    if principal
        .may(engine, Action::Op(Operation::Update), &doc)
        .allowed
    {
        let writable =
            engine.writable_fields(principal.permissions(), doctype, Operation::Update, &doc);
        let fields: Vec<&FieldDef> = view
            .fields
            .iter()
            .filter(|f| writable.contains(f.name.as_str()))
            .collect();
        tools.push(update_tool(view, &fields));
    }
}

fn search_tool(
    view: &DocTypeDef,
    engine: &PermissionEngine,
    principal: &McpPrincipal,
    readable: &[&FieldDef],
) -> ToolSpec {
    let mut filter_props = serde_json::Map::new();
    for field in readable {
        if engine.filter_ok(
            principal.permissions(),
            view.name.as_str(),
            field.name.as_str(),
        ) {
            filter_props.insert(field.name.as_str().to_owned(), field_schema(field));
        }
    }
    let input = json!({
        "type": "object",
        "properties": {
            "filter": {
                "type": "object",
                "description": "Equality filters on readable fields",
                "properties": Json::Object(filter_props),
                "additionalProperties": false,
            },
            "order_by": { "type": "string", "description": "a readable field to sort by" },
            "descending": { "type": "boolean" },
            "limit": { "type": "integer", "minimum": 1, "maximum": MAX_ROWS },
            "cursor": { "type": "string" },
            "fields": { "type": "array", "items": { "type": "string" } },
        },
        "additionalProperties": false,
    });
    ToolSpec {
        name: format!("search_{}", view.name),
        title: format!("Search {}", view.label),
        description: format!(
            "Search `{}` documents the caller may read. Only readable fields are returned.",
            view.name
        ),
        input_schema: input,
        kind: ToolKind::Search {
            doctype: view.name.as_str().to_owned(),
        },
    }
}

fn get_tool(view: &DocTypeDef) -> ToolSpec {
    ToolSpec {
        name: format!("get_{}", view.name),
        title: format!("Get {}", view.label),
        description: format!("Load one `{}` document the caller may read.", view.name),
        input_schema: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "format": "record-id" },
                "fields": { "type": "array", "items": { "type": "string" } },
            },
            "required": ["id"],
            "additionalProperties": false,
        }),
        kind: ToolKind::Get {
            doctype: view.name.as_str().to_owned(),
        },
    }
}

fn create_tool(view: &DocTypeDef, fields: &[&FieldDef]) -> ToolSpec {
    ToolSpec {
        name: format!("create_{}_draft", view.name),
        title: format!("Create {} draft", view.label),
        description: format!(
            "Create a draft `{}`. The document is never posted by this tool.",
            view.name
        ),
        input_schema: tool_input_schema(&owned_fields(fields)),
        kind: ToolKind::CreateDraft {
            doctype: view.name.as_str().to_owned(),
        },
    }
}

fn update_tool(view: &DocTypeDef, fields: &[&FieldDef]) -> ToolSpec {
    let mut schema = tool_input_schema(&owned_fields(fields));
    if let Some(object) = schema.as_object_mut() {
        if let Some(Json::Object(props)) = object.get_mut("properties") {
            props.insert(
                "id".to_owned(),
                json!({ "type": "string", "format": "record-id" }),
            );
            props.insert(
                "version".to_owned(),
                json!({ "type": "integer", "minimum": 1 }),
            );
        }
        if let Some(Json::Array(required)) = object.get_mut("required") {
            required.push(Json::String("id".to_owned()));
        }
    }
    ToolSpec {
        name: format!("update_{}", view.name),
        title: format!("Update {}", view.label),
        description: format!(
            "Update a draft `{}` document the caller may write.",
            view.name
        ),
        input_schema: schema,
        kind: ToolKind::Update {
            doctype: view.name.as_str().to_owned(),
        },
    }
}

fn owned_fields(fields: &[&FieldDef]) -> Vec<FieldDef> {
    fields
        .iter()
        .filter(|f| !f.read_only)
        .map(|f| (*f).clone())
        .collect()
}

fn query_tool() -> ToolSpec {
    ToolSpec {
        name: "query".to_owned(),
        title: "Query".to_owned(),
        description: "Run a read-only query over the storage query IR. Row, depth and timeout \
                      limits apply, and row filters and field rules are enforced."
            .to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "doctype": { "type": "string" },
                "select": { "type": "array", "items": { "type": "object" } },
                "filter": { "type": "object" },
                "group_by": { "type": "array", "items": { "type": "object" } },
                "having": { "type": "object" },
                "order_by": { "type": "array", "items": { "type": "object" } },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_ROWS },
                "offset": { "type": "integer", "minimum": 0 },
                "joins": { "type": "array", "items": { "type": "object" } },
            },
            "required": ["doctype", "select"],
            "additionalProperties": false,
        }),
        kind: ToolKind::Query,
    }
}

fn job_status_tool() -> ToolSpec {
    ToolSpec {
        name: "job_status".to_owned(),
        title: "Job status".to_owned(),
        description: "Read the status of a job started by an asynchronous tool.".to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": { "job_id": { "type": "integer", "minimum": 1 } },
            "required": ["job_id"],
            "additionalProperties": false,
        }),
        kind: ToolKind::JobStatus,
    }
}

fn function_tool(function: &FunctionInfo) -> ToolSpec {
    let async_job = function.verbs.is_none();
    let input = function
        .arguments
        .clone()
        .unwrap_or_else(|| json!({ "type": "object" }));
    let name = sanitize(&format!(
        "call_{}_{}_{}",
        function.key.app(),
        function.key.module(),
        function.key.name()
    ));
    ToolSpec {
        name,
        title: format!("Call {}", function.key),
        description: format!(
            "Call the whitelisted function `{}`{}.",
            function.key,
            if async_job {
                " as a background job"
            } else {
                ""
            }
        ),
        input_schema: input,
        kind: ToolKind::Function {
            app: function.key.app().to_owned(),
            module: function.key.module().to_owned(),
            name: function.key.name().to_owned(),
            async_job,
        },
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}
