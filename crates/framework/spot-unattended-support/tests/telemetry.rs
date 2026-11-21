//! Telemetry: separate consent, aggregate counters only, payload equals the inspector.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Mutex;

use spot_unattended_support::consent::{Consent, ConsentKind, ConsentState};
use spot_unattended_support::telemetry::{
    BUCKET_BOUNDS_MS, InstallId, SCHEMA_VERSION, Telemetry, TelemetryBatch, Uploader,
};

struct Recording(Mutex<Vec<Vec<u8>>>);

impl Uploader for Recording {
    fn upload(&self, kind: &str, body: &[u8]) -> Result<(), String> {
        assert_eq!(kind, "telemetry");
        self.0.lock().expect("lock").push(body.to_vec());
        Ok(())
    }
}

fn on() -> ConsentState {
    let mut s = ConsentState::first_run();
    s.set(ConsentKind::Telemetry, Consent::On, 1);
    s
}

fn telemetry() -> Telemetry {
    Telemetry::new(InstallId::from_random([7; 16]), 100)
}

#[test]
fn nothing_is_counted_or_sent_without_consent() {
    let t = telemetry();
    let ask = ConsentState::first_run();
    t.record_feature(&ask, "posting.submit").expect("ok");
    t.record_error(&ask, "document.required").expect("ok");
    assert!(t.pending_payload(&ask, 200).is_none());
    // Crash consent alone does not switch telemetry on.
    let mut crash_only = ConsentState::first_run();
    crash_only.set(ConsentKind::Crash, Consent::On, 1);
    t.record_feature(&crash_only, "posting.submit").expect("ok");
    assert!(t.pending_payload(&crash_only, 200).is_none());
    // Counting after consent starts from zero: the earlier calls left nothing behind.
    t.record_feature(&on(), "posting.submit").expect("ok");
    let batch: TelemetryBatch =
        serde_json::from_slice(&t.pending_payload(&on(), 200).expect("payload")).expect("json");
    assert_eq!(batch.feature_usage.get("posting.submit"), Some(&1));
    assert!(batch.error_codes.is_empty());
}

#[test]
fn the_payload_is_aggregate_counters_with_a_schema_version_and_one_id() {
    let t = telemetry();
    let c = on();
    for _ in 0..3 {
        t.record_feature(&c, "report.run").expect("ok");
    }
    t.record_error(&c, "document.required").expect("ok");
    for ms in [5, 10, 11, 100, 101, 5_000, 5_001, 90_000] {
        t.record_duration_ms(&c, "list.load", ms).expect("ok");
    }
    let batch: TelemetryBatch =
        serde_json::from_slice(&t.pending_payload(&c, 250).expect("payload")).expect("json");
    assert_eq!(batch.schema_version, SCHEMA_VERSION);
    assert_eq!(batch.install_id, "07".repeat(16));
    assert_eq!((batch.period_start, batch.period_end), (100, 250));
    assert_eq!(batch.feature_usage.get("report.run"), Some(&3));
    assert_eq!(batch.error_codes.get("document.required"), Some(&1));
    let buckets = batch.performance.get("list.load").expect("buckets");
    assert_eq!(buckets.len(), BUCKET_BOUNDS_MS.len() + 1);
    assert_eq!(buckets, &vec![2, 1, 1, 1, 0, 1, 2]);
}

#[test]
fn names_that_could_carry_content_are_refused() {
    let t = telemetry();
    let c = on();
    for bad in [
        "",
        "Has Space",
        "a@b.c",
        "UPPER",
        "1starts_with_digit",
        &"x".repeat(65),
        "o'brien",
    ] {
        let err = t.record_feature(&c, bad).expect_err(bad);
        assert_eq!(err.code(), "unattended_support.name_invalid");
    }
    assert!(t.record_feature(&c, "a_b.c9").is_ok());
}

#[test]
fn the_distinct_name_count_is_bounded() {
    let t = telemetry();
    let c = on();
    for i in 0..600 {
        t.record_feature(&c, &format!("feature_{i}")).expect("ok");
    }
    let batch: TelemetryBatch =
        serde_json::from_slice(&t.pending_payload(&c, 1).expect("payload")).expect("json");
    assert_eq!(batch.feature_usage.len(), 512);
}

#[test]
fn the_upload_carries_exactly_what_the_inspector_shows_and_then_resets() {
    let t = telemetry();
    let c = on();
    t.record_feature(&c, "report.run").expect("ok");
    t.record_error(&c, "document.required").expect("ok");
    let shown = t.pending_payload(&c, 300).expect("payload");
    let up = Recording(Mutex::new(Vec::new()));
    assert!(t.upload(&c, &up, 300).expect("sent"));
    assert_eq!(up.0.lock().expect("lock").as_slice(), [shown]);
    assert!(
        t.pending_payload(&c, 301).is_none(),
        "counters restart after a send"
    );
    assert!(!t.upload(&c, &up, 302).expect("nothing to send"));
    t.record_feature(&c, "report.run").expect("ok");
    let batch: TelemetryBatch =
        serde_json::from_slice(&t.pending_payload(&c, 400).expect("payload")).expect("json");
    assert_eq!(batch.period_start, 300);
}

#[test]
fn a_failed_upload_keeps_the_counters() {
    struct Blocked;
    impl Uploader for Blocked {
        fn upload(&self, _: &str, _: &[u8]) -> Result<(), String> {
            Err("blocked by egress policy".to_owned())
        }
    }
    let t = telemetry();
    let c = on();
    t.record_feature(&c, "report.run").expect("ok");
    let before = t.pending_payload(&c, 500).expect("payload");
    let err = t.upload(&c, &Blocked, 500).expect_err("blocked");
    assert_eq!(err.code(), "unattended_support.upload_failed");
    assert_eq!(t.pending_payload(&c, 500).expect("still there"), before);
}
