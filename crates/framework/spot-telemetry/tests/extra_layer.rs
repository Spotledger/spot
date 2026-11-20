//! A second layer attached to the subscriber (WP-DX-03.T4): what it sees, under which filter, and
//! that without one nothing changes.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::{Arc, Mutex};

use common::Buf;
use spot_telemetry::{
    BoxLayer, Redacted, TelemetryConfig, init_for_tests_with, init_for_tests_with_layer,
};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const MARKER: &str = "MARKER-SECRET-0123";

/// Records every event as `level target message k=v ...`.
#[derive(Clone, Default)]
struct Capture {
    seen: Arc<Mutex<Vec<String>>>,
}

struct Rendered(String);

impl Visit for Rendered {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!(" {}={value:?}", field.name()));
    }
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut text = Rendered(format!("{}", event.metadata().level()));
        event.record(&mut text);
        self.seen.lock().unwrap().push(text.0);
    }
}

impl Capture {
    fn as_boxed_layer(&self) -> BoxLayer {
        Box::new(self.clone())
    }

    fn lines(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn config(filter: &str) -> TelemetryConfig {
    TelemetryConfig {
        filter: filter.to_owned(),
        ..TelemetryConfig::default()
    }
}

#[test]
fn the_extra_layer_receives_events_and_a_redacted_value_is_already_masked() {
    let buf = Buf::default();
    let capture = Capture::default();
    let _guard =
        init_for_tests_with_layer(config("trace"), buf.clone(), Some(capture.as_boxed_layer()))
            .unwrap();
    tracing::info!(token = %Redacted(MARKER), answer = 42u64, "signed in");
    let lines = capture.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("signed in"), "{lines:?}");
    assert!(lines[0].contains("***"), "{lines:?}");
    assert!(
        !lines[0].contains(MARKER),
        "the marker secret reached the extra layer: {lines:?}"
    );
    assert!(
        !buf.text().contains(MARKER),
        "and the log line stays clean too"
    );
}

#[test]
fn the_log_output_is_unchanged_by_the_extra_layer() {
    let plain = Buf::default();
    let with = Buf::default();
    {
        let _guard = init_for_tests_with(config("trace"), plain.clone()).unwrap();
        tracing::warn!(answer = 1u64, "same");
    }
    {
        let capture = Capture::default();
        let _guard = init_for_tests_with_layer(
            config("trace"),
            with.clone(),
            Some(capture.as_boxed_layer()),
        )
        .unwrap();
        tracing::warn!(answer = 1u64, "same");
    }
    let (a, b) = (plain.lines(), with.lines());
    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);
    assert_eq!(a[0]["message"], b[0]["message"]);
    assert_eq!(a[0]["answer"], b[0]["answer"]);
}

#[test]
fn the_filter_applies_to_the_extra_layer_too() {
    let capture = Capture::default();
    let _guard = init_for_tests_with_layer(
        config("warn"),
        Buf::default(),
        Some(capture.as_boxed_layer()),
    )
    .unwrap();
    tracing::info!("below the filter");
    tracing::warn!("at the filter");
    let lines = capture.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("at the filter"));
}

#[test]
fn without_an_extra_layer_nothing_is_attached() {
    let capture = Capture::default();
    // A layer that was never handed to the subscriber sees nothing (the non-dev start).
    let _guard = init_for_tests_with_layer(config("trace"), Buf::default(), None).unwrap();
    tracing::info!("nobody listens");
    assert!(capture.lines().is_empty());
}

#[test]
fn a_bad_filter_is_still_an_error_with_an_extra_layer() {
    let capture = Capture::default();
    assert!(
        init_for_tests_with_layer(
            config("not a [valid filter"),
            Buf::default(),
            Some(capture.as_boxed_layer())
        )
        .is_err()
    );
}
