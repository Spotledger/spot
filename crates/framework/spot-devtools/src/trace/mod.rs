//! One trace across the layers: the span model, the per-site ring collector, the telemetry sink
//! and the text renderer.
//!
//! A request enters at [`Layer::Http`], runs lifecycle steps ([`Layer::Lifecycle`]), may call
//! scripts ([`Layer::Script`]) and WASM apps ([`Layer::Wasm`]), which reach storage
//! ([`Layer::Storage`]) and jobs ([`Layer::Job`]). Every one of those starts a child span from the
//! parent's [`TraceContext`], so the trace is one tree rather than several logs.
//!
//! # Time is an argument
//!
//! Every `start`/`end` takes a [`std::time::Duration`] from [`spot_core::Clock::monotonic`]. This
//! crate never calls `Instant::now`, `SystemTime::now` or `OffsetDateTime::now_utc`: a test can
//! drive the whole trace at exact times.

pub mod collector;
pub mod render;
pub mod sink;
pub mod span;

pub use collector::{
    ActiveSpan, MAX_SPANS_PER_TRACE, SAMPLE_ALL_PPM, Trace, TraceCollector, TraceContext,
    TraceSummary,
};
pub use render::{format_duration, render_layers, render_summaries, render_tree};
pub use sink::{TelemetrySink, TraceSink};
pub use span::{Layer, SpanId, TraceId, TraceSpan, is_sensitive_attribute};
