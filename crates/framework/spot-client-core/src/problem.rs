//! Problem details: the contract's `ErrorEnvelope`, parsed without ever panicking.

use serde::Serialize;
use serde_json::Value;

/// A parsed error body (`contracts/native-api`, `ErrorEnvelope`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    /// The stable dotted code, for example `document.stale_version`.
    pub code: String,
    /// Short plain-language message.
    pub message: String,
    /// True when the same request may succeed later unchanged.
    pub retryable: bool,
    /// The server's request id, when it sent one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// `details.current_version` of a `document.stale_version`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<u64>,
    /// Per-field problems of a validation failure, as `(field, code)` pairs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<(String, String)>,
}

impl Problem {
    /// A problem raised by the core itself, not by the server.
    pub(crate) fn local(code: &str, message: &str, retryable: bool) -> Self {
        Self {
            code: code.to_owned(),
            message: message.to_owned(),
            retryable,
            request_id: None,
            current_version: None,
            fields: Vec::new(),
        }
    }

    /// Parses an error body. A body that is not an `ErrorEnvelope` becomes
    /// `client.unparseable_error`, keeping the status in the message.
    pub(crate) fn parse(status: u16, body: &str) -> Self {
        let parsed: Option<Value> = serde_json::from_str(body).ok();
        let Some(error) = parsed.as_ref().and_then(|v| v.get("error")) else {
            return Self::local(
                "client.unparseable_error",
                &format!("The server answered {status} without a readable error body."),
                false,
            );
        };
        let text = |key: &str| error.get(key).and_then(Value::as_str).map(str::to_owned);
        let fields = error
            .get("fields")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        let field = row.get("field")?.as_str()?;
                        let code = row.get("code")?.as_str()?;
                        Some((field.to_owned(), code.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            code: text("code").unwrap_or_else(|| "client.unparseable_error".to_owned()),
            message: text("message").unwrap_or_default(),
            retryable: error
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            request_id: text("request_id"),
            current_version: error
                .get("details")
                .and_then(|d| d.get("current_version"))
                .and_then(Value::as_u64),
            fields,
        }
    }
}
