//! Observability for the one binary (blueprint WS3, WP-PLT-15).
//!
//! - [`init`] / [`init_for_tests`]: structured logs and tracing, with optional OTLP export
//!   (cargo feature `otlp`).
//! - [`metrics`]: Prometheus metrics, rendered by [`render_prometheus`] for a later `/metrics`
//!   route. No listener here.
//! - [`cost`]: per-request cost accounting that follows a request across `.await`s.
//! - [`TelemetryLayer`]: a tower layer for any `http` service; one access-log event per request.
//! - [`Redacted`] and [`loggable_headers`]: keep secrets out of logs.
//!
//! This crate does not depend on `spot-http`. The HTTP server inserts [`RouteTemplate`] and
//! [`SiteField`] into request extensions and applies the layer.

pub mod cost;
mod error;
mod init;
mod json;
mod layer;
pub mod metrics;
#[cfg(feature = "otlp")]
mod otlp;
mod redact;

pub use cost::RequestCost;
pub use error::TelemetryError;
pub use init::{
    FILTER_ENV, LogFormat, OtlpConfig, TelemetryConfig, TelemetryGuard, init, init_for_tests,
    init_for_tests_with,
};
pub use layer::{
    CANCELLED_STATUS, RouteTemplate, SiteField, TelemetryFuture, TelemetryLayer, TelemetryService,
    UNMATCHED_ROUTE,
};
pub use metrics::render_prometheus;
pub use redact::{LoggableHeaders, Redacted, is_sensitive_header, loggable_headers};
