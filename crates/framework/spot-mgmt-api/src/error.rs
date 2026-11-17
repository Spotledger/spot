//! The error registry of the management API: stable `mgmt.*` codes with their HTTP status and
//! retryable flag, exactly as `x-spot-error-codes` in `contracts/management-api/openapi.yaml`.
//!
//! Messages are fixed text per code. They never carry a signature, a nonce, a parameter value, a
//! path or a stack; the only code-specific datum is `details.parameter`, a parameter *name*.

use axum::Json;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// One code of the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MgmtCode {
    /// No verified client certificate.
    Unauthenticated,
    /// The operator may not call this operation.
    Forbidden,
    /// No such operation (for this caller).
    UnknownOperation,
    /// No such command (for this caller).
    UnknownCommand,
    /// A parameter is missing, unknown or out of bounds.
    InvalidParameter,
    /// The signed envelope is missing, malformed or does not verify.
    SignatureInvalid,
    /// The envelope is expired, from the future or lives too long.
    SignatureExpired,
    /// The nonce or command id was already used.
    Replayed,
    /// Approval records are needed.
    ApprovalRequired,
    /// Another command holds the lock.
    Conflict,
    /// The site's policy refuses the command.
    RefusedByPolicy,
    /// The operation is not supported here.
    NotAvailableHere,
    /// Too many requests or concurrent commands.
    RateLimited,
    /// The body is over 64 KiB.
    BodyTooLarge,
    /// An unexpected server error (also: audit or nonce store unavailable).
    Internal,
}

impl MgmtCode {
    /// Every code, for the conformance tests.
    pub const ALL: [MgmtCode; 15] = [
        Self::Unauthenticated,
        Self::Forbidden,
        Self::UnknownOperation,
        Self::UnknownCommand,
        Self::InvalidParameter,
        Self::SignatureInvalid,
        Self::SignatureExpired,
        Self::Replayed,
        Self::ApprovalRequired,
        Self::Conflict,
        Self::RefusedByPolicy,
        Self::NotAvailableHere,
        Self::RateLimited,
        Self::BodyTooLarge,
        Self::Internal,
    ];

    /// The dotted wire code.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthenticated => "mgmt.unauthenticated",
            Self::Forbidden => "mgmt.forbidden",
            Self::UnknownOperation => "mgmt.unknown_operation",
            Self::UnknownCommand => "mgmt.unknown_command",
            Self::InvalidParameter => "mgmt.invalid_parameter",
            Self::SignatureInvalid => "mgmt.signature_invalid",
            Self::SignatureExpired => "mgmt.signature_expired",
            Self::Replayed => "mgmt.replayed",
            Self::ApprovalRequired => "mgmt.approval_required",
            Self::Conflict => "mgmt.conflict",
            Self::RefusedByPolicy => "mgmt.refused_by_policy",
            Self::NotAvailableHere => "mgmt.not_available_here",
            Self::RateLimited => "mgmt.rate_limited",
            Self::BodyTooLarge => "mgmt.body_too_large",
            Self::Internal => "mgmt.internal",
        }
    }

    /// The HTTP status.
    #[must_use]
    pub fn status(self) -> StatusCode {
        match self {
            Self::Unauthenticated | Self::SignatureInvalid | Self::SignatureExpired => {
                StatusCode::UNAUTHORIZED
            }
            Self::Forbidden | Self::ApprovalRequired | Self::RefusedByPolicy => {
                StatusCode::FORBIDDEN
            }
            Self::UnknownOperation | Self::UnknownCommand => StatusCode::NOT_FOUND,
            Self::InvalidParameter => StatusCode::UNPROCESSABLE_ENTITY,
            Self::Replayed | Self::Conflict => StatusCode::CONFLICT,
            Self::NotAvailableHere => StatusCode::NOT_IMPLEMENTED,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Whether the same request may succeed later unchanged.
    #[must_use]
    pub fn retryable(self) -> bool {
        matches!(self, Self::Conflict | Self::RateLimited)
    }

    /// The fixed message: never a secret, a path or a stack.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Unauthenticated => "a verified client certificate is required",
            Self::Forbidden => "the operator may not call this operation",
            Self::UnknownOperation => "no such operation",
            Self::UnknownCommand => "no such command",
            Self::InvalidParameter => "a parameter is missing, unknown or out of bounds",
            Self::SignatureInvalid => "the signed command is missing, malformed or does not verify",
            Self::SignatureExpired => "the signed command is expired or outside the allowed window",
            Self::Replayed => "the command was already used",
            Self::ApprovalRequired => "the operation needs approval records",
            Self::Conflict => "another command holds the lock for this operation",
            Self::RefusedByPolicy => "the site's policy refuses the command",
            Self::NotAvailableHere => "the operation is not available on this site",
            Self::RateLimited => "too many requests or concurrent commands",
            Self::BodyTooLarge => "the request body is larger than 64 KiB",
            Self::Internal => "an internal error occurred",
        }
    }
}

/// A refusal or failure of one request: a code and, for parameter errors, the parameter's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MgmtError {
    /// The registry code.
    pub code: MgmtCode,
    /// The name of the offending parameter (never its value).
    pub parameter: Option<String>,
    /// A short audit-only reason code (never sent in the body).
    pub reason: &'static str,
}

impl MgmtError {
    /// An error with the code's own name as audit reason.
    #[must_use]
    pub fn new(code: MgmtCode) -> Self {
        Self {
            code,
            parameter: None,
            reason: code.as_str(),
        }
    }

    /// An error with a more specific audit reason.
    #[must_use]
    pub fn with_reason(code: MgmtCode, reason: &'static str) -> Self {
        Self {
            code,
            parameter: None,
            reason,
        }
    }

    /// A parameter error naming the parameter.
    #[must_use]
    pub fn parameter(name: &str) -> Self {
        let safe = is_safe_name(name);
        Self {
            code: MgmtCode::InvalidParameter,
            parameter: safe.then(|| name.to_owned()),
            reason: "mgmt.invalid_parameter",
        }
    }

    /// The HTTP response for this error under `request_id`.
    #[must_use]
    pub fn into_response_with(self, request_id: &str) -> Response {
        let mut error = json!({
            "code": self.code.as_str(),
            "message": self.code.message(),
            "retryable": self.code.retryable(),
            "request_id": request_id,
        });
        if let (Some(parameter), Some(map)) = (&self.parameter, error.as_object_mut()) {
            map.insert("details".to_owned(), json!({ "parameter": parameter }));
        }
        let body = Json(json!({ "error": error }));
        let mut response = (self.code.status(), body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        if self.code.retryable() {
            headers.insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
        }
        response
    }
}

/// A parameter or header name that is safe to echo: a short identifier.
fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
