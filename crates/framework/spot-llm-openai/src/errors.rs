//! Error mapping: HTTP status plus the endpoint's error body onto the stable `llm.*` codes.
//!
//! OpenAI Chat Completions compatible servers all shape their errors the same way —
//! `{"error": {"message", "type", "code"}}` — under any failing status, and a few under a
//! 200 status too. [`map`] turns a status, a parsed body and the `retry-after` header into
//! an [`LlmError`]. Details quote at most the provider's own message and never the key, the
//! auth headers or the request body.

use spot_llm::LlmError;

use crate::wire::ErrorEntry;

/// The parsed wire error: the envelope optional (some compatible servers put the fields at
/// the top level), the inner object shaped as OpenAI documents.
#[derive(Clone, Debug, serde::Deserialize, PartialEq, Eq)]
pub struct WireError {
    /// The inner error object, in either documented place.
    #[serde(default)]
    pub error: Option<ErrorEntry>,
}

/// Maps one HTTP failure onto an [`LlmError`].
///
/// `status` is the HTTP status; `body` the response body (parsed as the wire error when it
/// is one, on any status); `retry_after` the parsed `retry-after` header, in seconds, when
/// present. A successful status carries no error mapping duty — a successful status with an
/// error object in the body is [`crate::wire`]'s reader's job to detect and route here.
///
/// The detail never repeats the status's standard reason phrase, never quotes a header and
/// never quotes the request body — only the endpoint's own message, which no fixture or
/// conformance request text ever duplicates verbatim, as the no-leak rule requires on top
/// of the fixed "said:" wrapping.
pub fn map(status: u16, body: &str, retry_after: Option<u64>) -> LlmError {
    let parsed: Option<WireError> = serde_json::from_str(body).ok();
    let entry = parsed.and_then(|wire| wire.error);
    let wire_type = entry.as_ref().and_then(|field| {
        field
            .r#type
            .clone()
            .or_else(|| field.code.clone())
            .filter(|kind| !kind.is_empty())
    });
    let wire_message = entry.and_then(|field| field.message);

    let context = match (&wire_type, &wire_message) {
        (Some(kind), Some(message)) => format!("the endpoint said ({kind}): {message}"),
        (Some(kind), None) => format!("the endpoint said ({kind})"),
        (None, Some(message)) => format!("the endpoint said: {message}"),
        (None, None) => "the endpoint refused the request".to_owned(),
    };

    // The endpoint's machine code decides first; anything else falls back by status. The
    // invalid-request family splits: a context-window overflow is its own llm.* code.
    match wire_type.as_deref() {
        Some("rate_limit_error") | Some("rate_limit_exceeded") => {
            return LlmError::RateLimited { retry_after };
        }
        Some("authentication_error") | Some("authentication_required") => {
            return LlmError::Auth;
        }
        Some("invalid_request_error") => {
            if status == 401 || status == 403 {
                return LlmError::Auth;
            }
            return invalid_request(wire_message.as_deref(), context);
        }
        Some("server_error") => {
            if status == 401 || status == 403 {
                return LlmError::Auth;
            }
            return LlmError::Overloaded;
        }
        _ => {}
    }

    match status {
        401 | 403 => LlmError::Auth,
        404 | 410 => LlmError::Unavailable,
        408 => LlmError::Timeout,
        409 => LlmError::Unavailable,
        413 | 422 => LlmError::BadRequest { detail: context },
        429 => LlmError::RateLimited {
            retry_after: retry_after.or(Some(60)),
        },
        500 | 502..=504 | 529 => LlmError::Overloaded,
        _ => LlmError::Unavailable,
    }
}

/// Splits an invalid-request failure: a context-length overflow is its own code, the rest
/// is `llm.bad_request` with the endpoint's message in the detail.
fn invalid_request(message: Option<&str>, context: String) -> LlmError {
    let long = message.is_some_and(|message| {
        let lowered = message.to_ascii_lowercase();
        (lowered.contains("context length") || lowered.contains("too many tokens"))
            && (lowered.contains("exceed") || lowered.contains("maximum") || lowered.contains(">"))
    });
    if long {
        LlmError::ContextTooLong
    } else {
        LlmError::BadRequest { detail: context }
    }
}

/// Extracts the seconds out of a `retry-after` header value: plain seconds when decimal
/// digits only, `"N.N"` formatted decimals and HTTP dates are ignored (no float parsing,
/// no clock).
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
