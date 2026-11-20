//! The `tracing` layer that feeds the per-site [`TraceCollector`] from the spans the host and
//! the engine already emit, so the HTTP, lifecycle, pipeline-step, storage and job layers of one
//! request share one trace.
//!
//! The mapping is by span name (names are the host's own, fixed strings):
//!
//! | Span | Layer |
//! | --- | --- |
//! | `http_request`, `spot_http_request` | `Http` (a root, unless nested) |
//! | `doc.op`, `doc.phase` | `Lifecycle` |
//! | `doc.step` | `PipelineStep` |
//! | `storage.tx`, `storage.query` | `Storage` |
//! | `job.run` | `Job` |
//!
//! A span with another name is not traced. Attributes are the span's fields rendered as text;
//! the collector redacts every one of them before it is stored. Times come from the injected
//! clock, never the wall clock.

use std::sync::{Arc, Mutex};

use spot_core::Clock;
use spot_devtools::{ActiveSpan, Layer as TraceLayer, TraceCollector, TraceContext};
use tracing::span::{Attributes, Id};
use tracing::{Subscriber, field::Field, field::Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// The trace layer a span name maps to, or `None` for a span that is not traced.
#[must_use]
pub fn layer_of(name: &str) -> Option<TraceLayer> {
    match name {
        "http_request" | "spot_http_request" => Some(TraceLayer::Http),
        "doc.op" | "doc.phase" => Some(TraceLayer::Lifecycle),
        "doc.step" => Some(TraceLayer::PipelineStep),
        "storage.tx" | "storage.query" => Some(TraceLayer::Storage),
        "job.run" => Some(TraceLayer::Job),
        _ => None,
    }
}

/// What one traced span keeps in its extensions: the live collector span and its context.
struct Slot {
    active: Mutex<Option<ActiveSpan>>,
    context: Option<TraceContext>,
}

/// Collects a span's fields as `(name, text)` pairs.
#[derive(Default)]
struct Fields(Vec<(String, String)>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.push((field.name().to_owned(), format!("{value:?}")));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_owned(), value.to_owned()));
    }
}

/// The layer. Install it on the process subscriber (see the module guide: `spot-telemetry` has no
/// attach point yet).
pub struct CollectorLayer {
    collector: TraceCollector,
    clock: Arc<dyn Clock>,
}

impl CollectorLayer {
    /// A layer recording into `collector`, timing spans with `clock`.
    #[must_use]
    pub fn new(collector: TraceCollector, clock: Arc<dyn Clock>) -> Self {
        Self { collector, clock }
    }
}

impl<S> tracing_subscriber::Layer<S> for CollectorLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let name = attrs.metadata().name();
        let Some(layer) = layer_of(name) else {
            return;
        };
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        // The nearest traced ancestor is the parent; none makes this span a root.
        let parent = span.scope().skip(1).find_map(|ancestor| {
            ancestor
                .extensions()
                .get::<Slot>()
                .and_then(|slot| slot.context.clone())
        });
        let at = self.clock.monotonic();
        let active = match &parent {
            Some(parent) => self.collector.child(parent, layer, name, fields.0, at),
            None => self.collector.root(layer, name, fields.0, at),
        };
        let context = active.context();
        span.extensions_mut().insert(Slot {
            active: Mutex::new(Some(active)),
            context,
        });
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let slot = span.extensions_mut().remove::<Slot>();
        if let Some(slot) = slot {
            let taken = slot.active.lock().unwrap_or_else(|p| p.into_inner()).take();
            if let Some(active) = taken {
                active.end(self.clock.monotonic());
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_core::{ManualClock, SiteId};
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn span_names_map_to_layers() {
        assert_eq!(layer_of("doc.step"), Some(TraceLayer::PipelineStep));
        assert_eq!(layer_of("storage.query"), Some(TraceLayer::Storage));
        assert_eq!(layer_of("something.else"), None);
    }

    #[test]
    fn nested_spans_become_one_trace_with_a_parent_chain() {
        let collector = TraceCollector::new(SiteId::new("acme").unwrap(), 10);
        let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
        let subscriber =
            tracing_subscriber::registry().with(CollectorLayer::new(collector.clone(), clock));
        tracing::subscriber::with_default(subscriber, || {
            let http = tracing::info_span!("http_request", request_id = "r-1");
            let _http = http.enter();
            let op = tracing::info_span!("doc.op", operation = "create");
            let _op = op.enter();
            let query = tracing::info_span!("storage.query", kind = "insert");
            let _query = query.enter();
        });
        let trace = collector
            .by_request("r-1")
            .expect("one trace for the request");
        assert_eq!(trace.spans.len(), 3);
        let root = trace.root().expect("a root");
        assert_eq!(root.layer, TraceLayer::Http);
    }
}
