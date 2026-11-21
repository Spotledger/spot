//! A storage `Unavailable` (pool wait exceeded, backend unreachable) reaches the client as
//! `503 server.unavailable` with a `Retry-After`, never `500 http.internal`.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs, dead_code)]

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use spot_http::api::{ApiError, Code};
use spot_storage_contract::{ErrorKind, StorageError};

use spot_http::api::storage_error::{retry_after_secs, storage_error_to_api};

/// What a router does with the error: status from the registry, `Retry-After` from the detail.
fn render(error: &ApiError) -> (StatusCode, Option<HeaderValue>, serde_json::Value) {
    let body = error.to_envelope_json("req-1");
    let header = retry_after_secs(error).map(HeaderValue::from);
    let status = error.status();
    let mut response = (status, axum::Json(body.clone())).into_response();
    if let Some(value) = &header {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, value.clone());
    }
    (
        response.status(),
        response.headers().get(header::RETRY_AFTER).cloned(),
        body,
    )
}

fn unavailable() -> StorageError {
    StorageError::new(
        ErrorKind::Unavailable,
        "pool wait of 5s exceeded for postgres://app:hunter2@db-7.internal:5432/spot (max 32)",
    )
}

#[test]
fn unavailable_is_503_with_retry_after() {
    let error = storage_error_to_api(&unavailable(), None);
    let (status, header, body) = render(&error);
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(header.unwrap(), "1");
    assert_eq!(body["error"]["code"], "server.unavailable");
    assert_eq!(body["error"]["retryable"], true);
    assert_eq!(
        error.code,
        spot_http::api::ErrorCode::Registry(Code::ServerUnavailable)
    );
}

#[test]
fn a_larger_wait_is_kept_and_never_shortened() {
    let (_, header, _) = render(&storage_error_to_api(&unavailable(), Some(30)));
    assert_eq!(header.unwrap(), "30");
    let (_, header, _) = render(&storage_error_to_api(&unavailable(), Some(0)));
    assert_eq!(header.unwrap(), "1");
}

#[test]
fn other_storage_errors_keep_their_mapping() {
    for kind in [
        ErrorKind::NotFound,
        ErrorKind::Internal,
        ErrorKind::LockTimeout,
        ErrorKind::CommitOutcomeUnknown,
        ErrorKind::SerializationFailure,
    ] {
        let error = storage_error_to_api(&StorageError::new(kind, "x"), Some(9));
        let (status, header, body) = render(&error);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{kind:?}");
        assert!(header.is_none(), "{kind:?}");
        assert_eq!(body["error"]["code"], "http.internal", "{kind:?}");
        assert_eq!(body["error"]["retryable"], false, "{kind:?}");
    }
}

#[test]
fn the_body_names_no_connection_detail() {
    let (_, _, body) = render(&storage_error_to_api(&unavailable(), None));
    let text = body.to_string();
    for leak in ["postgres", "hunter2", "db-7", "5432", "pool", "32", "5s"] {
        assert!(!text.contains(leak), "the body leaks `{leak}`: {text}");
    }
}
