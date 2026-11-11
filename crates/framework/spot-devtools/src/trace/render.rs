//! Text rendering of a trace: the tree the CLI prints (`spot trace <request-id>`) and a per-layer
//! flame summary.
//!
//! The output is plain ASCII so it reads the same in a Windows console, a Fedora terminal and a
//! CI log. Durations are printed from integer microseconds; no float is ever computed.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;

use super::collector::{Trace, TraceSummary};
use super::span::{Layer, SpanId, TraceSpan};

/// A duration as `12us`, `1.234ms` or `1.002s`, built from integers.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let micros = duration.as_micros();
    if micros < 1_000 {
        return format!("{micros}us");
    }
    if micros < 1_000_000 {
        return format!("{}.{:03}ms", micros / 1_000, micros % 1_000);
    }
    format!(
        "{}.{:03}s",
        micros / 1_000_000,
        (micros % 1_000_000) / 1_000
    )
}

/// The trace as an indented tree, one line per span, children under their parent.
#[must_use]
pub fn render_tree(trace: &Trace) -> String {
    let origin = trace.origin();
    let mut out = String::new();
    let request = trace.request_id.as_deref().unwrap_or("-");
    let _ = writeln!(
        out,
        "trace {} request={} spans={} duration={} errors={}",
        trace.trace_id,
        request,
        trace.spans.len(),
        format_duration(trace.duration().unwrap_or_default()),
        trace.error_count(),
    );
    let mut children: BTreeMap<Option<SpanId>, Vec<&TraceSpan>> = BTreeMap::new();
    for span in &trace.spans {
        let parent = span.parent.filter(|p| trace.span(*p).is_some());
        children.entry(parent).or_default().push(span);
    }
    for siblings in children.values_mut() {
        siblings.sort_by_key(|span| (span.start, span.span_id));
    }
    let mut roots = children.remove(&None).unwrap_or_default();
    if roots.is_empty() {
        roots = trace.spans.iter().collect();
        roots.sort_by_key(|span| (span.start, span.span_id));
    }
    for root in roots {
        render_span(&mut out, root, &children, origin, 0);
    }
    out
}

fn render_span(
    out: &mut String,
    span: &TraceSpan,
    children: &BTreeMap<Option<SpanId>, Vec<&TraceSpan>>,
    origin: Duration,
    depth: usize,
) {
    let offset = span.start.saturating_sub(origin);
    let _ = write!(
        out,
        "{}{} [{}] +{} {}",
        "  ".repeat(depth),
        span.name,
        span.layer.as_str(),
        format_duration(offset),
        span.duration()
            .map_or_else(|| "unfinished".to_owned(), format_duration),
    );
    for (key, value) in &span.attributes {
        let _ = write!(out, " {key}={value}");
    }
    if let Some(error) = &span.error {
        let _ = write!(out, " error={error}");
    }
    out.push('\n');
    if let Some(kids) = children.get(&Some(span.span_id)) {
        for child in kids {
            render_span(out, child, children, origin, depth + 1);
        }
    }
}

/// The per-layer inclusive time, largest first: the "flame" view of where the request spent its
/// time. Inclusive means a parent's time includes its children's, so the layers do not sum to the
/// trace duration; the root line gives the whole time.
#[must_use]
pub fn render_layers(trace: &Trace) -> String {
    let mut by_layer: BTreeMap<Layer, (Duration, usize)> = BTreeMap::new();
    for span in &trace.spans {
        let entry = by_layer.entry(span.layer).or_insert((Duration::ZERO, 0));
        entry.0 += span.duration().unwrap_or_default();
        entry.1 += 1;
    }
    let mut rows: Vec<(Layer, Duration, usize)> = by_layer
        .into_iter()
        .map(|(layer, (duration, count))| (layer, duration, count))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut out = String::new();
    let total = trace.duration().unwrap_or_default();
    let _ = writeln!(
        out,
        "trace {} total={} (layers are inclusive)",
        trace.trace_id,
        format_duration(total)
    );
    for (layer, duration, count) in rows {
        let _ = writeln!(
            out,
            "  {:<14} {:>10}  {} span{}",
            layer.as_str(),
            format_duration(duration),
            count,
            if count == 1 { "" } else { "s" }
        );
    }
    out
}

/// The list `spot trace` prints when it has no request id: newest trace first.
#[must_use]
pub fn render_summaries(summaries: &[TraceSummary]) -> String {
    let mut out = String::new();
    if summaries.is_empty() {
        out.push_str("no traces recorded\n");
        return out;
    }
    for summary in summaries {
        let _ = writeln!(
            out,
            "{}  {:<14} {} spans={} duration={} errors={} request={}",
            summary.trace_id,
            summary.layer.as_str(),
            summary.root,
            summary.span_count,
            format_duration(summary.duration),
            summary.error_count,
            summary.request_id.as_deref().unwrap_or("-"),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{format_duration, render_layers, render_summaries, render_tree};
    use crate::trace::collector::TraceCollector;
    use crate::trace::span::Layer;

    fn built() -> TraceCollector {
        let collector = TraceCollector::new(spot_core::SiteId::new("acme.example").expect("id"), 4);
        let root = collector.root(
            Layer::Http,
            "POST /api/x",
            [("request_id".into(), "r-1".into())],
            Duration::from_millis(1),
        );
        let context = root.context().expect("sampled");
        let script = collector.child(
            &context,
            Layer::Script,
            "script:on_validate",
            [],
            Duration::from_millis(2),
        );
        let child = script.context().expect("sampled");
        collector
            .child(
                &child,
                Layer::Storage,
                "storage.query",
                [],
                Duration::from_millis(3),
            )
            .end(Duration::from_millis(5));
        script.end(Duration::from_millis(6));
        root.end(Duration::from_millis(8));
        collector
    }

    #[test]
    fn durations_print_from_integers() {
        assert_eq!(format_duration(Duration::from_micros(12)), "12us");
        assert_eq!(format_duration(Duration::from_micros(1_234)), "1.234ms");
        assert_eq!(format_duration(Duration::from_millis(1_002)), "1.002s");
    }

    #[test]
    fn the_tree_indents_children_under_their_parent() {
        let text = render_tree(&built().by_request("r-1").expect("trace"));
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("trace "));
        assert!(lines[1].starts_with("POST /api/x [http]"));
        assert!(lines[2].starts_with("  script:on_validate [script]"));
        assert!(lines[3].starts_with("    storage.query [storage]"));
    }

    #[test]
    fn the_layer_summary_orders_by_time() {
        let text = render_layers(&built().by_request("r-1").expect("trace"));
        let script = text.find("script").expect("script line");
        let storage = text.find("storage").expect("storage line");
        assert!(script < storage, "script is not above storage:\n{text}");
        assert!(text.contains("http"));
    }

    #[test]
    fn the_list_is_empty_when_nothing_was_recorded() {
        let collector = TraceCollector::new(spot_core::SiteId::new("acme.example").expect("id"), 4);
        assert!(render_summaries(&collector.summaries()).contains("no traces"));
    }
}
