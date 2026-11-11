//! The MCP message shapes (revision `2026-07-28`, plus the legacy handshake).
//!
//! Only the structures this server produces or consumes are modelled. Results carry
//! `resultType: "complete"`; the legacy handshake's results omit it, as legacy clients do not
//! know the field.

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

/// A client or server name and version (`Implementation`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Implementation {
    /// The implementation's name.
    pub name: String,
    /// The implementation's version.
    pub version: String,
}

/// The capabilities a server declares.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// Whether the server exposes tools.
    pub tools: bool,
    /// Whether the server exposes resources.
    pub resources: bool,
    /// Whether the server exposes prompts.
    pub prompts: bool,
}

impl ServerCapabilities {
    /// The capabilities object as the wire carries it.
    #[must_use]
    pub fn to_json(&self) -> Json {
        let mut out = serde_json::Map::new();
        if self.tools {
            out.insert("tools".to_owned(), json!({ "listChanged": false }));
        }
        if self.resources {
            out.insert("resources".to_owned(), json!({ "listChanged": false }));
        }
        if self.prompts {
            out.insert("prompts".to_owned(), json!({ "listChanged": false }));
        }
        Json::Object(out)
    }
}

/// One tool a client may call.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    /// The unique tool name.
    pub name: String,
    /// A human-readable title.
    pub title: Option<String>,
    /// A human-readable description.
    pub description: String,
    /// The JSON Schema of the tool's arguments.
    pub input_schema: Json,
    /// The JSON Schema of the tool's structured result, when it has one.
    pub output_schema: Option<Json>,
}

impl Tool {
    /// The tool as the wire carries it.
    #[must_use]
    pub fn to_json(&self) -> Json {
        let mut out = serde_json::Map::new();
        out.insert("name".to_owned(), Json::String(self.name.clone()));
        if let Some(title) = &self.title {
            out.insert("title".to_owned(), Json::String(title.clone()));
        }
        out.insert(
            "description".to_owned(),
            Json::String(self.description.clone()),
        );
        out.insert("inputSchema".to_owned(), self.input_schema.clone());
        if let Some(schema) = &self.output_schema {
            out.insert("outputSchema".to_owned(), schema.clone());
        }
        Json::Object(out)
    }
}

/// One resource (a document) a client may read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    /// The resource URI: `spot://<tenant>/<doctype>/<name>`.
    pub uri: String,
    /// A short name.
    pub name: String,
    /// A human-readable title.
    pub title: Option<String>,
    /// A description.
    pub description: Option<String>,
    /// The MIME type.
    pub mime_type: Option<String>,
}

impl Resource {
    /// The resource as the wire carries it.
    #[must_use]
    pub fn to_json(&self) -> Json {
        let mut out = serde_json::Map::new();
        out.insert("uri".to_owned(), Json::String(self.uri.clone()));
        out.insert("name".to_owned(), Json::String(self.name.clone()));
        if let Some(title) = &self.title {
            out.insert("title".to_owned(), Json::String(title.clone()));
        }
        if let Some(description) = &self.description {
            out.insert("description".to_owned(), Json::String(description.clone()));
        }
        if let Some(mime) = &self.mime_type {
            out.insert("mimeType".to_owned(), Json::String(mime.clone()));
        }
        Json::Object(out)
    }
}

/// One prompt argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptArgument {
    /// The argument name.
    pub name: String,
    /// A description.
    pub description: Option<String>,
    /// Whether the argument is required.
    pub required: bool,
}

/// One prompt a client may get.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// The prompt name.
    pub name: String,
    /// A human-readable title.
    pub title: Option<String>,
    /// A description.
    pub description: Option<String>,
    /// The prompt's arguments.
    pub arguments: Vec<PromptArgument>,
}

impl Prompt {
    /// The prompt as `prompts/list` carries it.
    #[must_use]
    pub fn to_json(&self) -> Json {
        let args: Vec<Json> = self
            .arguments
            .iter()
            .map(|a| {
                let mut m = serde_json::Map::new();
                m.insert("name".to_owned(), Json::String(a.name.clone()));
                if let Some(d) = &a.description {
                    m.insert("description".to_owned(), Json::String(d.clone()));
                }
                if a.required {
                    m.insert("required".to_owned(), Json::Bool(true));
                }
                Json::Object(m)
            })
            .collect();
        let mut out = serde_json::Map::new();
        out.insert("name".to_owned(), Json::String(self.name.clone()));
        if let Some(title) = &self.title {
            out.insert("title".to_owned(), Json::String(title.clone()));
        }
        if let Some(d) = &self.description {
            out.insert("description".to_owned(), Json::String(d.clone()));
        }
        out.insert("arguments".to_owned(), Json::Array(args));
        Json::Object(out)
    }
}

/// One content block in a tool result or prompt message.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    /// Text.
    Text(String),
    /// A resource link.
    ResourceLink {
        /// The resource URI.
        uri: String,
        /// The resource name.
        name: String,
        /// The MIME type.
        mime_type: Option<String>,
    },
    /// An embedded resource.
    Resource {
        /// The resource URI.
        uri: String,
        /// The MIME type.
        mime_type: String,
        /// The resource text.
        text: String,
    },
}

impl Content {
    /// The content block as the wire carries it.
    #[must_use]
    pub fn to_json(&self) -> Json {
        match self {
            Content::Text(text) => json!({ "type": "text", "text": text }),
            Content::ResourceLink {
                uri,
                name,
                mime_type,
            } => {
                let mut m = serde_json::Map::new();
                m.insert("type".to_owned(), Json::String("resource_link".to_owned()));
                m.insert("uri".to_owned(), Json::String(uri.clone()));
                m.insert("name".to_owned(), Json::String(name.clone()));
                if let Some(mime) = mime_type {
                    m.insert("mimeType".to_owned(), Json::String(mime.clone()));
                }
                Json::Object(m)
            }
            Content::Resource {
                uri,
                mime_type,
                text,
            } => json!({
                "type": "resource",
                "resource": { "uri": uri, "mimeType": mime_type, "text": text },
            }),
        }
    }
}

/// A tool call's result.
#[derive(Clone, Debug, PartialEq)]
pub struct CallToolResult {
    /// The unstructured content blocks.
    pub content: Vec<Content>,
    /// The structured content, when the tool has an output schema.
    pub structured: Option<Json>,
    /// Whether the call failed as a tool execution error (not a protocol error).
    pub is_error: bool,
    /// Whether the result's content carries untrusted document text.
    pub untrusted: bool,
}

impl CallToolResult {
    /// A successful text result.
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text(text.into())],
            structured: None,
            is_error: false,
            untrusted: false,
        }
    }

    /// A tool execution error: actionable feedback the model may act on.
    #[must_use]
    pub fn tool_error(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text(text.into())],
            structured: None,
            is_error: true,
            untrusted: false,
        }
    }

    /// Marks the result as carrying untrusted document text.
    #[must_use]
    pub fn untrusted(mut self) -> Self {
        self.untrusted = true;
        self
    }

    /// The result as the wire carries it.
    #[must_use]
    pub fn to_json(&self) -> Json {
        let content: Vec<Json> = self.content.iter().map(Content::to_json).collect();
        let mut out = serde_json::Map::new();
        out.insert("resultType".to_owned(), Json::String("complete".to_owned()));
        out.insert("content".to_owned(), Json::Array(content));
        out.insert("isError".to_owned(), Json::Bool(self.is_error));
        if let Some(structured) = &self.structured {
            out.insert("structuredContent".to_owned(), structured.clone());
        }
        if self.untrusted {
            out.insert("_meta".to_owned(), json!({ "spot/untrusted": true }));
        }
        Json::Object(out)
    }
}

/// The `_meta` a request carries (modern era).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestMeta {
    /// The protocol version the request declares.
    pub protocol_version: Option<String>,
    /// The client identity.
    pub client_info: Option<Implementation>,
    /// Whether the client declared any capabilities at all.
    pub has_client_capabilities: bool,
}

impl RequestMeta {
    /// Reads `_meta` from a request's params.
    #[must_use]
    pub fn from_params(params: Option<&Json>) -> RequestMeta {
        let Some(meta) = params.and_then(|p| p.get("_meta")) else {
            return RequestMeta::default();
        };
        let protocol_version = meta
            .get(crate::META_PROTOCOL_VERSION)
            .and_then(Json::as_str)
            .map(str::to_owned);
        let client_info = meta
            .get(crate::META_CLIENT_INFO)
            .and_then(|v| serde_json::from_value::<Implementation>(v.clone()).ok());
        let has_client_capabilities = meta
            .get(crate::META_CLIENT_CAPABILITIES)
            .is_some_and(Json::is_object);
        RequestMeta {
            protocol_version,
            client_info,
            has_client_capabilities,
        }
    }
}
