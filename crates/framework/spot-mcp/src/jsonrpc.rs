//! JSON-RPC 2.0 framing: the message types, the parser and the two transports' framings.
//!
//! MCP messages are JSON-RPC 2.0 objects. A request carries a non-null `id`; a notification
//! carries none; a response carries `result` or `error`. Batching is not part of MCP and a
//! top-level array is refused.

use serde_json::{Value as Json, json};

use crate::error::McpError;

/// A JSON-RPC request id: a string or an integer, never null.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Id {
    /// An integer id.
    Number(i64),
    /// A string id.
    String(String),
}

impl Id {
    /// The id as JSON.
    #[must_use]
    pub fn to_json(&self) -> Json {
        match self {
            Id::Number(n) => Json::from(*n),
            Id::String(s) => Json::String(s.clone()),
        }
    }

    /// Reads an id from JSON; `None` for null or any other shape.
    #[must_use]
    pub fn from_json(value: &Json) -> Option<Id> {
        match value {
            Json::Number(n) => n.as_i64().map(Id::Number),
            Json::String(s) => Some(Id::String(s.clone())),
            _ => None,
        }
    }
}

/// A JSON-RPC request.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The request id.
    pub id: Id,
    /// The method name.
    pub method: String,
    /// The parameters, when present.
    pub params: Option<Json>,
}

/// A JSON-RPC notification.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    /// The method name.
    pub method: String,
    /// The parameters, when present.
    pub params: Option<Json>,
}

/// One incoming JSON-RPC message.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    /// A request awaiting a response.
    Request(Request),
    /// A notification awaiting none.
    Notification(Notification),
}

/// Parses one JSON text into an [`Incoming`] message.
///
/// # Errors
///
/// [`McpError::parse`] for invalid JSON, [`McpError::invalid_request`] for a well-formed value
/// that is not a JSON-RPC request or notification.
pub fn parse_message(text: &str) -> Result<Incoming, McpError> {
    let value: Json = serde_json::from_str(text).map_err(|e| McpError::parse(e.to_string()))?;
    parse_value(&value)
}

/// Parses one already-decoded JSON value.
///
/// # Errors
///
/// [`McpError::invalid_request`] for anything that is not a JSON-RPC request or notification.
pub fn parse_value(value: &Json) -> Result<Incoming, McpError> {
    let obj = value
        .as_object()
        .ok_or_else(|| McpError::invalid_request("a message must be a JSON object"))?;
    if obj.get("jsonrpc").and_then(Json::as_str) != Some("2.0") {
        return Err(McpError::invalid_request("`jsonrpc` must be \"2.0\""));
    }
    let method = obj
        .get("method")
        .and_then(Json::as_str)
        .ok_or_else(|| McpError::invalid_request("a message must carry a `method`"))?
        .to_owned();
    let params = obj.get("params").cloned();
    match obj.get("id") {
        None | Some(Json::Null) => Ok(Incoming::Notification(Notification { method, params })),
        Some(id) => {
            let id = Id::from_json(id)
                .ok_or_else(|| McpError::invalid_request("`id` must be a string or an integer"))?;
            Ok(Incoming::Request(Request { id, method, params }))
        }
    }
}

/// A successful JSON-RPC result response.
#[must_use]
pub fn result_response(id: &Id, result: Json) -> Json {
    json!({ "jsonrpc": "2.0", "id": id.to_json(), "result": result })
}

/// A JSON-RPC error response.
#[must_use]
pub fn error_response(id: &Id, error: &McpError) -> Json {
    json!({ "jsonrpc": "2.0", "id": id.to_json(), "error": error.to_json() })
}

/// A JSON-RPC error response for a message whose id could not be read.
#[must_use]
pub fn error_response_no_id(error: &McpError) -> Json {
    json!({ "jsonrpc": "2.0", "id": Json::Null, "error": error.to_json() })
}

/// A server-to-client notification.
#[must_use]
pub fn notification(method: &str, params: Option<Json>) -> Json {
    match params {
        Some(params) => json!({ "jsonrpc": "2.0", "method": method, "params": params }),
        None => json!({ "jsonrpc": "2.0", "method": method }),
    }
}

/// The framing a transport uses for a stream of messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Newline-delimited JSON, the MCP stdio framing: one message per line, no embedded
    /// newlines.
    LineDelimited,
    /// `Content-Length`-prefixed frames, the language-server framing some clients still use.
    ContentLength,
}

/// Encodes one message for a newline-delimited stream.
///
/// # Errors
///
/// [`McpError::internal`] when the message's text contains a newline.
pub fn encode_line(message: &Json) -> Result<String, McpError> {
    let text = serde_json::to_string(message)
        .map_err(|e| McpError::internal(format!("cannot encode a message: {e}")))?;
    if text.contains('\n') {
        return Err(McpError::internal("a message may not contain a newline"));
    }
    Ok(format!("{text}\n"))
}

/// Decodes one line of a newline-delimited stream.
///
/// # Errors
///
/// The same as [`parse_message`].
pub fn decode_line(line: &str) -> Result<Incoming, McpError> {
    parse_message(line.trim())
}

/// Encodes one message for a `Content-Length` stream.
///
/// # Errors
///
/// [`McpError::internal`] when the message cannot be serialised.
pub fn encode_content_length(message: &Json) -> Result<Vec<u8>, McpError> {
    let body = serde_json::to_vec(message)
        .map_err(|e| McpError::internal(format!("cannot encode a message: {e}")))?;
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decodes one `Content-Length` frame from the front of `input`.
///
/// Returns the message (when a whole frame is present) and the number of bytes consumed. When
/// no complete frame is present yet, the message is `None` and the consumed count is 0.
///
/// # Errors
///
/// [`McpError::parse`] when the header or the body is malformed.
pub fn decode_content_length(input: &[u8]) -> Result<(Option<Incoming>, usize), McpError> {
    let header_end = input
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| McpError::parse("the frame header is incomplete"))?;
    let header = std::str::from_utf8(&input[..header_end])
        .map_err(|_| McpError::parse("the frame header is not UTF-8"))?;
    let mut length: Option<usize> = None;
    for line in header.split("\r\n") {
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            let n = rest
                .trim()
                .parse::<usize>()
                .map_err(|_| McpError::parse("`Content-Length` is not a number"))?;
            length = Some(n);
        }
    }
    let length = length.ok_or_else(|| McpError::parse("the frame has no `Content-Length`"))?;
    let start = header_end + 4;
    if input.len() < start + length {
        return Ok((None, 0));
    }
    let body = &input[start..start + length];
    let text =
        std::str::from_utf8(body).map_err(|_| McpError::parse("the frame body is not UTF-8"))?;
    Ok((Some(parse_message(text)?), start + length))
}
