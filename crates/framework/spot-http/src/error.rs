//! Error responses with the native API error envelope.

use std::net::SocketAddr;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::request::RequestId;

/// Errors raised while bringing the server up or running its accept loop.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// `bind` was configured without any address.
    #[error("server bind list must not be empty")]
    EmptyBind,
    /// The configured address could not be bound.
    #[error("failed to bind {addr}")]
    BindFailed {
        /// The address that failed.
        addr: SocketAddr,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The server loop itself failed (listener setup, task join).
    #[error("server loop failed: {0}")]
    ServerLoop(String),
}

/// The `x-spot-error-codes` codes this crate emits (contract `x-spot-error-codes` registry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpErrorCode {
    /// No site resolves for the request's host.
    UnknownSite,
    /// The request body is larger than allowed.
    BodyTooLarge,
    /// The request did not finish in time.
    Timeout,
    /// Unexpected server-side failure.
    Internal,
    /// No route is registered for the path.
    UnknownRoute,
    /// The method is not registered for the path.
    MethodNotAllowed,
}

impl HttpErrorCode {
    /// The dotted machine-readable code.
    pub fn code(self) -> &'static str {
        match self {
            HttpErrorCode::UnknownSite => "http.unknown_site",
            HttpErrorCode::BodyTooLarge => "http.body_too_large",
            HttpErrorCode::Timeout => "http.timeout",
            HttpErrorCode::Internal => "http.internal",
            HttpErrorCode::UnknownRoute => "api.unknown_route",
            HttpErrorCode::MethodNotAllowed => "api.method_not_allowed",
        }
    }

    /// The status registered for the code; fixed within a major version.
    pub fn status(self) -> StatusCode {
        match self {
            HttpErrorCode::UnknownSite => StatusCode::NOT_FOUND,
            HttpErrorCode::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            HttpErrorCode::Timeout => StatusCode::GATEWAY_TIMEOUT,
            HttpErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            HttpErrorCode::UnknownRoute => StatusCode::NOT_FOUND,
            HttpErrorCode::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
        }
    }

    /// Whether the contract marks the error retryable.
    pub fn retryable(self) -> bool {
        matches!(self, HttpErrorCode::Timeout)
    }

    /// The contract's plain-language default message, used when no more specific one applies.
    pub fn default_message(self) -> &'static str {
        match self {
            HttpErrorCode::UnknownSite => "No site is configured for this host name.",
            HttpErrorCode::BodyTooLarge => "The body is larger than the server allows.",
            HttpErrorCode::Timeout => "The request took longer than the server allows.",
            HttpErrorCode::Internal => {
                "Something went wrong on the server. Your request was not processed."
            }
            HttpErrorCode::UnknownRoute => "The path does not exist for this site.",
            HttpErrorCode::MethodNotAllowed => "The method is not supported for this resource.",
        }
    }
}

/// The native API error body (`{"error": ...}`).
#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    /// The only field of the envelope.
    pub error: ErrorDetail,
}

/// One machine-readable error.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorDetail {
    /// Dotted code from the contract.
    pub code: &'static str,
    /// Human-readable message.
    pub message: String,
    /// Contract retryability flag.
    pub retryable: bool,
    /// Request id, empty string when unknown.
    pub request_id: String,
}

impl ErrorEnvelope {
    /// Builds the envelope for a code, a message and an optional request id.
    pub fn new(
        code: HttpErrorCode,
        message: impl Into<String>,
        request_id: Option<&RequestId>,
    ) -> Self {
        ErrorEnvelope {
            error: ErrorDetail {
                code: code.code(),
                message: message.into(),
                retryable: code.retryable(),
                request_id: request_id
                    .map(|id| id.as_str().to_owned())
                    .unwrap_or_default(),
            },
        }
    }
}

/// Renders an error as a JSON response with the right status and content type.
pub(crate) fn error_response(
    code: HttpErrorCode,
    message: impl Into<String>,
    request_id: Option<&RequestId>,
) -> Response {
    (
        code.status(),
        axum::Json(ErrorEnvelope::new(code, message, request_id)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Must;

    #[test]
    fn unknown_site_is_404_and_not_retryable() {
        let code = HttpErrorCode::UnknownSite;
        assert_eq!(code.code(), "http.unknown_site");
        assert_eq!(code.status(), StatusCode::NOT_FOUND);
        assert!(!code.retryable());
    }

    #[test]
    fn body_too_large_is_413() {
        assert_eq!(
            HttpErrorCode::BodyTooLarge.status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(HttpErrorCode::BodyTooLarge.code(), "http.body_too_large");
        assert!(!HttpErrorCode::BodyTooLarge.retryable());
    }

    #[test]
    fn fallback_codes_match_contract() {
        assert_eq!(HttpErrorCode::UnknownRoute.status(), StatusCode::NOT_FOUND);
        assert_eq!(HttpErrorCode::UnknownRoute.code(), "api.unknown_route");
        assert!(!HttpErrorCode::UnknownRoute.retryable());
        assert_eq!(
            HttpErrorCode::MethodNotAllowed.status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(
            HttpErrorCode::MethodNotAllowed.code(),
            "api.method_not_allowed"
        );
        assert!(!HttpErrorCode::MethodNotAllowed.retryable());
    }

    #[test]
    fn internal_is_500() {
        assert_eq!(
            HttpErrorCode::Internal.status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(HttpErrorCode::Internal.code(), "http.internal");
        assert!(!HttpErrorCode::Internal.retryable());
    }

    #[test]
    fn timeout_is_504_and_retryable() {
        assert_eq!(
            HttpErrorCode::Timeout.status(),
            StatusCode::GATEWAY_TIMEOUT,
            "the contract registry pins http.timeout to 504"
        );
        assert_eq!(HttpErrorCode::Timeout.code(), "http.timeout");
        assert!(HttpErrorCode::Timeout.retryable());
    }

    #[test]
    fn envelope_has_contract_shape() {
        let id = RequestId::generate();
        let envelope = ErrorEnvelope::new(
            HttpErrorCode::BodyTooLarge,
            "The body is larger than the server allows.",
            Some(&id),
        );
        let json = serde_json::to_value(&envelope).must();
        assert_eq!(
            json.pointer("/error/code").and_then(|v| v.as_str()),
            Some("http.body_too_large")
        );
        assert_eq!(
            json.pointer("/error/retryable").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(
            json.pointer("/error/request_id").and_then(|v| v.as_str()),
            Some(id.as_str())
        );
        assert!(
            json.pointer("/error/message")
                .and_then(|v| v.as_str())
                .is_some()
        );
        assert!(json.get("error").and_then(|v| v.as_object()).must().len() == 4);
    }

    #[test]
    fn envelope_without_request_id_is_empty_string() {
        let envelope = ErrorEnvelope::new(HttpErrorCode::Internal, "boom", None);
        let json = serde_json::to_value(&envelope).must();
        assert_eq!(
            json.pointer("/error/request_id").and_then(|v| v.as_str()),
            Some("")
        );
    }
}
