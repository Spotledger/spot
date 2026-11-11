//! The one error type, with stable `mcp.*` codes and JSON-RPC mapping.

use serde_json::{Value as Json, json};

/// JSON-RPC 2.0 parse error.
pub const PARSE_ERROR: i32 = -32700;
/// JSON-RPC 2.0 invalid request.
pub const INVALID_REQUEST: i32 = -32600;
/// JSON-RPC 2.0 method not found.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC 2.0 invalid params. Also MCP's resource-not-found code since `2026-07-28`.
pub const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC 2.0 internal error.
pub const INTERNAL_ERROR: i32 = -32603;
/// MCP: a request needs a client capability the client did not declare.
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i32 = -32021;
/// MCP: the requested protocol version is not supported.
pub const UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

/// A stable `mcp.*` code raised by this crate.
pub mod codes {
    /// A message was not valid JSON or was not a JSON-RPC object.
    pub const PARSE: &str = "mcp.parse";
    /// A well-formed JSON-RPC object that violates the protocol shape.
    pub const INVALID_REQUEST: &str = "mcp.invalid_request";
    /// The method is not implemented.
    pub const METHOD_NOT_FOUND: &str = "mcp.method_not_found";
    /// The method's parameters are missing or malformed.
    pub const INVALID_PARAMS: &str = "mcp.invalid_params";
    /// An internal error.
    pub const INTERNAL: &str = "mcp.internal";
    /// The requested protocol version is not supported.
    pub const UNSUPPORTED_PROTOCOL_VERSION: &str = "mcp.unsupported_protocol_version";
    /// A request needs a client capability the client did not declare.
    pub const MISSING_CAPABILITY: &str = "mcp.missing_capability";
    /// The request carried no usable credential.
    pub const UNAUTHORIZED: &str = "mcp.unauthorized";
    /// The principal is not allowed to do this.
    pub const PERMISSION_DENIED: &str = "mcp.permission_denied";
    /// The named document or resource does not exist (or is invisible to the caller).
    pub const NOT_FOUND: &str = "mcp.not_found";
    /// The named tool does not exist.
    pub const UNKNOWN_TOOL: &str = "mcp.unknown_tool";
    /// The tool arguments are missing or malformed.
    pub const INVALID_ARGUMENTS: &str = "mcp.invalid_arguments";
    /// A configured limit (input size, rows, depth) was exceeded.
    pub const LIMIT_EXCEEDED: &str = "mcp.limit_exceeded";
    /// The principal has spent its rate limit.
    pub const RATE_LIMITED: &str = "mcp.rate_limited";
    /// The call exceeded its per-call timeout.
    pub const TIMEOUT: &str = "mcp.timeout";
    /// The call was cancelled by the client.
    pub const CANCELLED: &str = "mcp.cancelled";
    /// The named job does not exist.
    pub const JOB_NOT_FOUND: &str = "mcp.job_not_found";
    /// The call was refused because its audit record could not be written.
    pub const AUDIT_UNAVAILABLE: &str = "mcp.audit_unavailable";
    /// The requested feature is not available on this server.
    pub const NOT_SUPPORTED: &str = "mcp.not_supported";
}

/// An MCP error: a JSON-RPC code, a stable `mcp.*` code and a message.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct McpError {
    /// The JSON-RPC error code.
    pub code: i32,
    /// The stable machine-readable `mcp.*` code.
    pub kind: &'static str,
    /// A plain-language message that names no secret and no raw data.
    pub message: String,
    /// Code-specific data.
    pub data: Option<Json>,
}

impl McpError {
    /// An error with a JSON-RPC code, a stable kind and a message.
    #[must_use]
    pub fn new(code: i32, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            kind,
            message: message.into(),
            data: None,
        }
    }

    /// A parse error.
    #[must_use]
    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(PARSE_ERROR, codes::PARSE, message)
    }

    /// An invalid-request error.
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, codes::INVALID_REQUEST, message)
    }

    /// A method-not-found error.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(
            METHOD_NOT_FOUND,
            codes::METHOD_NOT_FOUND,
            format!("the method `{method}` is not implemented"),
        )
    }

    /// An invalid-params error.
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, codes::INVALID_PARAMS, message)
    }

    /// An internal error.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR, codes::INTERNAL, message)
    }

    /// An unsupported protocol version, naming the versions this server supports.
    #[must_use]
    pub fn unsupported_version(requested: &str) -> Self {
        Self::new(
            UNSUPPORTED_PROTOCOL_VERSION,
            codes::UNSUPPORTED_PROTOCOL_VERSION,
            "the requested protocol version is not supported",
        )
        .with_data(json!({
            "requested": requested,
            "supported": [crate::PROTOCOL_VERSION, crate::LEGACY_PROTOCOL_VERSION],
        }))
    }

    /// A missing client capability, listing what is required.
    #[must_use]
    pub fn missing_capability(required: &[&str]) -> Self {
        Self::new(
            MISSING_REQUIRED_CLIENT_CAPABILITY,
            codes::MISSING_CAPABILITY,
            "the request needs a client capability that was not declared",
        )
        .with_data(json!({ "requiredCapabilities": required }))
    }

    /// The request carried no usable credential.
    #[must_use]
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, codes::UNAUTHORIZED, message)
    }

    /// The principal is not allowed to do this.
    #[must_use]
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, codes::PERMISSION_DENIED, message)
    }

    /// The named thing does not exist (or is invisible to the caller).
    #[must_use]
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, codes::NOT_FOUND, message)
    }

    /// The named tool does not exist.
    #[must_use]
    pub fn unknown_tool(name: &str) -> Self {
        Self::new(
            INVALID_PARAMS,
            codes::UNKNOWN_TOOL,
            format!("unknown tool: {name}"),
        )
        .with_data(json!({ "tool": name }))
    }

    /// The tool arguments are missing or malformed.
    #[must_use]
    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, codes::INVALID_ARGUMENTS, message)
    }

    /// A configured limit was exceeded.
    #[must_use]
    pub fn limit_exceeded(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, codes::LIMIT_EXCEEDED, message)
    }

    /// The principal has spent its rate limit.
    #[must_use]
    pub fn rate_limited(retry_after_ms: u64) -> Self {
        Self::new(
            INVALID_PARAMS,
            codes::RATE_LIMITED,
            "the rate limit for this principal is spent; retry later",
        )
        .with_data(json!({ "retry_after_ms": retry_after_ms }))
    }

    /// The call exceeded its timeout.
    #[must_use]
    pub fn timeout(limit_ms: u64) -> Self {
        Self::new(
            INTERNAL_ERROR,
            codes::TIMEOUT,
            "the call took longer than the server allows",
        )
        .with_data(json!({ "timeout_ms": limit_ms }))
    }

    /// The call was cancelled.
    #[must_use]
    pub fn cancelled() -> Self {
        Self::new(INVALID_PARAMS, codes::CANCELLED, "the call was cancelled")
    }

    /// The named job does not exist.
    #[must_use]
    pub fn job_not_found(id: u64) -> Self {
        Self::new(INVALID_PARAMS, codes::JOB_NOT_FOUND, format!("no job {id}"))
    }

    /// The requested feature is not available.
    #[must_use]
    pub fn not_supported(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, codes::NOT_SUPPORTED, message)
    }

    /// The call was refused because the audit chain could not record it. The message never
    /// names the arguments.
    #[must_use]
    pub fn audit_unavailable() -> Self {
        Self::new(
            INTERNAL_ERROR,
            codes::AUDIT_UNAVAILABLE,
            "the call was refused because it could not be recorded to the audit chain",
        )
    }

    /// Attaches code-specific data.
    #[must_use]
    pub fn with_data(mut self, data: Json) -> Self {
        self.data = Some(data);
        self
    }

    /// The JSON-RPC `error` object.
    #[must_use]
    pub fn to_json(&self) -> Json {
        let mut data = match &self.data {
            Some(Json::Object(map)) => map.clone(),
            Some(other) => {
                let mut map = serde_json::Map::new();
                map.insert("detail".to_owned(), other.clone());
                map
            }
            None => serde_json::Map::new(),
        };
        data.insert("code".to_owned(), Json::String(self.kind.to_owned()));
        json!({ "code": self.code, "message": self.message, "data": Json::Object(data) })
    }
}

/// Maps a `spot-app-api` error to an MCP error without leaking document content.
#[must_use]
pub fn from_app_error(e: &spot_app_api::error::AppError) -> McpError {
    use spot_app_api::error::ErrorClass;
    match e.class {
        ErrorClass::NotFound => McpError::not_found("the document does not exist"),
        ErrorClass::PermissionDenied => McpError::permission_denied("the caller may not do this"),
        ErrorClass::Validation => McpError::invalid_arguments("the document did not validate"),
        ErrorClass::StaleVersion => McpError::new(
            INVALID_PARAMS,
            codes::INVALID_REQUEST,
            "the document changed",
        ),
        _ => McpError::internal("the operation failed"),
    }
}
