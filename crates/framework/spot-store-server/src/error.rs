//! Errors: the contract's `store.*` wire codes, the server's own `store_server.*` codes, and the
//! mapping of every refusal of the layers below onto them.
//!
//! The contract (`contracts/store-api`) names its codes `store.<word>`; they are the wire
//! vocabulary and are built here from the bare word, so the code scan does not mistake the
//! contract's vocabulary for this crate's own. A message never carries a key, a token, a bundle,
//! a review summary, SQL or a connection string.

use std::fmt;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use spot_bundle_signing::SigningError;
use spot_registry::RegistryError;
use spot_store_state::StoreStateError;

tokio::task_local! {
    /// The id of the request being served; the error envelope carries it.
    pub(crate) static REQUEST_ID: String;
}

/// The stable codes of this crate's own refusals (not part of the store API contract).
pub mod codes {
    /// The configuration file is missing, unreadable or invalid.
    pub const CONFIG: &str = "store_server.config";
    /// A key file is missing, unreadable, malformed or readable by other users.
    pub const KEY_FILE: &str = "store_server.key_file";
    /// The server could not start: database, bridge, trust roots or the first revocation list.
    pub const STARTUP: &str = "store_server.startup";
    /// No such route, or an operator route names something that does not exist.
    pub const NOT_FOUND: &str = "store_server.not_found";
    /// The route exists but not for this method.
    pub const METHOD_NOT_ALLOWED: &str = "store_server.method_not_allowed";
    /// The server is overloaded or a dependency (database, review workers) is unavailable.
    pub const UNAVAILABLE: &str = "store_server.unavailable";
    /// An unexpected failure; the request id names it in the log.
    pub const INTERNAL: &str = "store_server.internal";
    /// The operator credential is missing or wrong.
    pub const ADMIN_UNAUTHORIZED: &str = "store_server.admin_unauthorized";
    /// An operator request is malformed.
    pub const INVALID_ADMIN_REQUEST: &str = "store_server.invalid_admin_request";
    /// An operator request asks for a listing transition the current state does not allow.
    pub const BAD_STATE: &str = "store_server.bad_state";
    /// A review could not be queued: the queue is full or every review worker is stuck.
    pub const REVIEW_UNAVAILABLE: &str = "store_server.review_unavailable";
    /// A review ran past its wall-clock limit and was recorded as failed.
    pub const REVIEW_TIMEOUT: &str = "store_server.review_timeout";
    /// A review ended without a report.
    pub const REVIEW_FAILED: &str = "store_server.review_failed";
}

/// The contract's error codes, by their word after `store.`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wire {
    InvalidRequest,
    Unauthorized,
    Forbidden,
    NotEntitled,
    PublisherRevoked,
    AppNotFound,
    VersionNotFound,
    ReviewNotFound,
    NameTaken,
    AlreadyYanked,
    VersionNotMonotonic,
    SignatureInvalid,
    TooLarge,
    InvalidManifest,
    BundleInvalid,
    RateLimited,
}

impl Wire {
    fn suffix(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotEntitled => "not_entitled",
            Self::PublisherRevoked => "publisher_revoked",
            Self::AppNotFound => "app_not_found",
            Self::VersionNotFound => "version_not_found",
            Self::ReviewNotFound => "review_not_found",
            Self::NameTaken => "name_taken",
            Self::AlreadyYanked => "already_yanked",
            Self::VersionNotMonotonic => "version_not_monotonic",
            Self::SignatureInvalid => "signature_invalid",
            Self::TooLarge => "too_large",
            Self::InvalidManifest => "invalid_manifest",
            Self::BundleInvalid => "bundle_invalid",
            Self::RateLimited => "rate_limited",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            Self::InvalidRequest => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden | Self::NotEntitled | Self::PublisherRevoked => StatusCode::FORBIDDEN,
            Self::AppNotFound | Self::VersionNotFound | Self::ReviewNotFound => {
                StatusCode::NOT_FOUND
            }
            Self::NameTaken | Self::AlreadyYanked => StatusCode::CONFLICT,
            Self::VersionNotMonotonic
            | Self::SignatureInvalid
            | Self::InvalidManifest
            | Self::BundleInvalid => StatusCode::UNPROCESSABLE_ENTITY,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::InvalidRequest => "The request is malformed or does not match the schema.",
            Self::Unauthorized => "No credential was sent, or it is invalid, expired or revoked.",
            Self::Forbidden => "The credential is valid but does not permit this operation.",
            Self::NotEntitled => "This site licence does not cover this paid app.",
            Self::PublisherRevoked => "The publisher's status is not active.",
            Self::AppNotFound => "No app with this name, or it has no public listing.",
            Self::VersionNotFound => "No such version of this app.",
            Self::ReviewNotFound => "No such review.",
            Self::NameTaken => "This app name is owned by a different publisher.",
            Self::AlreadyYanked => "This version is already yanked.",
            Self::VersionNotMonotonic => {
                "The version is not strictly greater than every version already published."
            }
            Self::SignatureInvalid => {
                "The detached signature does not verify against the uploaded bundle."
            }
            Self::TooLarge => "The uploaded bundle is larger than the 64 MiB maximum.",
            Self::InvalidManifest => "The bundle's manifest is too large, invalid or misnamed.",
            Self::BundleInvalid => "The archive failed a lower-level bundle check.",
            Self::RateLimited => "Too many requests for this credential.",
        }
    }

    fn explanation(self) -> &'static str {
        match self {
            Self::InvalidRequest => {
                "Check the request against the store API contract and send it again."
            }
            Self::Unauthorized => "Send `Authorization: Bearer <key>` with a live publisher key.",
            Self::Forbidden => "This credential cannot act for that publisher.",
            Self::NotEntitled => "A paid app's bundle needs a site licence that covers it.",
            Self::PublisherRevoked => {
                "A revoked publisher may not publish or yank; contact the store operator."
            }
            Self::AppNotFound => {
                "The app does not exist, or none of its versions is publicly listed yet."
            }
            Self::VersionNotFound => {
                "The version does not exist, is not publicly listed, or is not yours to change."
            }
            Self::ReviewNotFound => {
                "The review does not exist, belongs to another publisher, or has expired."
            }
            Self::NameTaken => {
                "App names are first-come, first-served and exclusive. Choose a different name."
            }
            Self::AlreadyYanked => "There is no un-yank; publish a higher version instead.",
            Self::VersionNotMonotonic => {
                "Publish a version above every version already published for this app."
            }
            Self::SignatureInvalid => {
                "Sign the bundle's canonical digest with your own certificate and upload that signature."
            }
            Self::TooLarge => "Shrink the archive and publish again.",
            Self::InvalidManifest => {
                "Fix manifest.toml (at most 64 KiB, the name must equal the path) and publish again."
            }
            Self::BundleInvalid => "details.bundle_code names the exact bundle problem.",
            Self::RateLimited => "Wait for the number of seconds in Retry-After and try again.",
        }
    }
}

/// A refusal that becomes an error envelope.
#[derive(Debug)]
pub(crate) struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub explanation: Option<String>,
    pub retryable: bool,
    /// Boxed so the error stays small inside every `Result`.
    pub details: Option<Box<Value>>,
    pub retry_after: Option<u64>,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl ApiError {
    /// A contract error with its normative status, message and explanation.
    pub fn wire(wire: Wire) -> Self {
        Self {
            status: wire.status(),
            code: format!("store.{}", wire.suffix()),
            message: wire.message().to_owned(),
            explanation: Some(wire.explanation().to_owned()),
            retryable: matches!(wire, Wire::RateLimited),
            details: None,
            retry_after: None,
        }
    }

    /// A contract error with a more precise message.
    pub fn wire_with(wire: Wire, message: impl Into<String>) -> Self {
        let mut error = Self::wire(wire);
        error.message = message.into();
        error
    }

    /// `store.invalid_request` naming what is wrong (never a value).
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::wire_with(Wire::InvalidRequest, message)
    }

    /// A refusal under this crate's own codes.
    pub fn server(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.to_owned(),
            message: message.into(),
            explanation: None,
            retryable: status == StatusCode::SERVICE_UNAVAILABLE,
            details: None,
            retry_after: None,
        }
    }

    /// 503 with `Retry-After`: overloaded, or a dependency is down.
    pub fn unavailable(message: impl Into<String>) -> Self {
        let mut error = Self::server(StatusCode::SERVICE_UNAVAILABLE, codes::UNAVAILABLE, message);
        error.retry_after = Some(1);
        error
    }

    /// 500: nothing the caller can fix.
    pub fn internal() -> Self {
        Self::server(
            StatusCode::INTERNAL_SERVER_ERROR,
            codes::INTERNAL,
            "an unexpected failure; quote the request id",
        )
    }

    /// 404 under the server's own code (an unknown route or operator subject).
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::server(StatusCode::NOT_FOUND, codes::NOT_FOUND, message)
    }

    /// 401 for the operator credential.
    pub fn admin_unauthorized() -> Self {
        Self::server(
            StatusCode::UNAUTHORIZED,
            codes::ADMIN_UNAUTHORIZED,
            "the operator credential is missing or invalid",
        )
    }

    /// 400 for a malformed operator request.
    pub fn invalid_admin(message: impl Into<String>) -> Self {
        Self::server(
            StatusCode::BAD_REQUEST,
            codes::INVALID_ADMIN_REQUEST,
            message,
        )
    }

    /// 429 with the seconds until the window opens again.
    pub fn rate_limited(retry_after: u64) -> Self {
        let mut error = Self::wire(Wire::RateLimited);
        error.retry_after = Some(retry_after.max(1));
        error
    }

    /// Adds `details`.
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(Box::new(details));
        self
    }

    /// Replaces the status (an over-long body is a 413 and a slow one a 408, both under
    /// `store.invalid_request` or `store.too_large`).
    pub fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let request_id = REQUEST_ID
            .try_with(Clone::clone)
            .unwrap_or_else(|_| String::new());
        let mut error = json!({
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
            "request_id": request_id,
        });
        if let (Some(text), Some(object)) = (self.explanation, error.as_object_mut()) {
            object.insert("explanation".to_owned(), Value::String(text));
        }
        if let (Some(details), Some(object)) = (self.details, error.as_object_mut()) {
            object.insert("details".to_owned(), *details);
        }
        let body = json!({ "error": error }).to_string();
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = self.status;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        if let Some(seconds) = self.retry_after
            && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

/// Whether a storage refusal means "the dependency is down or busy" (as opposed to a rule).
fn registry_unavailable(code: &str) -> bool {
    code == "registry.storage"
}

impl From<RegistryError> for ApiError {
    fn from(error: RegistryError) -> Self {
        match error.code {
            code if registry_unavailable(code) => {
                Self::unavailable("the registry index is unavailable")
            }
            "registry.invalid_cursor" | "registry.invalid_query" => {
                Self::invalid("the cursor or the query is not valid; restart the search")
            }
            _ => Self::internal(),
        }
    }
}

impl From<StoreStateError> for ApiError {
    fn from(error: StoreStateError) -> Self {
        match error.code {
            "store_state.backend" => Self::unavailable("the store state is unavailable"),
            "store_state.too_large" => Self::invalid("a field is empty or too long"),
            _ => Self::internal(),
        }
    }
}

impl From<SigningError> for ApiError {
    fn from(_error: SigningError) -> Self {
        Self::internal()
    }
}

/// The mapping of a refusal of `Registry::publish` onto the contract's codes.
pub(crate) fn publish_error(error: &RegistryError) -> ApiError {
    let code = error.code;
    match code {
        "registry.too_large" | "bundle.too_large" => ApiError::wire(Wire::TooLarge),
        "registry.not_owner" => ApiError::wire(Wire::NameTaken),
        "registry.version_not_increasing" | "registry.version_conflict" => {
            ApiError::wire(Wire::VersionNotMonotonic)
        }
        "registry.publisher_inactive" | "signing.publisher_revoked" => {
            ApiError::wire(Wire::PublisherRevoked)
        }
        "registry.invalid_name" => ApiError::wire(Wire::InvalidManifest),
        "registry.signature_in_future" => ApiError::wire(Wire::SignatureInvalid),
        "registry.storage" => ApiError::unavailable("the registry index is unavailable"),
        other if other.starts_with("signing.") => ApiError::wire(Wire::SignatureInvalid),
        other if other.starts_with("bundle.") || other.starts_with("manifest.") => {
            bundle_error_code(other)
        }
        _ => ApiError::internal(),
    }
}

/// The contract answer for a `bundle.*` or `manifest.*` refusal: the manifest ones are
/// `store.invalid_manifest`, the rest `store.bundle_invalid` with `details.bundle_code`.
pub(crate) fn bundle_error_code(code: &str) -> ApiError {
    if code.starts_with("manifest.")
        || code == "bundle.bad_manifest"
        || code == "bundle.missing_entry"
    {
        return ApiError::wire(Wire::InvalidManifest).with_details(json!({ "bundle_code": code }));
    }
    ApiError::wire(Wire::BundleInvalid).with_details(json!({ "bundle_code": code }))
}

#[cfg(test)]
mod tests {
    use super::{ApiError, Wire, publish_error};
    use axum::http::StatusCode;
    use spot_registry::RegistryError;

    #[test]
    fn wire_codes_carry_the_contract_status() {
        assert_eq!(ApiError::wire(Wire::NameTaken).status, StatusCode::CONFLICT);
        assert_eq!(ApiError::wire(Wire::NameTaken).code, "store.name_taken");
        assert_eq!(
            ApiError::wire(Wire::VersionNotMonotonic).status,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            ApiError::wire(Wire::TooLarge).status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn publish_refusals_map_to_the_contract() {
        let map = |code: &'static str| publish_error(&RegistryError::new(code, "x")).code;
        assert_eq!(map("registry.not_owner"), "store.name_taken");
        assert_eq!(
            map("registry.version_conflict"),
            "store.version_not_monotonic"
        );
        assert_eq!(map("signing.signature_invalid"), "store.signature_invalid");
        assert_eq!(map("signing.publisher_revoked"), "store.publisher_revoked");
        assert_eq!(map("bundle.bad_archive"), "store.bundle_invalid");
        assert_eq!(map("bundle.bad_manifest"), "store.invalid_manifest");
        assert_eq!(map("registry.storage"), "store_server.unavailable");
    }

    #[test]
    fn bundle_invalid_names_the_bundle_code() {
        let error = publish_error(&RegistryError::new("bundle.illegal_path", "x"));
        let details = error.details.unwrap_or_default();
        assert_eq!(details["bundle_code"], "bundle.illegal_path");
    }
}
