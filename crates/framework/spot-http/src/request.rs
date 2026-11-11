//! Request identification. See the blueprint: every request gets exactly one `X-Request-Id`,
//! echoing a trusted client-provided value when valid, generating a UUIDv7 otherwise.

use axum::extract::Request;
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use uuid::Uuid;

/// The logical header name. See the blueprint: `X-Request-Id`.
pub const REQUEST_ID_HEADER: &str = "X-Request-Id";

/// Wire header name (lowercase for `HeaderName::from_static`).
pub(crate) const REQUEST_ID_WIRE: &str = "x-request-id";

/// Validates and carries the id for one request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestId(String);

impl RequestId {
    /// Accepts a caller-provided id when it is 1..=128 visible ASCII characters
    /// (0x21..=0x7E); rejects it otherwise, without panicking.
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let value = value.as_ref();
        if value.is_empty() || value.len() > 128 {
            return None;
        }
        if value.bytes().all(|b| (0x21..=0x7E).contains(&b)) {
            Some(RequestId(value.to_owned()))
        } else {
            None
        }
    }

    /// Generates a new id as a UUIDv7 string.
    pub fn generate() -> Self {
        RequestId(Uuid::now_v7().to_string())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Accepts the incoming `X-Request-Id` when valid; generates one otherwise.
pub(crate) fn accept_or_generate<B>(request: &axum::http::Request<B>) -> RequestId {
    let incoming = request
        .headers()
        .get(REQUEST_ID_WIRE)
        .and_then(|value| value.to_str().ok())
        .and_then(RequestId::new);
    incoming.unwrap_or_else(RequestId::generate)
}

/// Outermost middleware: pins the request id into the extensions and stamps it on the response.
pub(crate) async fn request_id_middleware(request: Request, next: Next) -> Response {
    let id = accept_or_generate(&request);
    let mut request = request;
    request.extensions_mut().insert(id.clone());
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(id.as_str()) {
        response
            .headers_mut()
            .insert(header::HeaderName::from_static(REQUEST_ID_WIRE), value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Must;
    use axum::http::HeaderName;

    fn header_name() -> HeaderName {
        HeaderName::from_static(REQUEST_ID_WIRE)
    }

    #[test]
    fn accepts_valid_request_id() {
        let id = RequestId::new("client-42").must();
        assert_eq!(id.as_str(), "client-42");
    }

    #[test]
    fn rejects_129_chars() {
        let too_long = "x".repeat(129);
        assert!(RequestId::new(too_long).is_none());
    }

    #[test]
    fn rejects_non_ascii_chars() {
        assert!(RequestId::new("has space").is_none());
        assert!(RequestId::new("has\ttab").is_none());
        assert!(RequestId::new("héllo").is_none());
        assert!(RequestId::new("id+ümlaut").is_none());
    }

    #[test]
    fn rejects_empty_string() {
        assert!(RequestId::new("").is_none());
    }

    #[test]
    fn accepts_upper_ascii() {
        let id = RequestId::new("ABC!123").must();
        assert_eq!(id.as_str(), "ABC!123");
    }

    #[test]
    fn accepts_max_length() {
        let exactly = "x".repeat(128);
        assert!(RequestId::new(exactly).is_some());
    }

    #[test]
    fn generated_request_id_is_uuid_v7() {
        let id = RequestId::generate();
        let parsed = Uuid::parse_str(id.as_str()).must();
        assert_eq!(parsed.get_version_num(), 7);
    }

    #[test]
    fn accept_or_generate_echoes_valid_header() {
        let request = Request::builder()
            .header(header_name(), "from-client")
            .body(())
            .must();
        assert_eq!(accept_or_generate(&request).as_str(), "from-client");
    }

    #[test]
    fn accept_or_generate_overrides_invalid_header() {
        let request = Request::builder()
            .header(header_name(), "bad value with spaces")
            .body(())
            .must();
        let id = accept_or_generate(&request);
        assert_ne!(id.as_str(), "bad value with spaces");
        assert!(Uuid::parse_str(id.as_str()).is_ok());
    }

    #[test]
    fn accept_or_generate_creates_when_header_missing() {
        let request = Request::builder().body(()).must();
        let id = accept_or_generate(&request);
        assert!(Uuid::parse_str(id.as_str()).is_ok());
    }
}
