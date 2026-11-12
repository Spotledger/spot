//! Small HTTP helpers shared by the handlers: response builders, conditional requests, path
//! parsing, rate-limit checks and bounded JSON bodies.

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use spot_registry::{AppName, Version};

use crate::error::ApiError;
use crate::metrics::Metrics;
use crate::ratelimit::Class;
use crate::service::Service;

/// A JSON response with `value` as the whole body.
pub(crate) fn json_response(status: StatusCode, value: &Value) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// A JSON response `{"data": value}`, the contract's success envelope.
pub(crate) fn data_response(status: StatusCode, value: Value) -> Response {
    json_response(status, &json!({ "data": value }))
}

/// Sets a header, ignoring a name or value that is not valid (none of ours is not).
pub(crate) fn set_header(response: &mut Response, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(value),
    ) {
        response.headers_mut().insert(name, value);
    }
}

/// Whether `If-None-Match` names `etag` (or `*`), weak tags compared by their opaque part.
pub(crate) fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    value.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
    })
}

/// A path segment as an app name; `store.invalid_request` otherwise (a name that is not
/// `[a-z][a-z0-9_]{0,62}`, `../x` included, never reaches a store or a file path).
pub(crate) fn parse_app(text: &str) -> Result<AppName, ApiError> {
    AppName::parse(text).map_err(|_| ApiError::invalid("the app name is not valid"))
}

/// A path segment as a semantic version.
pub(crate) fn parse_version(text: &str) -> Result<Version, ApiError> {
    Version::parse(text).map_err(|_| ApiError::invalid("the version is not a semantic version"))
}

/// Counts one call of `class` for `key`, answering `429` with `Retry-After` when it is over.
pub(crate) fn limit(service: &Service, class: Class, key: &str) -> Result<(), ApiError> {
    let now = service.core.clock.now();
    service.limiter.hit(class, key, now).map_err(|wait| {
        Metrics::bump(&service.core.metrics.rate_limited);
        ApiError::rate_limited(wait)
    })
}

/// Whether the request declares a JSON body.
fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
        })
}

fn body_error(admin: bool, message: &str) -> ApiError {
    if admin {
        ApiError::invalid_admin(message.to_owned())
    } else {
        ApiError::invalid(message.to_owned())
    }
}

async fn read_bytes(body: Body, max: usize, admin: bool) -> Result<axum::body::Bytes, ApiError> {
    axum::body::to_bytes(body, max).await.map_err(|_| {
        body_error(admin, "the body is unreadable or too large")
            .with_status(StatusCode::PAYLOAD_TOO_LARGE)
    })
}

/// Reads a JSON body of at most `max` bytes after checking its content type. `admin` selects the
/// operator route's error code.
pub(crate) async fn read_json<T: DeserializeOwned>(
    headers: &HeaderMap,
    body: Body,
    max: usize,
    admin: bool,
) -> Result<T, ApiError> {
    if !is_json(headers) {
        return Err(body_error(
            admin,
            "the content type must be application/json",
        ));
    }
    let bytes = read_bytes(body, max, admin).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| body_error(admin, "the body does not match the schema"))
}

/// As [`read_json`] for an operator route, but an empty body is `T::default()`.
pub(crate) async fn read_json_or_default<T: DeserializeOwned + Default>(
    headers: &HeaderMap,
    body: Body,
    max: usize,
) -> Result<T, ApiError> {
    let bytes = read_bytes(body, max, true).await?;
    if bytes.is_empty() {
        return Ok(T::default());
    }
    if !is_json(headers) {
        return Err(body_error(
            true,
            "the content type must be application/json",
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| body_error(true, "the body does not match the schema"))
}

/// `Idempotency-Key`: 1 to 128 visible ASCII characters, or none.
pub(crate) fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let text = value
        .to_str()
        .map_err(|_| ApiError::invalid("the Idempotency-Key is not visible ASCII"))?;
    let valid = !text.is_empty()
        && text.len() <= 128
        && text.bytes().all(|byte| (0x21..=0x7e).contains(&byte));
    if valid {
        Ok(Some(text.to_owned()))
    } else {
        Err(ApiError::invalid(
            "the Idempotency-Key must be 1 to 128 visible ASCII characters",
        ))
    }
}

/// An error answer for a range the content cannot satisfy: `416` with `Content-Range: bytes */size`.
pub(crate) fn unsatisfiable_range(size: u64) -> Response {
    use axum::response::IntoResponse;
    let mut response = ApiError::invalid("the requested range is outside the content")
        .with_status(StatusCode::RANGE_NOT_SATISFIABLE)
        .into_response();
    set_header(&mut response, "content-range", &format!("bytes */{size}"));
    response
}

/// A requested byte range of a bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slice {
    /// The whole content.
    Full,
    /// Bytes `start` to `end`, both inclusive and inside the content.
    Part { start: u64, end: u64 },
}

/// Reads a single-range `Range` header against a content of `size` bytes. A missing header or a
/// unit other than `bytes` is the whole content; a malformed, multiple or unsatisfiable range is
/// `Err(())`, answered `416`.
pub(crate) fn parse_range(headers: &HeaderMap, size: u64) -> Result<Slice, ()> {
    let Some(value) = headers.get(header::RANGE) else {
        return Ok(Slice::Full);
    };
    let text = value.to_str().map_err(|_| ())?;
    let Some(spec) = text.strip_prefix("bytes=") else {
        return Ok(Slice::Full);
    };
    if spec.contains(',') || size == 0 {
        return Err(());
    }
    let (first, last) = spec.split_once('-').ok_or(())?;
    let number = |digits: &str| digits.parse::<u64>().map_err(|_| ());
    let (start, end) = match (first.is_empty(), last.is_empty()) {
        (true, true) => return Err(()),
        (true, false) => {
            let count = number(last)?;
            if count == 0 {
                return Err(());
            }
            (size - count.min(size), size - 1)
        }
        (false, true) => (number(first)?, size - 1),
        (false, false) => (number(first)?, number(last)?.min(size - 1)),
    };
    if start > end || start >= size {
        return Err(());
    }
    Ok(Slice::Part { start, end })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn range(value: &str, size: u64) -> Result<Slice, ()> {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_str(value).unwrap());
        parse_range(&headers, size)
    }

    #[test]
    fn ranges_follow_rfc_9110() {
        assert_eq!(parse_range(&HeaderMap::new(), 10), Ok(Slice::Full));
        assert_eq!(range("bytes=0-3", 10), Ok(Slice::Part { start: 0, end: 3 }));
        assert_eq!(range("bytes=4-", 10), Ok(Slice::Part { start: 4, end: 9 }));
        assert_eq!(range("bytes=-3", 10), Ok(Slice::Part { start: 7, end: 9 }));
        assert_eq!(range("bytes=-30", 10), Ok(Slice::Part { start: 0, end: 9 }));
        assert_eq!(
            range("bytes=5-99", 10),
            Ok(Slice::Part { start: 5, end: 9 })
        );
        assert_eq!(range("items=1-2", 10), Ok(Slice::Full));
        assert!(range("bytes=10-", 10).is_err());
        assert!(range("bytes=5-2", 10).is_err());
        assert!(range("bytes=0-1,3-4", 10).is_err());
        assert!(range("bytes=-", 10).is_err());
        assert!(range("bytes=-0", 10).is_err());
        assert!(range("bytes=0-0", 0).is_err());
    }

    #[test]
    fn if_none_match_reads_lists_weak_tags_and_star() {
        let etag = "\"abc\"";
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::IF_NONE_MATCH, HeaderValue::from_str(value).unwrap());
            if_none_match(&headers, etag)
        };
        assert!(with("\"abc\""));
        assert!(with("\"x\", \"abc\""));
        assert!(with("W/\"abc\""));
        assert!(with("*"));
        assert!(!with("\"abd\""));
        assert!(!if_none_match(&HeaderMap::new(), etag));
    }

    #[test]
    fn idempotency_keys_are_visible_ascii_of_bounded_length() {
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("idempotency-key", HeaderValue::from_str(value).unwrap());
            idempotency_key(&headers)
        };
        assert_eq!(with("abc-123").unwrap().as_deref(), Some("abc-123"));
        assert!(with("has space").is_err());
        assert!(with(&"k".repeat(129)).is_err());
        assert_eq!(idempotency_key(&HeaderMap::new()).unwrap(), None);
    }

    #[test]
    fn app_names_that_are_paths_are_refused() {
        assert!(parse_app("invoice_reminders").is_ok());
        assert!(parse_app("../x").is_err());
        assert!(parse_app("a/b").is_err());
        assert!(parse_app("Upper").is_err());
        assert!(parse_app(&"a".repeat(64)).is_err());
        assert!(parse_version("1.4.0").is_ok());
        assert!(parse_version("1.4").is_err());
    }
}
