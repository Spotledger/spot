//! Stable `link_transport.*` codes and the one error type both the server and the client use.
//!
//! The server answers with `{ "error": { "code", "message", "retryable" } }`. Messages are fixed
//! sentences: they never echo anything the peer sent, never a key, a signature, a payload or a
//! certificate, and a refusal that happens before the caller is tied to the link says the same
//! thing for "no such link" and "not your link" ([`codes::FORBIDDEN`]), so the endpoint is not
//! an oracle for which links exist.

use std::time::Duration;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

/// Stable codes of this crate (catalogued in `spot-errors/catalogue/link_transport.toml`).
pub mod codes {
    /// No client certificate was presented.
    pub const IDENTITY_REQUIRED: &str = "link_transport.identity_required";
    /// The caller is not allowed on this link (unknown certificate, unknown link, a link the
    /// caller is not the peer of, or a message that is not from the authenticated peer).
    pub const FORBIDDEN: &str = "link_transport.forbidden";
    /// The link was revoked.
    pub const REVOKED: &str = "link_transport.revoked";
    /// The link is not active (not yet activated, or expired).
    pub const NOT_ACTIVE: &str = "link_transport.not_active";
    /// This side does not use this carrier in this direction.
    pub const CARRIER_DISABLED: &str = "link_transport.carrier_disabled";
    /// The request is malformed.
    pub const INVALID_REQUEST: &str = "link_transport.invalid_request";
    /// A handshake message or revoke that was already used (`409`): a replay, which is not a
    /// malformed request.
    pub const REPLAYED: &str = "link_transport.replayed";
    /// The request body is larger than the limit.
    pub const TOO_LARGE: &str = "link_transport.too_large";
    /// The link's rate limit is used up.
    pub const RATE_LIMITED: &str = "link_transport.rate_limited";
    /// The receiver is busy; nothing was taken.
    pub const BUSY: &str = "link_transport.busy";
    /// A store, audit or quarantine failure; nothing was acknowledged.
    pub const UNAVAILABLE: &str = "link_transport.unavailable";
    /// The peer could not be reached or answered with a server error.
    pub const PEER_UNAVAILABLE: &str = "link_transport.peer_unavailable";
    /// The peer refused the request.
    pub const PEER_REFUSED: &str = "link_transport.peer_refused";
    /// The peer sent something that cannot be right (a receipt or message that does not verify,
    /// a reply that does not parse).
    pub const PEER_MISBEHAVED: &str = "link_transport.peer_misbehaved";
    /// The peer's circuit breaker is open; a probe is allowed later.
    pub const CIRCUIT_OPEN: &str = "link_transport.circuit_open";
    /// The call's deadline passed.
    pub const DEADLINE_EXCEEDED: &str = "link_transport.deadline_exceeded";

    /// Every code, so a scan sees each one.
    pub const ALL: [&str; 16] = [
        IDENTITY_REQUIRED,
        FORBIDDEN,
        REVOKED,
        NOT_ACTIVE,
        CARRIER_DISABLED,
        INVALID_REQUEST,
        REPLAYED,
        TOO_LARGE,
        RATE_LIMITED,
        BUSY,
        UNAVAILABLE,
        PEER_UNAVAILABLE,
        PEER_REFUSED,
        PEER_MISBEHAVED,
        CIRCUIT_OPEN,
        DEADLINE_EXCEEDED,
    ];
}

/// Result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// What went wrong in one transport operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    /// The stable code; callers match on it, never on the message.
    pub code: &'static str,
    /// The HTTP status the server answers with.
    pub status: u16,
    /// A fixed sentence.
    pub message: &'static str,
    /// Whether the same request may succeed later.
    pub retryable: bool,
    /// How long to wait before trying again, when the server said so.
    pub retry_after: Option<Duration>,
}

impl Error {
    const fn new(code: &'static str, status: u16, message: &'static str, retryable: bool) -> Self {
        Self {
            code,
            status,
            message,
            retryable,
            retry_after: None,
        }
    }

    /// No client certificate.
    #[must_use]
    pub const fn identity_required() -> Self {
        Self::new(
            codes::IDENTITY_REQUIRED,
            401,
            "mutual TLS with an enrolled certificate is required",
            false,
        )
    }

    /// Not allowed on this link.
    #[must_use]
    pub const fn forbidden() -> Self {
        Self::new(
            codes::FORBIDDEN,
            403,
            "this caller may not use this link",
            false,
        )
    }

    /// The link was revoked.
    #[must_use]
    pub const fn revoked() -> Self {
        Self::new(codes::REVOKED, 403, "the link was revoked", false)
    }

    /// The link is not active.
    #[must_use]
    pub const fn not_active() -> Self {
        Self::new(codes::NOT_ACTIVE, 403, "the link is not active", false)
    }

    /// The carrier is off in this direction.
    #[must_use]
    pub const fn carrier_disabled() -> Self {
        Self::new(
            codes::CARRIER_DISABLED,
            403,
            "this carrier is not used in this direction on this link",
            false,
        )
    }

    /// A malformed request.
    #[must_use]
    pub const fn invalid_request(message: &'static str) -> Self {
        Self::new(codes::INVALID_REQUEST, 400, message, false)
    }

    /// A body above the limit.
    #[must_use]
    pub const fn too_large() -> Self {
        Self::new(
            codes::TOO_LARGE,
            413,
            "the request body is larger than the limit",
            false,
        )
    }

    /// The rate limit is used up.
    #[must_use]
    pub const fn rate_limited(retry_after: Duration) -> Self {
        Self {
            retry_after: Some(retry_after),
            ..Self::new(
                codes::RATE_LIMITED,
                429,
                "too many requests on this link; slow down",
                true,
            )
        }
    }

    /// A handshake message or revoke that was already used (`409`, [`codes::REPLAYED`]). The text
    /// is fixed: no nonce, link id or peer name is in it.
    #[must_use]
    pub const fn replayed() -> Self {
        Self::new(codes::REPLAYED, 409, "the message was already used", false)
    }

    /// A handshake message or revoke that is out of date for the state of the
    /// link (`409`). It reuses [`codes::INVALID_REQUEST`]: the peer learns only that the request
    /// cannot be applied as sent, never which check refused it.
    #[must_use]
    pub const fn conflict() -> Self {
        Self::new(
            codes::INVALID_REQUEST,
            409,
            "the message was already used or is out of date",
            false,
        )
    }

    /// The receiver took nothing.
    #[must_use]
    pub const fn busy() -> Self {
        Self::new(
            codes::BUSY,
            503,
            "the receiver is busy; send again later",
            true,
        )
    }

    /// A store, audit or quarantine failure.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self::new(
            codes::UNAVAILABLE,
            503,
            "the request could not be recorded; nothing was acknowledged",
            true,
        )
    }

    /// The peer is unreachable.
    #[must_use]
    pub const fn peer_unavailable() -> Self {
        Self::new(
            codes::PEER_UNAVAILABLE,
            503,
            "the peer could not be reached",
            true,
        )
    }

    /// The peer refused.
    #[must_use]
    pub const fn peer_refused() -> Self {
        Self::new(
            codes::PEER_REFUSED,
            502,
            "the peer refused the request",
            false,
        )
    }

    /// The peer sent something wrong.
    #[must_use]
    pub const fn peer_misbehaved() -> Self {
        Self::new(
            codes::PEER_MISBEHAVED,
            502,
            "the peer sent something that does not verify",
            false,
        )
    }

    /// The breaker is open.
    #[must_use]
    pub const fn circuit_open(retry_after: Duration) -> Self {
        Self {
            retry_after: Some(retry_after),
            ..Self::new(
                codes::CIRCUIT_OPEN,
                503,
                "the peer is being given time to recover",
                true,
            )
        }
    }

    /// The deadline passed.
    #[must_use]
    pub const fn deadline_exceeded() -> Self {
        Self::new(
            codes::DEADLINE_EXCEEDED,
            504,
            "the call took longer than its deadline",
            true,
        )
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// The error body on the wire.
#[derive(Serialize, Deserialize)]
pub(crate) struct ErrorBody {
    pub(crate) error: ErrorDetail,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ErrorDetail {
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) retryable: bool,
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code.to_owned(),
                message: self.message.to_owned(),
                retryable: self.retryable,
            },
        };
        let mut response = (status, Json(body)).into_response();
        if let Some(wait) = self.retry_after {
            let seconds = wait.as_secs().saturating_add(1);
            if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

/// Maps a code a peer sent back to the nearest local error, so a client reports a stable code
/// and never repeats the peer's text.
pub(crate) fn from_remote(status: u16, code: &str) -> Error {
    match code {
        codes::REVOKED => Error::revoked(),
        codes::NOT_ACTIVE => Error::not_active(),
        codes::CARRIER_DISABLED => Error::carrier_disabled(),
        codes::TOO_LARGE => Error::too_large(),
        codes::REPLAYED => Error::replayed(),
        _ if status == 429 => Error::rate_limited(Duration::from_secs(1)),
        _ if status >= 500 => Error::peer_unavailable(),
        _ => Error::peer_refused(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_constructor_uses_a_listed_code() {
        let all = [
            Error::identity_required(),
            Error::forbidden(),
            Error::revoked(),
            Error::not_active(),
            Error::carrier_disabled(),
            Error::invalid_request("x"),
            Error::too_large(),
            Error::conflict(),
            Error::replayed(),
            Error::rate_limited(Duration::ZERO),
            Error::busy(),
            Error::unavailable(),
            Error::peer_unavailable(),
            Error::peer_refused(),
            Error::peer_misbehaved(),
            Error::circuit_open(Duration::ZERO),
            Error::deadline_exceeded(),
        ];
        for error in all {
            assert!(codes::ALL.contains(&error.code), "{}", error.code);
        }
        assert_eq!(codes::ALL.len(), 16);
    }

    #[test]
    fn a_retry_after_header_is_sent_rounded_up() {
        let response = Error::rate_limited(Duration::from_millis(1500)).into_response();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .map(HeaderValue::as_bytes),
            Some(b"2".as_slice())
        );
    }
}
