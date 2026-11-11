//! [`TraceCollector`]: the ring buffer of traces of one development site, and the handles that
//! record spans into it.
//!
//! One collector is the site's recorder. A root span starts a trace ([`TraceCollector::root`]);
//! every layer below passes the parent's [`TraceContext`] and starts a child
//! ([`TraceCollector::child`]). A `TraceContext` is `Clone + Send`, so an execution thread (a
//! script run, a WASM call) carries it across the boundary and the spans still nest.
//!
//! Sampling is 100 % by default — a dev site wants every span — and is expressed in parts per
//! million, never as a float. The ring keeps the last `capacity` traces of the site, which bounds
//! memory whatever a runaway script does.
//!
//! Every attribute value goes through [`spot_redact::Redactor`] when it is set, and an attribute
//! whose *name* is sensitive ([`crate::trace::is_sensitive_attribute`]) is dropped outright: a
//! token never reaches the buffer, the viewer or the sink, whatever the caller passes.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use spot_core::SiteId;
use spot_redact::{RedactionPolicy, Redactor};

use super::sink::TraceSink;
use super::span::{Layer, SpanId, TraceId, TraceSpan, is_sensitive_attribute, micros};

/// The most spans one trace keeps. Beyond it, further spans are dropped and the trace records
/// `trace.truncated` once, so a broken loop cannot grow the buffer without bound.
pub const MAX_SPANS_PER_TRACE: usize = 4096;

/// Sampling decision for a dev site: every trace.
pub const SAMPLE_ALL_PPM: u32 = 1_000_000;

/// A recorded trace: every span of one trace id, in the order the spans ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trace {
    /// The trace id.
    pub trace_id: TraceId,
    /// The spans.
    pub spans: Vec<TraceSpan>,
    /// The request id, if the trace carries one (the `request_id` attribute).
    pub request_id: Option<String>,
}

impl Trace {
    /// The root span (the earliest span with no parent), if any.
    #[must_use]
    pub fn root(&self) -> Option<&TraceSpan> {
        let mut roots: Vec<&TraceSpan> = self.spans.iter().filter(|s| s.parent.is_none()).collect();
        roots.sort_by_key(|s| (s.start, s.span_id));
        roots.first().copied().or_else(|| self.spans.first())
    }

    /// The trace's wall time: from the earliest start to the latest end.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        let start = self.spans.iter().map(|s| s.start).min()?;
        let end = self.spans.iter().filter_map(|s| s.end).max()?;
        Some(end.saturating_sub(start))
    }

    /// How many spans carry an error.
    #[must_use]
    pub fn error_count(&self) -> usize {
        self.spans.iter().filter(|s| s.error.is_some()).count()
    }

    /// The span with `span_id`.
    #[must_use]
    pub fn span(&self, span_id: SpanId) -> Option<&TraceSpan> {
        self.spans.iter().find(|s| s.span_id == span_id)
    }

    /// The original clock origin of the trace: the earliest span start.
    #[must_use]
    pub fn origin(&self) -> Duration {
        self.spans
            .iter()
            .map(|s| s.start)
            .min()
            .unwrap_or(Duration::ZERO)
    }

    /// The trace as JSON (integers only).
    #[must_use]
    pub fn to_json(&self) -> Value {
        let origin = self.origin();
        let spans: Vec<Value> = self.spans.iter().map(|s| s.to_json(origin)).collect();
        serde_json::json!({
            "traceId": self.trace_id.to_string(),
            "requestId": self.request_id,
            "durationMicros": self.duration().map_or(0, micros),
            "errorCount": self.error_count(),
            "spans": spans,
        })
    }
}

/// A one-line summary of a trace, for the trace list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceSummary {
    /// The trace id.
    pub trace_id: TraceId,
    /// The root span's name, if known.
    pub root: String,
    /// The root span's layer.
    pub layer: Layer,
    /// How many spans the trace holds.
    pub span_count: usize,
    /// The trace's total time.
    pub duration: Duration,
    /// The request id, if any.
    pub request_id: Option<String>,
    /// How many spans carry an error.
    pub error_count: usize,
}

impl Trace {
    /// The summary of this trace.
    #[must_use]
    pub fn summary(&self) -> TraceSummary {
        let root = self.root();
        TraceSummary {
            trace_id: self.trace_id.clone(),
            root: root.map(|s| s.name.clone()).unwrap_or_default(),
            layer: root.map_or(Layer::Http, |s| s.layer),
            span_count: self.spans.len(),
            duration: self.duration().unwrap_or_default(),
            request_id: self.request_id.clone(),
            error_count: self.error_count(),
        }
    }
}

/// The parent of the next span: the trace id, the parent span id and whether the trace is kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceContext {
    /// The trace id.
    pub trace_id: TraceId,
    /// The parent span's id.
    pub span_id: SpanId,
    /// Whether the trace is sampled (a span started from an unsampled context records nothing).
    pub sampled: bool,
}

struct State {
    traces: VecDeque<Trace>,
    /// Spans that have started but not ended yet, so a query during a pause sees the trace.
    open: BTreeMap<SpanId, TraceSpan>,
    truncated: Vec<TraceId>,
}

struct Inner {
    site: SiteId,
    capacity: usize,
    sampling_ppm: AtomicU32,
    redactor: Option<Arc<Redactor>>,
    sink: Mutex<Option<Arc<dyn TraceSink>>>,
    state: Mutex<State>,
    trace_seq: AtomicU64,
    span_seq: AtomicU64,
    sample_seq: AtomicU64,
}

/// The trace recorder of one site. Cheap to clone; every clone records into the same ring.
#[derive(Clone)]
pub struct TraceCollector {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for TraceCollector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceCollector")
            .field("site", &self.inner.site)
            .field("capacity", &self.inner.capacity)
            .field(
                "sampling_ppm",
                &self.inner.sampling_ppm.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl TraceCollector {
    /// A collector for `site`, keeping the last `capacity` traces (at least 1), sampling every
    /// trace and using the default redaction policy.
    #[must_use]
    pub fn new(site: SiteId, capacity: usize) -> Self {
        let redactor = Redactor::from_policy(&RedactionPolicy::default()).ok();
        Self::with_redactor(site, capacity, redactor.map(Arc::new))
    }

    /// A collector with the site's own redactor, keeping the last `capacity` traces (at least 1).
    /// With `None`, only the sensitive-name filter applies (no redactor could be built).
    #[must_use]
    pub fn with_redactor(site: SiteId, capacity: usize, redactor: Option<Arc<Redactor>>) -> Self {
        Self {
            inner: Arc::new(Inner {
                site,
                capacity: capacity.max(1),
                sampling_ppm: AtomicU32::new(SAMPLE_ALL_PPM),
                redactor,
                sink: Mutex::new(None),
                state: Mutex::new(State {
                    traces: VecDeque::new(),
                    open: BTreeMap::new(),
                    truncated: Vec::new(),
                }),
                trace_seq: AtomicU64::new(1),
                span_seq: AtomicU64::new(1),
                sample_seq: AtomicU64::new(0),
            }),
        }
    }

    /// Samples `ppm` parts per million of new traces (`SAMPLE_ALL_PPM` is every trace, 0 none).
    /// A dev site uses the default; the knob exists so a soak test can turn recording off.
    #[must_use]
    pub fn with_sampling(self, ppm: u32) -> Self {
        self.inner
            .sampling_ppm
            .store(ppm.min(SAMPLE_ALL_PPM), Ordering::Relaxed);
        self
    }

    /// Sends every finished span to `sink`.
    #[must_use]
    pub fn with_sink(self, sink: Arc<dyn TraceSink>) -> Self {
        if let Ok(mut slot) = self.inner.sink.lock() {
            *slot = Some(sink);
        }
        self
    }

    /// The site this collector records.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.inner.site
    }

    /// The ring capacity, in traces.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Starts a new trace, respecting the sampling rate. `at` is the current monotonic time.
    #[must_use]
    pub fn root(
        &self,
        layer: Layer,
        name: &str,
        attributes: impl IntoIterator<Item = (String, String)>,
        at: Duration,
    ) -> ActiveSpan {
        let sampled = self.sample();
        let sequence = self.inner.trace_seq.fetch_add(1, Ordering::Relaxed);
        let trace_id = TraceId::from_u128(
            (u128::from(site_bits(self.inner.site.as_str())) << 64) | u128::from(sequence),
        );
        self.start_span(trace_id, None, sampled, layer, name, attributes, at)
    }

    /// Starts a child span of `parent`.
    #[must_use]
    pub fn child(
        &self,
        parent: &TraceContext,
        layer: Layer,
        name: &str,
        attributes: impl IntoIterator<Item = (String, String)>,
        at: Duration,
    ) -> ActiveSpan {
        self.start_span(
            parent.trace_id.clone(),
            Some(parent.span_id),
            parent.sampled,
            layer,
            name,
            attributes,
            at,
        )
    }

    fn sample(&self) -> bool {
        let ppm = self.inner.sampling_ppm.load(Ordering::Relaxed);
        if ppm == 0 {
            return false;
        }
        if ppm >= SAMPLE_ALL_PPM {
            return true;
        }
        let n = self.inner.sample_seq.fetch_add(1, Ordering::Relaxed);
        (n % u64::from(SAMPLE_ALL_PPM)) < u64::from(ppm)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_span(
        &self,
        trace_id: TraceId,
        parent: Option<SpanId>,
        sampled: bool,
        layer: Layer,
        name: &str,
        attributes: impl IntoIterator<Item = (String, String)>,
        at: Duration,
    ) -> ActiveSpan {
        if !sampled {
            return ActiveSpan {
                inner: Arc::clone(&self.inner),
                span: None,
            };
        }
        let span_id = SpanId::from_u64(self.inner.span_seq.fetch_add(1, Ordering::Relaxed));
        let mut span = TraceSpan {
            trace_id,
            span_id,
            parent,
            layer,
            name: redact(&self.inner.redactor, name),
            attributes: BTreeMap::new(),
            start: at,
            end: None,
            error: None,
        };
        for (key, value) in attributes {
            put_attribute(&mut span, &self.inner.redactor, &key, &value);
        }
        if let Ok(mut state) = self.inner.state.lock() {
            state.open.insert(span_id, span.clone());
        }
        ActiveSpan {
            inner: Arc::clone(&self.inner),
            span: Some(span),
        }
    }
}

fn redact(redactor: &Option<Arc<Redactor>>, text: &str) -> String {
    match redactor {
        Some(redactor) => redactor.redact_str(text).into_owned(),
        None => text.to_owned(),
    }
}

fn site_bits(site: &str) -> u64 {
    // FNV-1a, written here so the trace ids stay deterministic across runs and platforms.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in site.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn put_attribute(span: &mut TraceSpan, redactor: &Option<Arc<Redactor>>, key: &str, value: &str) {
    if is_sensitive_attribute(key) {
        return;
    }
    span.attributes
        .insert(key.to_owned(), redact(redactor, value));
}

/// A span in progress. Dropping it without [`ActiveSpan::end`] records it as incomplete rather
/// than losing it: a missing end is usually a bug in the caller and should be visible.
pub struct ActiveSpan {
    inner: Arc<Inner>,
    span: Option<TraceSpan>,
}

impl ActiveSpan {
    /// The span id (or `None` for an unsampled span).
    #[must_use]
    pub fn span_id(&self) -> Option<SpanId> {
        self.span.as_ref().map(|s| s.span_id)
    }

    /// The context a child span needs.
    #[must_use]
    pub fn context(&self) -> Option<TraceContext> {
        self.span.as_ref().map(|s| TraceContext {
            trace_id: s.trace_id.clone(),
            span_id: s.span_id,
            sampled: true,
        })
    }

    /// Whether this span records anything (false for an unsampled trace).
    #[must_use]
    pub fn is_sampled(&self) -> bool {
        self.span.is_some()
    }

    /// Sets an attribute, redacted. A sensitive name is ignored.
    pub fn set_attribute(&mut self, key: &str, value: &str) {
        if let Some(span) = self.span.as_mut() {
            put_attribute(span, &self.inner.redactor, key, value);
        }
    }

    /// Ends the span at `at`.
    pub fn end(self, at: Duration) {
        self.finish(at, None);
    }

    /// Ends the span at `at` with an error (redacted).
    pub fn fail(self, at: Duration, error: &str) {
        self.finish(at, Some((at, error)));
    }

    fn finish(mut self, at: Duration, error: Option<(Duration, &str)>) {
        if let Some(mut span) = self.span.take() {
            span.end = Some(at.max(span.start));
            if let Some((_, error)) = error {
                span.error = Some(redact(&self.inner.redactor, error));
            }
            record(&self.inner, span);
        }
    }
}

impl Drop for ActiveSpan {
    fn drop(&mut self) {
        if let Some(mut span) = self.span.take()
            && span.end.is_none()
        {
            span.end = Some(span.start);
            span.attributes
                .insert("incomplete".to_owned(), "true".to_owned());
            record(&self.inner, span);
        }
    }
}

impl std::fmt::Debug for ActiveSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.span {
            Some(span) => f
                .debug_struct("ActiveSpan")
                .field("trace_id", &span.trace_id)
                .field("span_id", &span.span_id)
                .field("layer", &span.layer)
                .field("name", &span.name)
                .finish(),
            None => f.write_str("ActiveSpan(unsampled)"),
        }
    }
}

fn record(inner: &Arc<Inner>, span: TraceSpan) {
    let Ok(mut state) = inner.state.lock() else {
        return;
    };
    let trace_id = span.trace_id.clone();
    let index = match state
        .traces
        .iter()
        .position(|trace| trace.trace_id == trace_id)
    {
        Some(index) => index,
        None => {
            state.traces.push_back(Trace {
                trace_id: trace_id.clone(),
                spans: Vec::new(),
                request_id: None,
            });
            state.traces.len() - 1
        }
    };
    let too_long = state
        .traces
        .get(index)
        .is_none_or(|trace| trace.spans.len() >= MAX_SPANS_PER_TRACE);
    if too_long {
        if !state.truncated.contains(&trace_id) {
            state.truncated.push(trace_id);
        }
        return;
    }
    let request_id = span
        .attributes
        .get("request_id")
        .cloned()
        .or_else(|| state.traces.get(index).and_then(|t| t.request_id.clone()));
    let recorded = span.clone();
    state.open.remove(&span.span_id);
    if let Some(trace) = state.traces.get_mut(index) {
        trace.request_id = request_id.clone();
        trace.spans.push(span);
    }
    while state.traces.len() > inner.capacity {
        state.traces.pop_front();
    }
    let finished = (recorded.parent.is_none())
        .then(|| {
            state
                .traces
                .iter()
                .find(|trace| trace.trace_id == recorded.trace_id)
                .cloned()
        })
        .flatten();
    drop(state);
    if let Ok(sink) = inner.sink.lock()
        && let Some(sink) = sink.as_ref()
    {
        sink.span_finished(&recorded, request_id.as_deref());
        if let Some(trace) = finished {
            sink.trace_finished(&trace);
        }
    }
}

impl TraceCollector {
    /// The recorded traces, newest first.
    #[must_use]
    pub fn traces(&self) -> Vec<Trace> {
        match self.inner.state.lock() {
            Ok(state) => state.traces.iter().rev().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The summaries, newest first.
    #[must_use]
    pub fn summaries(&self) -> Vec<TraceSummary> {
        self.traces().iter().map(Trace::summary).collect()
    }

    /// The trace with `trace_id` (32 hex digits).
    #[must_use]
    pub fn trace(&self, trace_id: &str) -> Option<Trace> {
        let id = TraceId::parse(trace_id)?;
        match self.inner.state.lock() {
            Ok(state) => state
                .traces
                .iter()
                .find(|trace| trace.trace_id == id)
                .cloned(),
            Err(_) => None,
        }
    }

    /// The trace of request `request_id`, newest first.
    #[must_use]
    pub fn by_request(&self, request_id: &str) -> Option<Trace> {
        match self.inner.state.lock() {
            Ok(state) => state
                .traces
                .iter()
                .rev()
                .find(|trace| trace.request_id.as_deref() == Some(request_id))
                .cloned(),
            Err(_) => None,
        }
    }

    /// What is in the buffer now: recorded traces and spans still open.
    #[must_use]
    pub fn counts(&self) -> (usize, usize) {
        match self.inner.state.lock() {
            Ok(state) => (state.traces.len(), state.open.len()),
            Err(_) => (0, 0),
        }
    }

    /// Drops every recorded trace.
    pub fn clear(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            state.traces.clear();
            state.open.clear();
            state.truncated.clear();
        }
    }

    /// Whether `trace_id` was truncated by [`MAX_SPANS_PER_TRACE`].
    #[must_use]
    pub fn was_truncated(&self, trace_id: &TraceId) -> bool {
        self.inner
            .state
            .lock()
            .is_ok_and(|state| state.truncated.contains(trace_id))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use super::{SAMPLE_ALL_PPM, TraceCollector};
    use crate::trace::span::Layer;

    fn collector(capacity: usize) -> TraceCollector {
        TraceCollector::new(spot_core::SiteId::new("acme.example").unwrap(), capacity)
    }

    #[test]
    fn a_root_and_a_child_nest_and_query_by_request_id() {
        let c = collector(8);
        let root = c.root(
            Layer::Http,
            "POST /api/x",
            [("request_id".into(), "r-1".into())],
            Duration::from_millis(10),
        );
        let context = root.context().unwrap();
        let child = c.child(
            &context,
            Layer::Script,
            "script:on_validate",
            [],
            Duration::from_millis(11),
        );
        child.end(Duration::from_millis(14));
        root.end(Duration::from_millis(20));

        let trace = c.by_request("r-1").unwrap();
        assert_eq!(trace.spans.len(), 2);
        let root_span = trace.root().unwrap();
        assert_eq!(root_span.name, "POST /api/x");
        assert_eq!(root_span.parent, None);
        let child = trace
            .spans
            .iter()
            .find(|span| span.layer == Layer::Script)
            .expect("script span");
        assert_eq!(child.parent, Some(root_span.span_id));
        assert_eq!(trace.duration(), Some(Duration::from_millis(10)));
        assert_eq!(c.summaries().len(), 1);
    }

    #[test]
    fn the_ring_keeps_the_newest_traces() {
        let c = collector(2);
        for i in 0..3 {
            c.root(Layer::Job, &format!("job-{i}"), [], Duration::ZERO)
                .end(Duration::from_millis(1));
        }
        let summaries = c.summaries();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].root, "job-2");
        assert_eq!(summaries[1].root, "job-1");
    }

    #[test]
    fn sampling_zero_records_nothing_and_all_records_everything() {
        let none = collector(4).with_sampling(0);
        none.root(Layer::Http, "x", [], Duration::ZERO)
            .end(Duration::ZERO);
        assert!(none.summaries().is_empty());

        let all = collector(4).with_sampling(SAMPLE_ALL_PPM);
        all.root(Layer::Http, "x", [], Duration::ZERO)
            .end(Duration::ZERO);
        assert_eq!(all.summaries().len(), 1);
    }
}
