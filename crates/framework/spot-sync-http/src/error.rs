//! Stable error codes for the sync transport binding: the `sync_http.*` family of the native API
//! error registry, and the app-facing error envelope.
//!
//! The state machines (WP-EDG-02.T1) speak `sync_protocol.*`; this crate is the app-facing
//! surface, so it maps them onto `sync_http.*` codes with a fixed HTTP status and a `retryable` flag
//! (ADR-055, ADR-074). A code's status never changes within a major version.
//!
//! Every code here is catalogued in `crates/framework/spot-errors/catalogue/sync_http.toml`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

/// No client certificate was presented, or the connection is not mutual TLS.
pub const IDENTITY_REQUIRED: &str = "sync_http.identity_required";

/// The presented client certificate is not in the node directory; the node is not enrolled.
pub const IDENTITY_UNKNOWN: &str = "sync_http.identity_unknown";

/// The presented client certificate was revoked; its node may no longer sync.
pub const IDENTITY_REVOKED: &str = "sync_http.identity_revoked";

/// A batch names a source partition the authenticated node does not own (ADR-062 G1): the
/// receiver refuses it before decoding any record beyond the header.
pub const PARTITION_VIOLATION: &str = "sync_http.partition_violation";

/// A batch is larger than the flow-control limit (too many records, too many bytes, or a
/// record payload above the maximum).
pub const BATCH_TOO_LARGE: &str = "sync_http.batch_too_large";

/// The two peers' protocol version ranges do not overlap, so no version is common to them.
pub const VERSION_MISMATCH: &str = "sync_http.version_mismatch";

/// The two peers' metadata epochs differ beyond the allowed skew (ADR-066).
pub const METADATA_EPOCH: &str = "sync_http.metadata_epoch";

/// A streamed batch failed its checksum or its framing, so the receiver refused it before
/// reading a record.
pub const CORRUPT_BATCH: &str = "sync_http.corrupt_batch";

/// A request is malformed: a bad query parameter, an unreadable body, or a missing framing line.
pub const INVALID_REQUEST: &str = "sync_http.invalid_request";

/// A session's deadline passed before its step completed.
pub const DEADLINE_EXCEEDED: &str = "sync_http.deadline_exceeded";

/// The client's circuit breaker for the hub is open; a probe is allowed later.
pub const CIRCUIT_OPEN: &str = "sync_http.circuit_open";

/// The transport failed: the hub is unreachable, the TLS handshake failed, or the hub answered
/// a server error.
pub const UNAVAILABLE: &str = "sync_http.unavailable";

/// Every `sync_http.*` code this crate defines, so the error-code scan sees each exactly where it is
/// used. Each code's literal also appears at the constructor that raises it.
pub const CODES: [&str; 12] = [
    IDENTITY_REQUIRED,
    IDENTITY_UNKNOWN,
    IDENTITY_REVOKED,
    PARTITION_VIOLATION,
    BATCH_TOO_LARGE,
    VERSION_MISMATCH,
    METADATA_EPOCH,
    CORRUPT_BATCH,
    INVALID_REQUEST,
    DEADLINE_EXCEEDED,
    CIRCUIT_OPEN,
    UNAVAILABLE,
];

/// What went wrong in one transport operation.
///
/// `code` is stable and callers match on it, never on the message; the message names the node,
/// the source and the sequences involved and never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The stable machine-readable code (`sync_http.partition_violation`, ...).
    pub code: &'static str,
    /// What went wrong. Never a secret, never a raw credential.
    pub message: String,
}

impl Error {
    /// An error with the given code and message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// No client certificate was presented.
    pub fn identity_required(message: impl Into<String>) -> Self {
        Self::new(IDENTITY_REQUIRED, message)
    }

    /// The certificate is not enrolled.
    pub fn identity_unknown(message: impl Into<String>) -> Self {
        Self::new(IDENTITY_UNKNOWN, message)
    }

    /// The certificate was revoked.
    pub fn identity_revoked(message: impl Into<String>) -> Self {
        Self::new(IDENTITY_REVOKED, message)
    }

    /// A batch's source partition is not owned by the authenticated node.
    pub fn partition_violation(node: &str, source: &str) -> Self {
        Self::new(
            PARTITION_VIOLATION,
            format!("node `{node}` does not own the partition of source `{source}`"),
        )
    }

    /// A batch exceeded a size or record limit.
    pub fn batch_too_large(message: impl Into<String>) -> Self {
        Self::new(BATCH_TOO_LARGE, message)
    }

    /// No protocol version is common to the two peers.
    pub fn version_mismatch(ours: impl std::fmt::Display, theirs: impl std::fmt::Display) -> Self {
        Self::new(
            VERSION_MISMATCH,
            format!("no common protocol version: ours {ours}, theirs {theirs}"),
        )
    }

    /// The two peers' metadata epochs differ.
    pub fn metadata_epoch(ours: u64, theirs: u64) -> Self {
        Self::new(
            METADATA_EPOCH,
            format!("metadata epoch differs: ours {ours}, theirs {theirs}"),
        )
    }

    /// A streamed batch failed its checksum or its framing.
    pub fn corrupt_batch(message: impl Into<String>) -> Self {
        Self::new(CORRUPT_BATCH, message)
    }

    /// A request is malformed.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(INVALID_REQUEST, message)
    }

    /// A session's deadline passed.
    pub fn deadline_exceeded(message: impl Into<String>) -> Self {
        Self::new(DEADLINE_EXCEEDED, message)
    }

    /// The client's breaker for the hub is open.
    pub fn circuit_open(destination: &str, retry_after: std::time::Duration) -> Self {
        Self::new(
            CIRCUIT_OPEN,
            format!(
                "the circuit for `{destination}` is open; retry after {} ms",
                retry_after.as_millis()
            ),
        )
    }

    /// The transport failed.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(UNAVAILABLE, message)
    }

    /// The HTTP status registered for this code; fixed within a major version.
    pub fn status(&self) -> StatusCode {
        match self.code {
            IDENTITY_REQUIRED => StatusCode::UNAUTHORIZED,
            IDENTITY_UNKNOWN | IDENTITY_REVOKED | PARTITION_VIOLATION => StatusCode::FORBIDDEN,
            BATCH_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
            VERSION_MISMATCH | METADATA_EPOCH => StatusCode::CONFLICT,
            CORRUPT_BATCH | INVALID_REQUEST => StatusCode::BAD_REQUEST,
            DEADLINE_EXCEEDED => StatusCode::GATEWAY_TIMEOUT,
            CIRCUIT_OPEN | UNAVAILABLE => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Whether the registry marks this code retryable.
    pub fn retryable(&self) -> bool {
        matches!(self.code, DEADLINE_EXCEEDED | CIRCUIT_OPEN | UNAVAILABLE)
    }

    /// Rebuilds an error from a code and message received from the hub.
    ///
    /// A code outside this crate's vocabulary is reported as `sync_http.unavailable`, with the original
    /// code kept in the message: a peer may be newer, and the client must never invent a code.
    pub fn from_code(code: &str, message: impl Into<String>) -> Self {
        let message = message.into();
        let known: &'static str = match code {
            IDENTITY_REQUIRED => IDENTITY_REQUIRED,
            IDENTITY_UNKNOWN => IDENTITY_UNKNOWN,
            IDENTITY_REVOKED => IDENTITY_REVOKED,
            PARTITION_VIOLATION => PARTITION_VIOLATION,
            BATCH_TOO_LARGE => BATCH_TOO_LARGE,
            VERSION_MISMATCH => VERSION_MISMATCH,
            METADATA_EPOCH => METADATA_EPOCH,
            CORRUPT_BATCH => CORRUPT_BATCH,
            INVALID_REQUEST => INVALID_REQUEST,
            DEADLINE_EXCEEDED => DEADLINE_EXCEEDED,
            CIRCUIT_OPEN => CIRCUIT_OPEN,
            UNAVAILABLE => UNAVAILABLE,
            _ => UNAVAILABLE,
        };
        if known == UNAVAILABLE && code != UNAVAILABLE {
            Self::new(UNAVAILABLE, format!("{code}: {message}"))
        } else {
            Self::new(known, message)
        }
    }

    /// The app-facing envelope for this error.
    pub fn to_body(&self) -> ErrorBody {
        ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: self.message.clone(),
                retryable: self.retryable(),
                request_id: String::new(),
            },
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// Result of every fallible sync-transport operation.
pub type Result<T> = std::result::Result<T, Error>;

/// The native API error body (`{"error": ...}`), the same shape `spot-http` renders.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    /// The only field of the envelope.
    pub error: ErrorDetail,
}

/// One machine-readable error.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorDetail {
    /// Dotted code from the registry.
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
    /// Registry retryability flag.
    pub retryable: bool,
    /// Request id, empty string when unknown.
    pub request_id: String,
}

/// An [`Error`] rendered as an HTTP response, used as a handler's error type.
#[derive(Debug)]
pub struct Failure(Error);

impl Failure {
    /// Wraps an error for rendering.
    pub fn new(error: Error) -> Self {
        Self(error)
    }

    /// The underlying error.
    pub fn error(&self) -> &Error {
        &self.0
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self(error)
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = self.0.status();
        (status, axum::Json(self.0.to_body())).into_response()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn codes_have_the_expected_status_and_retryability() {
        assert_eq!(Error::identity_unknown("x").status(), StatusCode::FORBIDDEN);
        assert_eq!(
            Error::partition_violation("edge", "site#p").status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            Error::batch_too_large("big").status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert!(Error::unavailable("down").retryable());
        assert!(!Error::corrupt_batch("bad").retryable());
        assert_eq!(CODES.len(), 12);
    }

    #[test]
    fn the_envelope_has_the_native_api_shape() {
        let body = Error::partition_violation("edge-khi", "site#other").to_body();
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(
            json.pointer("/error/code").and_then(|v| v.as_str()),
            Some(PARTITION_VIOLATION)
        );
        assert_eq!(
            json.pointer("/error/retryable").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(json.pointer("/error/message").is_some());
        assert!(json.pointer("/error/request_id").is_some());
    }
}
