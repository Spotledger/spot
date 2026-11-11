//! Lease metrics: the bounded `lease` label and the recorded series.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use spot_telemetry::metrics::{
    install, lease_kind, record_lease_event, render_prometheus, set_lease_held,
};

#[test]
fn lease_kind_maps_the_real_lease_names() {
    // The formats of `consumer_lease` and `scheduler_lease` in spot-cli's services/jobs.rs.
    assert_eq!(lease_kind("serve:acme:jobs"), "consumer");
    assert_eq!(lease_kind("scheduler:acme"), "scheduler");
    assert_eq!(lease_kind("lock:something"), "other");
    assert_eq!(lease_kind("serve:acme:other"), "other");
    assert_eq!(lease_kind(""), "other");
}

fn count(text: &str, prefix: &str) -> f64 {
    text.lines()
        .find(|l| l.starts_with(prefix))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

// One test: the recorder is process-global.
#[test]
fn events_and_gauge_render_with_bounded_labels() {
    install().unwrap();
    let key = r#"spot_lease_events_total{lease="scheduler",event="acquired"}"#;
    let before = count(&render_prometheus(), key);
    record_lease_event("scheduler", "acquired");
    record_lease_event("scheduler", "acquired");
    record_lease_event("consumer", "bogus-event");
    set_lease_held("scheduler", true);
    let text = render_prometheus();
    assert_eq!(count(&text, key) - before, 2.0);
    assert!(text.contains(r#"spot_lease_events_total{lease="consumer",event="error"}"#));
    assert_eq!(count(&text, r#"spot_lease_held{lease="scheduler"}"#), 1.0);
    set_lease_held("scheduler", false);
    assert_eq!(
        count(
            &render_prometheus(),
            r#"spot_lease_held{lease="scheduler"}"#
        ),
        0.0
    );
    let text = render_prometheus();
    for line in text.lines().filter(|l| l.contains("spot_lease")) {
        assert!(
            !line.contains("acme"),
            "a lease label carries a site: {line}"
        );
    }
}
