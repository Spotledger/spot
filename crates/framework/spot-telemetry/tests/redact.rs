//! Redaction.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::Buf;
use http::{HeaderMap, HeaderValue};
use spot_telemetry::{Redacted, init_for_tests, is_sensitive_header, loggable_headers};

#[test]
fn redacted_prints_stars() {
    let r = Redacted("hunter2");
    assert_eq!(format!("{r}"), "***");
    assert_eq!(format!("{r:?}"), "***");
    assert_eq!(
        format!("{:?}", Some(Redacted(String::from("pw")))),
        "Some(***)"
    );
}

#[test]
fn sensitive_names() {
    for name in [
        "Authorization",
        "cookie",
        "Set-Cookie",
        "X-Api-Key",
        "X-CSRF-Token",
        "x-amz-security-token",
        "My-Secret-Header",
        "proxy-authorization",
    ] {
        assert!(is_sensitive_header(name), "{name}");
    }
    for name in ["content-type", "accept", "x-request-id", "user-agent"] {
        assert!(!is_sensitive_header(name), "{name}");
    }
}

#[test]
fn sensitive_headers_never_reach_the_output() {
    let mut headers = HeaderMap::new();
    for (k, v) in [
        ("authorization", "Bearer s3cr3t-A"),
        ("cookie", "sid=s3cr3t-B"),
        ("set-cookie", "sid=s3cr3t-C"),
        ("x-api-key", "s3cr3t-D"),
        ("x-refresh-token", "s3cr3t-E"),
        ("x-client-secret", "s3cr3t-F"),
        ("accept", "text/plain"),
    ] {
        headers.insert(k, HeaderValue::from_static(v));
    }
    headers.insert("x-bin", HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap());
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    tracing::info!(headers = %loggable_headers(&headers), debug = ?loggable_headers(&headers), "got request");
    let text = buf.text();
    assert!(!text.contains("s3cr3t"), "leaked: {text}");
    assert!(text.contains("authorization=***"));
    assert!(text.contains("accept=\\\"text/plain\\\""));
    assert!(text.contains("x-bin=<binary>"));
}
