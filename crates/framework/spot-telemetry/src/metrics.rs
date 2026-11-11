//! Prometheus metrics through the `metrics` facade. No HTTP listener: a later `/metrics`
//! route serves [`render_prometheus`].
//!
//! Labels are never site ids, user ids or raw paths. `route` is the matched route template the
//! caller supplies, and `method` is folded to a fixed set.

use std::sync::OnceLock;
use std::time::Duration;

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

use crate::TelemetryError;

/// Counter: HTTP requests, labelled `method`, `route`, `status`.
pub const HTTP_REQUESTS_TOTAL: &str = "spot_http_requests_total";
/// Histogram: HTTP request duration in seconds, labelled `method`, `route`.
pub const HTTP_REQUEST_DURATION_SECONDS: &str = "spot_http_request_duration_seconds";
/// Counter: database statements run.
pub const DB_STATEMENTS_TOTAL: &str = "spot_db_statements_total";
/// Histogram: database statement time in seconds.
pub const DB_TIME_SECONDS: &str = "spot_db_time_seconds";

/// Counter: coordinator lease events, labelled `lease` (the kind, see [`lease_kind`]) and
/// `event` (`acquired`, `renewed`, `released`, `lost`, `fence_refused`, `error`).
pub const LEASE_EVENTS_TOTAL: &str = "spot_lease_events_total";
/// Gauge: 1 while this process holds a lease of the kind, else 0, labelled `lease`.
pub const LEASE_HELD: &str = "spot_lease_held";

/// Bucket upper bounds, in seconds, of [`HTTP_REQUEST_DURATION_SECONDS`]: 1 ms to 10 s, roughly
/// 2.5x apart, so a p99 of an interactive request and of a slow report both land inside.
pub const HTTP_DURATION_BUCKETS_SECONDS: [f64; 12] = [
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Bucket upper bounds, in seconds, of [`DB_TIME_SECONDS`]: 100 us to 2.5 s.
pub const DB_TIME_BUCKETS_SECONDS: [f64; 10] = [
    0.0001, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.1, 0.5, 2.5,
];

static HANDLE: OnceLock<Result<PrometheusHandle, String>> = OnceLock::new();

fn build() -> Result<PrometheusHandle, String> {
    let builder = PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(HTTP_REQUEST_DURATION_SECONDS.to_owned()),
            &HTTP_DURATION_BUCKETS_SECONDS,
        )
        .and_then(|b| {
            b.set_buckets_for_metric(
                Matcher::Full(DB_TIME_SECONDS.to_owned()),
                &DB_TIME_BUCKETS_SECONDS,
            )
        })
        .map_err(|e| e.to_string())?;
    let handle = builder.install_recorder().map_err(|e| e.to_string())?;
    metrics::describe_counter!(
        HTTP_REQUESTS_TOTAL,
        "HTTP requests by method, route template and status"
    );
    metrics::describe_histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "HTTP request duration"
    );
    metrics::describe_counter!(DB_STATEMENTS_TOTAL, "Database statements run");
    metrics::describe_histogram!(
        DB_TIME_SECONDS,
        metrics::Unit::Seconds,
        "Database statement time"
    );
    metrics::describe_counter!(
        LEASE_EVENTS_TOTAL,
        "Lease events by lease kind (consumer, scheduler, other) and event"
    );
    metrics::describe_gauge!(
        LEASE_HELD,
        "1 while this process holds a lease of the kind, else 0"
    );
    Ok(handle)
}

/// Installs the process-wide Prometheus recorder once. Later calls return the same outcome.
pub fn install() -> Result<(), TelemetryError> {
    match HANDLE.get_or_init(build) {
        Ok(_) => Ok(()),
        Err(reason) => Err(TelemetryError::Metrics(reason.clone())),
    }
}

/// The metrics in Prometheus text format; empty if no recorder is installed. This is what a
/// `/metrics` route serves.
pub fn render_prometheus() -> String {
    match HANDLE.get_or_init(build) {
        Ok(handle) => handle.render(),
        Err(_) => String::new(),
    }
}

/// Folds an HTTP method into a bounded label value.
pub(crate) fn method_label(method: &http::Method) -> &'static str {
    match *method {
        http::Method::GET => "GET",
        http::Method::POST => "POST",
        http::Method::PUT => "PUT",
        http::Method::PATCH => "PATCH",
        http::Method::DELETE => "DELETE",
        http::Method::HEAD => "HEAD",
        http::Method::OPTIONS => "OPTIONS",
        _ => "OTHER",
    }
}

pub(crate) fn record_http_request(
    method: &'static str,
    route: &'static str,
    status: u16,
    duration: Duration,
) {
    metrics::counter!(
        HTTP_REQUESTS_TOTAL,
        "method" => method,
        "route" => route,
        "status" => status.to_string()
    )
    .increment(1);
    metrics::histogram!(
        HTTP_REQUEST_DURATION_SECONDS,
        "method" => method,
        "route" => route
    )
    .record(duration.as_secs_f64());
}

pub(crate) fn record_db_statement(duration: Duration) {
    metrics::counter!(DB_STATEMENTS_TOTAL).increment(1);
    metrics::histogram!(DB_TIME_SECONDS).record(duration.as_secs_f64());
}

/// The bounded `lease` label for a lease name: `consumer` for the job consumer lease
/// (`serve:<site>:jobs`), `scheduler` for the schedule leader lease (`scheduler:<site>`) and
/// `other` for anything else. The name carries the site id, so it is never the label itself.
pub fn lease_kind(name: &str) -> &'static str {
    if name.starts_with("serve:") && name.ends_with(":jobs") {
        "consumer"
    } else if name.starts_with("scheduler:") {
        "scheduler"
    } else {
        "other"
    }
}

/// The events [`record_lease_event`] accepts; anything else is folded to `error`.
const LEASE_EVENTS: [&str; 6] = [
    "acquired",
    "renewed",
    "released",
    "lost",
    "fence_refused",
    "error",
];

/// Counts one lease event. `kind` is a [`lease_kind`] result; `event` one of `acquired`,
/// `renewed`, `released`, `lost`, `fence_refused`, `error` (other values count as `error`, so
/// the label set stays bounded). A no-op if no recorder is installed.
pub fn record_lease_event(kind: &'static str, event: &'static str) {
    let event = if LEASE_EVENTS.contains(&event) {
        event
    } else {
        "error"
    };
    metrics::counter!(LEASE_EVENTS_TOTAL, "lease" => kind, "event" => event).increment(1);
}

/// Sets the held gauge of a lease kind. A no-op if no recorder is installed.
pub fn set_lease_held(kind: &'static str, held: bool) {
    metrics::gauge!(LEASE_HELD, "lease" => kind).set(if held { 1.0 } else { 0.0 });
}
