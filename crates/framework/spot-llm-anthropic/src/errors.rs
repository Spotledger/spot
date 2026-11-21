//! Error mapping: HTTP status plus the Anthropic `error.type` onto the stable `llm.*` codes.
//!
//! `WireError` is the parsed shape of an Anthropic error body (`{"type": "error", "error":
//! {"type": ..., "message": ...}}`); [`map`](map) turns a status, the parsed body and the
//! `retry-after` header into an [`LlmError`]. Details quote at most the provider's own message,
//! and never the key, the auth headers or the request body.

use serde::Deserialize;
use spot_llm::LlmError;

/// The parsed wire error. Unknown `error.type` values map by status; the message is echoed only
/// through [`map`]'s detail-building, which prefixes it with the provider's own error type.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct WireError {
    /// The error envelope type; Anthropic sends `"error"`.
    #[serde(default)]
    pub kind: Option<String>,
    /// The inner error object.
    pub error: InnerError,
}

/// The inner error object: Anthropic's `error.type` and its human message.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct InnerError {
    /// The machine type (`authentication_error`, `rate_limit_error`, ...).
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// The provider's human message.
    #[serde(default)]
    pub message: Option<String>,
}

/// Maps one HTTP failure onto an [`LlmError`].
///
/// `status` is the HTTP status; `body` the response body (parsed as the wire error when it is
/// one); `retry_after` the parsed `retry-after` header, in seconds, when present.
///
/// The detail never repeats the status's standard reason phrase, never quotes a header and
/// never quotes the request body — only Anthropic's own `error.type` and message, as the
/// documented failure surface.
pub fn map(status: u16, body: &str, retry_after: Option<u64>) -> LlmError {
    // Parse the body when it is the documented error envelope; otherwise the detail names the
    // status only.
    let parsed: Option<WireError> = serde_json::from_str(body).ok();
    let (wire_type, wire_message) = match parsed {
        Some(wire) => (wire.error.kind, wire.error.message),
        None => (None, None),
    };

    let context = match (&wire_type, &wire_message) {
        (Some(kind), Some(message)) => format!("anthropic error type '{kind}' said: {message}"),
        (Some(kind), None) => format!("anthropic error type '{kind}'"),
        (None, Some(message)) => format!("the Messages API said: {message}"),
        (None, None) => "the Messages API refused the request".to_owned(),
    };

    // The documented error types decide first; anything else falls back by status. The
    // invalid-request family splits: the context-window overflow is its own llm.* code.
    if wire_type.as_deref() == Some("invalid_request_error") {
        let long = wire_message
            .as_deref()
            .is_some_and(|message| message.contains("context") && message.contains("too long"));
        return if long {
            LlmError::ContextTooLong
        } else {
            LlmError::BadRequest {
                detail: context.clone(),
            }
        };
    }

    let by_type = match wire_type.as_deref() {
        Some("authentication_error") | Some("permission_error") => Some(LlmError::Auth),
        Some("rate_limit_error") => Some(LlmError::RateLimited { retry_after }),
        Some("overloaded_error") => Some(LlmError::Overloaded),
        Some("api_error") | Some("request_too_large") => match status {
            413 => Some(LlmError::BadRequest {
                detail: context.clone(),
            }),
            500 | 502..=504 | 529 => Some(LlmError::Overloaded),
            _ => Some(LlmError::Unavailable),
        },
        Some("timeout_error") => Some(LlmError::Timeout),
        _ => None,
    };
    if let Some(error) = by_type {
        return error;
    }

    // Unknown or missing type: fall back on the HTTP status.
    match status {
        401 | 403 => LlmError::Auth,
        404 | 410 => LlmError::Unavailable,
        408 => LlmError::Timeout,
        409 => LlmError::Unavailable,
        413 | 422 => LlmError::BadRequest { detail: context },
        429 => LlmError::RateLimited {
            retry_after: retry_after.or(Some(0)),
        },
        500 | 502..=504 | 529 => LlmError::Overloaded,
        _ => LlmError::Unavailable,
    }
}

/// Extracts the seconds out of a `retry-after` header value: plain seconds when decimal digits
/// only, `"N.N"` formatted decimals and HTTP dates are ignored (no float parsing, no clock).
pub fn retry_after_seconds(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        // An all-digit field parses by hand; leading zeros are fine ("07" is seven).
        let mut seconds: u64 = 0;
        for byte in value.bytes() {
            let digit = u64::from(byte - b'0');
            seconds = seconds.checked_mul(10)?.checked_add(digit)?;
        }
        return Some(seconds);
    }
    None
}

/// The `retry-after` header's canonical lowercase name.
pub const RETRY_AFTER_HEADER: &str = "retry-after";
