//! A sign-in that fails because storage is unavailable returns the same `server.unavailable`
//! envelope, keeps its friendly message and gains the retry hint.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use spot_http::api::{Code, ErrorCode};
use spot_login::{LoginError, LoginErrorKind};

fn unavailable() -> LoginError {
    LoginError::store_unavailable("session load: pool wait exceeded")
}

#[test]
fn unavailable_sign_in_is_server_unavailable_with_a_retry_hint() {
    let error = unavailable().to_api_error();
    assert_eq!(error.code, ErrorCode::Registry(Code::ServerUnavailable));
    assert_eq!(
        error.message,
        "The sign-in service is temporarily unavailable."
    );
    assert!(error.retryable());
    assert_eq!(
        error.details.get("retry_after").and_then(|v| v.as_u64()),
        Some(1)
    );
    assert_eq!(error.status().as_u16(), 503);
}

#[test]
fn the_envelope_carries_no_connection_detail() {
    let body = unavailable()
        .to_api_error()
        .to_envelope_json("req-9")
        .to_string();
    assert!(!body.contains("pool"), "{body}");
    assert!(!body.contains("unavailable: "), "{body}");
}

#[test]
fn other_store_failures_stay_internal() {
    let other = LoginError::new(
        LoginErrorKind::StoreFailed,
        "session load: storage.internal: bug",
    );
    let error = other.to_api_error();
    assert_eq!(error.code, ErrorCode::Registry(Code::HttpInternal));
    assert_eq!(
        error.message,
        "The sign-in service is temporarily unavailable."
    );
    assert!(!other.is_unavailable());
}

#[test]
fn text_that_looks_like_the_storage_code_does_not_make_a_failure_retryable() {
    for detail in [
        "session load: storage.unavailable: pool wait exceeded",
        "find user: storage.unavailable",
        "storage.unavailable",
    ] {
        let ordinary = LoginError::new(LoginErrorKind::StoreFailed, detail);
        assert!(!ordinary.is_unavailable(), "{detail}");
        let error = ordinary.to_api_error();
        assert_eq!(error.code, ErrorCode::Registry(Code::HttpInternal));
        assert_eq!(error.status().as_u16(), 500);
    }
}

#[test]
fn only_a_store_failure_can_be_unavailable() {
    let mut other = LoginError::new(LoginErrorKind::InvalidCredentials, "x");
    other.retryable_after = Some(1);
    assert!(!other.is_unavailable());
}

#[test]
fn a_store_failure_is_logged_once_with_the_request_id() {
    let mut lines = Vec::new();
    let _ = unavailable().to_api_error_logged("req-7", &mut |l| lines.push(l.to_owned()));
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("request_id=req-7"));
    let mut none = Vec::new();
    let bad = LoginError::new(LoginErrorKind::InvalidCredentials, "x");
    let _ = bad.to_api_error_logged("req-8", &mut |l| none.push(l.to_owned()));
    assert!(none.is_empty());
}
