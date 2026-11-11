//! The native MCP server core (WP-AI-04.T1; blueprint WS13, ADR-26).
//!
//! MCP (the Model Context Protocol) lets any compatible agent — including a person's own
//! Claude — drive Spotledger under that person's own rights. This crate is the server's
//! protocol core, **sans I/O**: it parses messages, dispatches methods and returns messages;
//! the transport (stdio or Streamable HTTP) is a thin adapter, and storage, permissions,
//! functions, jobs and the audit chain are injected by the host.
//!
//! # Protocol revision
//!
//! This crate implements the **`2026-07-28`** revision of the MCP specification (the
//! "modern", stateless era: version, client identity and client capabilities travel in the
//! per-request `_meta` field, and there is no negotiation handshake). It is also a
//! **dual-era** server: it additionally answers the legacy `initialize` / `initialized`
//! handshake and the legacy feature methods, so a client written for `2025-11-25` and earlier
//! interoperates. The revision is [`PROTOCOL_VERSION`]; the legacy revision is
//! [`LEGACY_PROTOCOL_VERSION`].
//!
//! Implemented messages:
//!
//! - the lifecycle: `server/discover` (modern), `initialize` / `initialized` (legacy),
//!   `ping`;
//! - `tools/list` and `tools/call`, with cursor pagination, progress notifications and
//!   cancellation;
//! - `resources/list`, `resources/templates/list` and `resources/read`, documents as
//!   resources by URI `spot://<tenant>/<doctype>/<name>`;
//! - `prompts/list` and `prompts/get` for a few built-in prompts;
//! - the `notifications/*` a server sends (progress, cancellation acknowledgement,
//!   list-changed) and receives (`notifications/cancelled`).
//!
//! Result responses carry `resultType: "complete"`; error responses carry a standard
//! JSON-RPC code plus a stable `mcp.*` code in `data.code`.
//!
//! # Tools from metadata
//!
//! [`ToolCatalog::build`] generates per-DocType tools from the effective metadata for the
//! calling principal: `search_<doctype>`, `get_<doctype>`, `create_<doctype>_draft` and
//! `update_<doctype>` (the write tools only when the principal may write; drafts only, never a
//! lifecycle submit or cancel), the read-only [`query`](crate::query) tool over the storage
//! query IR, the `job_status` tool, and one tool per whitelisted function the host registers.
//! Hidden and forbidden fields are omitted from a tool's input schema for that principal, and
//! a runtime-created DocType appears on the next `tools/list` with no restart.
//!
//! # Running strictly as the calling user
//!
//! Every request carries an authenticated principal (an OAuth bearer or an API key). The
//! server never executes as the system: a tool runs through the same [`DocumentService`] and
//! query seam as the native API, filtered by [`spot_perm_query`] so a tool never returns what
//! the principal cannot read. An API key is narrowed by [`spot_iam_access::KeyScope`], a
//! provable subset of its owner's rights.
//!
//! # Safety
//!
//! Input is size-checked, every principal is rate-limited through the coordinator, a call may
//! not exceed its timeout, document text is returned as untrusted data with a provenance
//! marker, and every call is recorded to the AI audit chain. No tool call holds a database
//! connection or transaction across calls: each call is one service invocation.
//!
//! [`DocumentService`]: spot_app_api::context::DocumentService

#![forbid(unsafe_code)]

pub mod audit;
pub mod catalog;
pub mod error;
pub mod execute;
pub mod http;
pub mod jobs;
pub mod jsonrpc;
pub mod limits;
pub mod metrics;
pub mod principal;
pub mod protocol;
pub mod query;
pub mod schema;
pub mod server;

pub use audit::{
    AuditAlert, AuditAlerts, AuditFailurePolicy, AuditFailures, AuditPhase, AuditSink, ChainSink,
    MemoryAudit, TelemetryAlerts,
};
pub use catalog::{ToolCatalog, ToolKind, ToolSpec};
pub use error::McpError;
pub use jsonrpc::{Framing, Id, Incoming, Notification, Request, parse_message};
pub use limits::Limits;
pub use principal::{Authenticator, Credential, McpPrincipal};
pub use protocol::{
    CallToolResult, Content, Implementation, Prompt, PromptArgument, Resource, ServerCapabilities,
    Tool,
};
pub use query::{QueryPort, ServiceQuery};
pub use schema::{decode_value, encode_value, field_schema, tool_input_schema};
pub use server::{MetaState, MetadataSource, Server};

/// The MCP protocol revision this server implements (`2026-07-28`, the modern era).
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// A boxed, `Send` future: the shape every async seam in this crate uses, so the host's
/// services stay object-safe.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
/// The newest legacy (handshake) revision this dual-era server also answers (`2025-11-25`).
pub const LEGACY_PROTOCOL_VERSION: &str = "2025-11-25";

/// The server's self-reported name (`serverInfo.name`).
pub const SERVER_NAME: &str = "spot";

/// The server's self-reported version.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The URI scheme documents are exposed under: `spot://<tenant>/<doctype>/<name>`.
pub const RESOURCE_SCHEME: &str = "spot";

/// The `_meta` key that carries a request's protocol version.
pub const META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// The `_meta` key that carries a request's client identity.
pub const META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";

/// The `_meta` key that carries a request's client capabilities.
pub const META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";

/// The `_meta` key that carries the server identity in a result.
pub const META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";
