//! JSON log shape, filters and setup errors.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::Buf;
use spot_telemetry::{
    LogFormat, TelemetryConfig, TelemetryError, init_for_tests, init_for_tests_with,
};

#[test]
fn json_line_has_required_keys_and_inherits_span_fields() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let span = tracing::info_span!(
        "req",
        site = "acme.example",
        request_id = "r-1",
        trace_id = "abc"
    );
    span.in_scope(|| tracing::warn!(answer = 42u64, name = "x\"y", "hello {}", "world"));
    let lines = buf.lines();
    assert_eq!(lines.len(), 1);
    let line = &lines[0];
    assert_eq!(line["level"], "warn");
    assert_eq!(line["message"], "hello world");
    assert!(
        line["target"].starts_with("logs"),
        "target {}",
        line["target"]
    );
    assert_eq!(line["site"], "acme.example");
    assert_eq!(line["request_id"], "r-1");
    assert_eq!(line["trace_id"], "abc");
    assert_eq!(line["span_id"].len(), 16);
    assert_eq!(line["answer"], "42");
    assert_eq!(line["name"], "x\"y");
    // RFC 3339 UTC.
    let ts = &line["ts"];
    assert!(
        ts.ends_with('Z') && ts.contains('T') && ts.len() >= 20,
        "ts {ts}"
    );
}

#[test]
fn event_outside_any_span_has_no_span_keys() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    tracing::info!("plain");
    let line = &buf.lines()[0];
    for key in ["site", "request_id", "trace_id", "span_id"] {
        assert!(!line.contains_key(key), "unexpected {key}");
    }
}

#[test]
fn inner_span_overrides_outer_and_event_overrides_span() {
    let buf = Buf::default();
    let _guard = init_for_tests(buf.clone());
    let outer = tracing::info_span!("outer", site = "outer.example", k = "outer");
    let _o = outer.enter();
    let inner = tracing::info_span!("inner", site = "inner.example");
    inner.in_scope(|| tracing::info!(k = "event", "m"));
    let line = &buf.lines()[0];
    assert_eq!(line["site"], "inner.example");
    assert_eq!(line["k"], "event");
}

#[test]
fn filter_is_honoured() {
    let buf = Buf::default();
    let config = TelemetryConfig {
        filter: "warn,noisy=off".to_owned(),
        ..TelemetryConfig::default()
    };
    let _guard = init_for_tests_with(config, buf.clone()).unwrap();
    tracing::info!("dropped");
    tracing::warn!("kept");
    tracing::error!(target: "noisy", "silenced");
    let lines = buf.lines();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["message"], "kept");
}

#[test]
fn invalid_filter_is_a_typed_error() {
    let config = TelemetryConfig {
        filter: "info,=bad[".to_owned(),
        ..TelemetryConfig::default()
    };
    let err = init_for_tests_with(config, Buf::default()).err().unwrap();
    assert!(
        matches!(err, TelemetryError::InvalidFilter { ref filter, .. } if filter == "info,=bad[")
    );
}

#[cfg(not(feature = "otlp"))]
#[test]
fn otlp_without_the_feature_is_a_typed_error() {
    let config = TelemetryConfig {
        otlp: Some(spot_telemetry::OtlpConfig {
            endpoint: "http://127.0.0.1:1".to_owned(),
            sample_ppm: 1,
        }),
        ..TelemetryConfig::default()
    };
    let err = init_for_tests_with(config, Buf::default()).err().unwrap();
    assert!(matches!(err, TelemetryError::OtlpNotCompiled));
}

#[test]
fn pretty_format_writes_human_readable_output() {
    let buf = Buf::default();
    let config = TelemetryConfig {
        format: LogFormat::Pretty,
        ..TelemetryConfig::default()
    };
    let _guard = init_for_tests_with(config, buf.clone()).unwrap();
    tracing::info!("pretty message");
    let text = buf.text();
    assert!(text.contains("pretty message"));
    assert!(!text.starts_with('{'));
}
