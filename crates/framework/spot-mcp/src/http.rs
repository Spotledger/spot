//! The Streamable HTTP transport adapter, built on `spot-http`.
//!
//! The MCP Streamable HTTP binding is a single endpoint that accepts `POST` with a JSON-RPC
//! body and answers with one `application/json` message, or — when the request produces more
//! than one message (progress notifications around a result) — a `text/event-stream` carrying
//! them. This adapter is sans-I/O: it maps an [`HttpRequest`] to the [`Server`] and back to an
//! [`HttpResponse`]; mounting it on an axum router is the host's job.
//!
//! The stdio binding is newline-delimited JSON; [`stdio_reply`] frames the core's messages for
//! it. The `Content-Length` framing is offered for the clients that still use it.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use spot_http::{ErrorEnvelope, HttpErrorCode, RequestId};
use time::OffsetDateTime;

use crate::error::McpError;
use crate::jsonrpc::{Framing, encode_content_length, encode_line};
use crate::principal::Credential;
use crate::server::Server;
use crate::{LEGACY_PROTOCOL_VERSION, PROTOCOL_VERSION};

/// An HTTP request, in the parts the MCP transport needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    /// The method (`POST`).
    pub method: String,
    /// The path.
    pub path: String,
    /// The headers, keyed in lower case.
    pub headers: BTreeMap<String, String>,
    /// The raw body.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// A request with no headers.
    #[must_use]
    pub fn new(method: &str, path: &str, body: Vec<u8>) -> Self {
        Self {
            method: method.to_owned(),
            path: path.to_owned(),
            headers: BTreeMap::new(),
            body,
        }
    }

    /// Sets a header (its key is lower-cased).
    #[must_use]
    pub fn with_header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_lowercase(), value.to_owned());
        self
    }
}

/// An HTTP response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The headers, keyed in lower case.
    pub headers: BTreeMap<String, String>,
    /// The raw body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, value: &Json) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_default();
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        headers.insert(
            "mcp-protocol-version".to_owned(),
            PROTOCOL_VERSION.to_owned(),
        );
        Self {
            status,
            headers,
            body,
        }
    }

    fn text(status: u16, content_type: &str, body: String) -> Self {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_owned(), content_type.to_owned());
        headers.insert(
            "mcp-protocol-version".to_owned(),
            PROTOCOL_VERSION.to_owned(),
        );
        Self {
            status,
            headers,
            body: body.into_bytes(),
        }
    }
}

/// Handles one Streamable HTTP request.
pub async fn handle(server: &Server, request: &HttpRequest, now: OffsetDateTime) -> HttpResponse {
    let request_id = RequestId::generate();
    if request.method != "POST" {
        let mut response = transport_error(
            HttpErrorCode::UnknownRoute,
            "the MCP endpoint answers POST only",
            &request_id,
        );
        response.status = 405;
        return response;
    }
    if let Some(version) = request.headers.get("mcp-protocol-version")
        && version != PROTOCOL_VERSION
        && version != LEGACY_PROTOCOL_VERSION
    {
        let error = McpError::unsupported_version(version);
        return HttpResponse::json(400, &error.to_json());
    }
    let Some(credential) = credential_of(request) else {
        return transport_error(
            HttpErrorCode::UnknownSite,
            "a credential is required",
            &request_id,
        );
    };
    let Ok(text) = std::str::from_utf8(&request.body) else {
        return transport_error(
            HttpErrorCode::Internal,
            "the body is not UTF-8",
            &request_id,
        );
    };
    let messages = server.handle_text(text, Some(&credential), now).await;
    match messages.len() {
        0 => {
            let mut response = HttpResponse::text(202, "application/json", String::new());
            response
                .headers
                .insert("x-request-id".to_owned(), request_id.as_str().to_owned());
            response
        }
        1 => {
            let mut response = HttpResponse::json(200, &messages[0]);
            response
                .headers
                .insert("x-request-id".to_owned(), request_id.as_str().to_owned());
            response
        }
        _ => {
            let mut body = String::new();
            for message in &messages {
                let text = serde_json::to_string(message).unwrap_or_default();
                body.push_str("event: message\n");
                body.push_str("data: ");
                body.push_str(&text);
                body.push_str("\n\n");
            }
            let mut response = HttpResponse::text(200, "text/event-stream", body);
            response
                .headers
                .insert("x-request-id".to_owned(), request_id.as_str().to_owned());
            response
        }
    }
}

/// The credential a request presented: `Authorization: Bearer <token>`, or the `X-Api-Key`
/// header carrying `<id>:<secret>`.
#[must_use]
pub fn credential_of(request: &HttpRequest) -> Option<Credential> {
    if let Some(key) = request.headers.get("x-api-key")
        && let Some((id, secret)) = key.split_once(':')
    {
        return Some(Credential::api_key(id, secret));
    }
    let value = request.headers.get("authorization")?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    Some(Credential::bearer(token))
}

/// Frames the core's messages for the stdio transport.
///
/// # Errors
///
/// [`McpError::internal`] when a message cannot be framed.
pub fn stdio_reply(messages: &[Json], framing: Framing) -> Result<Vec<u8>, McpError> {
    let mut out = Vec::new();
    for message in messages {
        match framing {
            Framing::LineDelimited => out.extend_from_slice(encode_line(message)?.as_bytes()),
            Framing::ContentLength => out.extend_from_slice(&encode_content_length(message)?),
        }
    }
    Ok(out)
}

fn transport_error(code: HttpErrorCode, message: &str, request_id: &RequestId) -> HttpResponse {
    let envelope = ErrorEnvelope::new(code, message, Some(request_id));
    let body = serde_json::to_vec(&envelope).unwrap_or_default();
    let mut headers = BTreeMap::new();
    headers.insert("content-type".to_owned(), "application/json".to_owned());
    headers.insert("x-request-id".to_owned(), request_id.as_str().to_owned());
    HttpResponse {
        status: code.status().as_u16(),
        headers,
        body,
    }
}

/// The JSON-RPC error object for an MCP error, as the HTTP adapter would render it.
#[must_use]
pub fn error_body(error: &McpError) -> Json {
    json!({ "jsonrpc": "2.0", "id": Json::Null, "error": error.to_json() })
}
