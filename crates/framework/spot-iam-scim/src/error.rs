//! The one error of this crate: an RFC 7644 error body with a stable `iam_scim.*` code.
//!
//! Every refusal a SCIM client can see is one of the codes in [`codes`], and every one of them
//! renders as the RFC 7644 error document ([`ScimErrorBody`]): a `status`, a `detail` sentence
//! and, where the RFC defines one, a `scimType` keyword. A detail sentence names a field or a
//! path, never a value, a token or a credential.

use serde::{Deserialize, Serialize};

use crate::resource::ERROR_SCHEMA;

/// The stable codes of this crate, as listed in `catalogue/iam_scim.toml`.
pub mod codes {
    /// `iam_scim.invalid_filter`: a `filter` query parameter does not parse or names no attribute.
    pub const INVALID_FILTER: &str = "iam_scim.invalid_filter";
    /// `iam_scim.invalid_path`: a PATCH `path` is not an attribute this resource has.
    pub const INVALID_PATH: &str = "iam_scim.invalid_path";
    /// `iam_scim.invalid_value`: a PATCH value does not fit the attribute it is written to.
    pub const INVALID_VALUE: &str = "iam_scim.invalid_value";
    /// `iam_scim.invalid_syntax`: the request body is not a well-formed SCIM document.
    pub const INVALID_SYNTAX: &str = "iam_scim.invalid_syntax";
    /// `iam_scim.no_target`: a `remove` names a path that holds nothing to remove.
    pub const NO_TARGET: &str = "iam_scim.no_target";
    /// `iam_scim.uniqueness`: a `userName` or `displayName` is already taken.
    pub const UNIQUENESS: &str = "iam_scim.uniqueness";
    /// `iam_scim.mutability`: a read-only attribute was written.
    pub const MUTABILITY: &str = "iam_scim.mutability";
    /// `iam_scim.too_many`: the result set is larger than the page cap allows.
    pub const TOO_MANY: &str = "iam_scim.too_many";
    /// `iam_scim.not_found`: no such resource.
    pub const NOT_FOUND: &str = "iam_scim.not_found";
    /// `iam_scim.unauthorised`: the bearer credential is missing, expired or not for a service user.
    pub const UNAUTHORISED: &str = "iam_scim.unauthorised";
    /// `iam_scim.forbidden`: the credential may not act on this tenant or this resource.
    pub const FORBIDDEN: &str = "iam_scim.forbidden";
    /// `iam_scim.rate_limited`: the credential has sent too many requests.
    pub const RATE_LIMITED: &str = "iam_scim.rate_limited";
    /// `iam_scim.too_large`: the request body is larger than the limit allows.
    pub const TOO_LARGE: &str = "iam_scim.too_large";
    /// `iam_scim.version_conflict`: an `If-Match` did not match the resource's current version.
    pub const VERSION_CONFLICT: &str = "iam_scim.version_conflict";
    /// `iam_scim.method`: the path does not allow the method.
    pub const METHOD: &str = "iam_scim.method";
    /// `iam_scim.unsupported`: the endpoint or filter feature is not served.
    pub const UNSUPPORTED: &str = "iam_scim.unsupported";
    /// `iam_scim.mapping`: a group-mapping row is invalid (unknown role, empty group, bad scope).
    pub const MAPPING: &str = "iam_scim.mapping";
    /// `iam_scim.internal`: an invariant the caller must fix; never a client's fault.
    pub const INTERNAL: &str = "iam_scim.internal";
}

use codes::*;

/// The RFC 7644 error body a client receives.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScimErrorBody {
    /// Always `["urn:ietf:params:scim:api:messages:2.0:Error"]`.
    pub schemas: Vec<String>,
    /// The `scimType` keyword, when the RFC defines one.
    #[serde(rename = "scimType", skip_serializing_if = "Option::is_none")]
    pub scim_type: Option<String>,
    /// A sentence naming the field or the rule, never a value.
    pub detail: String,
    /// The HTTP status as text, as the RFC's examples show.
    pub status: String,
}

/// A refusal: a stable code, the HTTP status, the RFC `scimType` when there is one, and a
/// sentence for the client.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {detail}")]
pub struct ScimError {
    /// The stable `iam_scim.*` code.
    pub code: &'static str,
    /// The HTTP status the adapter must send.
    pub status: u16,
    /// The RFC 7644 `scimType` keyword, when one applies.
    pub scim_type: Option<&'static str>,
    /// What is wrong, naming no value.
    pub detail: String,
}

/// The result of a SCIM operation.
pub type ScimResult<T> = Result<T, ScimError>;

impl ScimError {
    /// A refusal with an explicit code, status and `scimType`.
    pub fn new(
        code: &'static str,
        status: u16,
        scim_type: Option<&'static str>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            status,
            scim_type,
            detail: detail.into(),
        }
    }

    /// `400 invalidFilter`.
    pub fn invalid_filter(detail: impl Into<String>) -> Self {
        Self::new(INVALID_FILTER, 400, Some("invalidFilter"), detail)
    }

    /// `400 invalidPath`.
    pub fn invalid_path(detail: impl Into<String>) -> Self {
        Self::new(INVALID_PATH, 400, Some("invalidPath"), detail)
    }

    /// `400 invalidValue`.
    pub fn invalid_value(detail: impl Into<String>) -> Self {
        Self::new(INVALID_VALUE, 400, Some("invalidValue"), detail)
    }

    /// `400 invalidSyntax`.
    pub fn invalid_syntax(detail: impl Into<String>) -> Self {
        Self::new(INVALID_SYNTAX, 400, Some("invalidSyntax"), detail)
    }

    /// `400 noTarget`.
    pub fn no_target(detail: impl Into<String>) -> Self {
        Self::new(NO_TARGET, 400, Some("noTarget"), detail)
    }

    /// `409 uniqueness`.
    pub fn uniqueness(detail: impl Into<String>) -> Self {
        Self::new(UNIQUENESS, 409, Some("uniqueness"), detail)
    }

    /// `400 mutability`.
    pub fn mutability(detail: impl Into<String>) -> Self {
        Self::new(MUTABILITY, 400, Some("mutability"), detail)
    }

    /// `400 tooMany` (the RFC's page-cap refusal).
    pub fn too_many(detail: impl Into<String>) -> Self {
        Self::new(TOO_MANY, 400, Some("tooMany"), detail)
    }

    /// `404`.
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(NOT_FOUND, 404, None, detail)
    }

    /// `401`.
    pub fn unauthorised(detail: impl Into<String>) -> Self {
        Self::new(UNAUTHORISED, 401, None, detail)
    }

    /// `403`.
    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(FORBIDDEN, 403, None, detail)
    }

    /// `429`.
    pub fn rate_limited(detail: impl Into<String>) -> Self {
        Self::new(RATE_LIMITED, 429, None, detail)
    }

    /// `413`.
    pub fn too_large(detail: impl Into<String>) -> Self {
        Self::new(TOO_LARGE, 413, None, detail)
    }

    /// `412`.
    pub fn version_conflict(detail: impl Into<String>) -> Self {
        Self::new(VERSION_CONFLICT, 412, None, detail)
    }

    /// `405`.
    pub fn method(detail: impl Into<String>) -> Self {
        Self::new(METHOD, 405, None, detail)
    }

    /// `501`.
    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self::new(UNSUPPORTED, 501, None, detail)
    }

    /// `400` for an invalid mapping row.
    pub fn mapping(detail: impl Into<String>) -> Self {
        Self::new(MAPPING, 400, Some("invalidValue"), detail)
    }

    /// `500`.
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(INTERNAL, 500, None, detail)
    }

    /// The RFC 7644 error document.
    pub fn body(&self) -> ScimErrorBody {
        ScimErrorBody {
            schemas: vec![ERROR_SCHEMA.to_owned()],
            scim_type: self.scim_type.map(str::to_owned),
            detail: self.detail.clone(),
            status: self.status.to_string(),
        }
    }
}
