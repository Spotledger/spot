//! OTLP setup (feature `otlp`).
#![cfg(feature = "otlp")]
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::Buf;
use spot_telemetry::{OtlpConfig, TelemetryConfig, init_for_tests_with};

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_endpoint_does_not_fail_init() {
    let buf = Buf::default();
    let config = TelemetryConfig {
        otlp: Some(OtlpConfig {
            endpoint: "http://127.0.0.1:1/v1/traces".to_owned(),
            sample_ppm: 1_000_000,
        }),
        ..TelemetryConfig::default()
    };
    let guard =
        init_for_tests_with(config, buf.clone()).expect("init must not depend on the collector");
    tracing::info_span!("work").in_scope(|| tracing::info!("still logging"));
    assert_eq!(buf.lines().len(), 1);
    // Dropping flushes against a dead collector: best effort, no panic, no hang.
    drop(guard);
}
