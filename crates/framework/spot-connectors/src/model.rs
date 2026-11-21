//! The connector vocabulary the host speaks: the connector's identity, its operations, the
//! auth kinds it supports, the invocation context and the JSON HTTP request/response pair that
//! crosses the world boundary.
//!
//! These are the Rust side of the `connector` world's types. The generated bindings are
//! converted to and from them at the boundary ([`crate::bindings`]), so the rest of the host
//! depends on these and not on `wasmtime`.

use serde::{Deserialize, Serialize};

/// How a connector authenticates. The host injects the credential for [`AuthKind::Bearer`],
/// [`AuthKind::Basic`] and [`AuthKind::OAuth2`]; the guest never holds the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// No credential.
    None,
    /// `Authorization: Bearer <token>`.
    Bearer,
    /// `Authorization: Basic <base64>`.
    Basic,
    /// `Authorization: Bearer <access-token>`, refreshed by the host.
    OAuth2,
}

impl AuthKind {
    /// Whether the host injects the credential into outbound requests (so the guest must not
    /// read it).
    pub fn is_injected(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// One operation a connector supports. `input_schema` and `output_schema` are JSON Schema
/// documents as text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    /// The operation name the caller passes to `invoke`.
    pub name: String,
    /// The JSON Schema of the operation's input.
    pub input_schema: String,
    /// The JSON Schema of the operation's output.
    pub output_schema: String,
}

/// What `describe` returns: the connector's identity, its operations and its auth kinds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorInfo {
    /// The connector's name.
    pub name: String,
    /// The connector's version.
    pub version: String,
    /// The operations it supports.
    pub operations: Vec<Operation>,
    /// The auth kinds it supports, at least one.
    pub auth: Vec<AuthKind>,
}

impl ConnectorInfo {
    /// Whether the connector declares the named operation.
    pub fn supports(&self, operation: &str) -> bool {
        self.operations.iter().any(|o| o.name == operation)
    }
}

/// The invocation context: who is calling, the invocation's clock and its deadline. The guest
/// has no clock of its own; `now_micros` is the only time it sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallContext {
    /// The tenant the call runs for.
    pub tenant: String,
    /// The actor (user or service), when there is one.
    pub actor: Option<String>,
    /// The invocation's time, in microseconds since the Unix epoch.
    pub now_micros: i64,
    /// The deadline, in microseconds since the Unix epoch.
    pub deadline_micros: i64,
}

/// An outbound HTTP request the guest asks the host to make. `body` is text (JSON for the
/// operations that carry a body).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    /// The HTTP method, upper-case.
    pub method: String,
    /// The absolute URL.
    pub url: String,
    /// The request headers, in order.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// The request body, if any.
    #[serde(default)]
    pub body: Option<String>,
}

/// The response to a guest's outbound HTTP request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The response headers, in order.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// The response body as text.
    #[serde(default)]
    pub body: String,
}

/// The stable identity of an installed connector (its bundle's app id).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnectorId(String);

impl ConnectorId {
    /// Wraps `id`. An empty id is refused.
    pub fn new(id: impl Into<String>) -> Option<Self> {
        let id = id.into();
        (!id.is_empty()).then_some(Self(id))
    }

    /// The id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConnectorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
