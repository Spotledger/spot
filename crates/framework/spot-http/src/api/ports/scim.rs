//! The SCIM seam of `/scim/v2` (WP-IAM-06.T2; ADR-070 sections 7.2 and 7.3).
//!
//! SCIM is not part of the native API contract (`/api/v1`): it is the identity provider's own
//! protocol, served under `/scim/v2` with a **dedicated SCIM credential** (an API key whose scope
//! is exactly `scim`), never a user session. `spot-http` therefore defines [`ScimPort`] over plain
//! parts of a request, and the host (`spot-cli`, which holds the engine, the directory and
//! `spot-iam-scim`) writes the adapter. Everything that decides is the port's: whether the bearer
//! is a SCIM key, which tenant it belongs to, what the request does and how an error is worded.
//! The route passes **no user and no role**.
//!
//! A reply is always a [`ScimReply`]: a port failure is an RFC 7644 error body, never the native
//! API's error envelope.

use std::collections::BTreeMap;

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`ScimPort`].
pub const KEY: PortKey = PortKey::of::<dyn ScimPort>();

/// The media type of every SCIM body.
pub const SCIM_CONTENT_TYPE: &str = "application/scim+json";

/// The `schemas` URN of a SCIM error body (RFC 7644 section 3.12).
pub const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";

/// One SCIM request, in the plain parts the port needs. The body is raw bytes: a malformed
/// document is the port's error to word (and never echo).
#[derive(Debug)]
pub struct ScimCall {
    /// The HTTP method, upper case.
    pub method: String,
    /// The path as routed, for instance `/scim/v2/Users/{id}`.
    pub path: String,
    /// The decoded query parameters (`filter`, `startIndex`, `count`, `attributes`, ...).
    pub query: BTreeMap<String, String>,
    /// The `If-Match` header, when present.
    pub if_match: Option<String>,
    /// The body bytes, at most [`ScimPort::max_body_bytes`].
    pub body: Vec<u8>,
}

/// One SCIM answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScimReply {
    /// The HTTP status.
    pub status: u16,
    /// The JSON body; empty for `204`.
    pub body: String,
    /// The `ETag`, when the answer is one resource.
    pub etag: Option<String>,
    /// The `Location`, for a created resource.
    pub location: Option<String>,
    /// Seconds for `Retry-After`, when the credential is over its rate.
    pub retry_after: Option<u64>,
}

impl ScimReply {
    /// An RFC 7644 error answer: `status`, an optional `scimType` and a sentence that names no
    /// secret and echoes no request content.
    pub fn error(status: u16, scim_type: Option<&str>, detail: &str) -> Self {
        let mut body = serde_json::json!({
            "schemas": [ERROR_SCHEMA],
            "status": status.to_string(),
            "detail": detail,
        });
        if let (Some(kind), Some(map)) = (scim_type, body.as_object_mut()) {
            map.insert("scimType".to_owned(), serde_json::Value::from(kind));
        }
        Self {
            status,
            body: body.to_string(),
            etag: None,
            location: None,
            retry_after: None,
        }
    }
}

/// Serves `/scim/v2` for one site.
pub trait ScimPort: Send + Sync {
    /// The largest body the port reads, in bytes. The route refuses a larger one before the port
    /// is called.
    fn max_body_bytes(&self) -> usize;

    /// Serves one request as the SCIM credential `ctx` carries. It authenticates the bearer
    /// itself and answers `401` for anything that is not a live SCIM key of this site.
    fn serve<'a>(&'a self, ctx: &'a RequestCtx, call: ScimCall) -> BoxFuture<'a, ScimReply>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn an_error_reply_is_an_rfc_7644_body_with_a_string_status() {
        let reply = ScimReply::error(409, Some("uniqueness"), "the user name is taken");
        let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert_eq!(json["schemas"][0], ERROR_SCHEMA);
        assert_eq!(json["status"], "409");
        assert_eq!(json["scimType"], "uniqueness");
        assert_eq!(reply.status, 409);
    }

    #[test]
    fn an_error_reply_without_a_type_has_no_scim_type() {
        let reply = ScimReply::error(401, None, "a bearer credential is required");
        let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert!(json.get("scimType").is_none());
    }
}
