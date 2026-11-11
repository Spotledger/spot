//! Where a finished span goes besides the ring: the telemetry pipeline.
//!
//! `spot-telemetry` installs the process-wide `tracing` subscriber (JSON lines, OTLP when the
//! feature is on) and owns the sinks; this crate does not depend on them. It emits each finished
//! span on that pipeline through [`TraceSink`], so whichever subscriber `spot-telemetry` installed
//! receives it without `spot-devtools` knowing about the exporter. [`TelemetrySink`] is the
//! implementation to wire in production:
//!
//! ```text
//! debug!(target: "spot_devtools::trace", trace_id, span_id, parent, layer, name,
//!        duration_us, request_id, error, "trace span")
//! ```
//!
//! A host that wants spans elsewhere (a test buffer, a second collector) implements
//! [`TraceSink`] itself.

use crate::trace::collector::Trace;
use crate::trace::span::{TraceSpan, micros};

/// One destination of finished spans.
pub trait TraceSink: Send + Sync {
    /// A span ended and is now in the collector's ring.
    fn span_finished(&self, span: &TraceSpan, request_id: Option<&str>);

    /// A whole trace ended (the root span finished). The default does nothing.
    fn trace_finished(&self, _trace: &Trace) {}
}

/// The `tracing` sink: the subscriber `spot-telemetry` installs receives one debug event per span.
#[derive(Clone, Copy, Debug, Default)]
pub struct TelemetrySink;

impl TraceSink for TelemetrySink {
    fn span_finished(&self, span: &TraceSpan, request_id: Option<&str>) {
        let duration_us = span.duration().map_or(0, micros);
        let parent = span.parent.map(|p| p.as_hex()).unwrap_or_default();
        tracing::debug!(
            target: "spot_devtools::trace",
            trace_id = %span.trace_id,
            span_id = %span.span_id.as_hex(),
            parent_span_id = %parent,
            layer = span.layer.as_str(),
            name = %span.name,
            duration_us,
            request_id = request_id.unwrap_or(""),
            error = span.error.as_deref().unwrap_or(""),
            "trace span"
        );
    }

    fn trace_finished(&self, trace: &Trace) {
        let duration_us = trace.duration().map_or(0, micros);
        tracing::debug!(
            target: "spot_devtools::trace",
            trace_id = %trace.trace_id,
            request_id = trace.request_id.as_deref().unwrap_or(""),
            spans = trace.spans.len(),
            duration_us,
            errors = trace.error_count(),
            "trace finished"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use spot_telemetry::init_for_tests_with;
    use tracing_subscriber::fmt::MakeWriter;

    use super::TelemetrySink;
    use crate::trace::collector::TraceCollector;
    use crate::trace::span::Layer;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            if let Ok(mut buffer) = self.0.lock() {
                buffer.extend_from_slice(data);
            }
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn a_finished_span_reaches_the_telemetry_pipeline() {
        let captured = Buf::default();
        let _guard = init_for_tests_with(
            spot_telemetry::TelemetryConfig {
                filter: "trace".into(),
                ..spot_telemetry::TelemetryConfig::default()
            },
            captured.clone(),
        );
        let collector =
            TraceCollector::new(spot_core::SiteId::new("acme.example").expect("site id"), 4)
                .with_sink(Arc::new(TelemetrySink));
        let span = collector.root(
            Layer::Http,
            "POST /api/v1/x",
            [("request_id".into(), "r-7".into())],
            std::time::Duration::from_millis(1),
        );
        span.end(std::time::Duration::from_millis(3));

        let text = String::from_utf8(captured.0.lock().expect("buffer").clone()).expect("utf-8");
        let line = text.lines().next().expect("one line");
        let json: serde_json::Value = serde_json::from_str(line).expect("json line");
        assert_eq!(json["target"], "spot_devtools::trace");
        assert_eq!(json["message"], "trace span");
        assert_eq!(json["request_id"], "r-7");
        assert_eq!(json["layer"], "http");
        assert_eq!(json["duration_us"], 2000);
    }

    #[test]
    fn a_test_sink_can_count_spans() {
        let collector = TraceCollector::new(spot_core::SiteId::new("acme.example").expect("id"), 4);
        let span = collector.root(Layer::Job, "j", [], std::time::Duration::ZERO);
        span.end(std::time::Duration::from_millis(1));
        assert_eq!(collector.summaries().len(), 1);
        let _ = TelemetrySink;
    }
}
