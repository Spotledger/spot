//! The unified trace across the layers: one request, one tree, queryable by request id.
#![allow(clippy::unwrap_used, clippy::expect_used)]
//!
//! The acceptance sentence is "traces work across native, WASM and TypeScript code": a request
//! that starts at HTTP, runs a lifecycle step, calls a script, reaches storage and crosses into a
//! WASM app must be **one** trace with the right nesting, not five log lines. This test builds
//! exactly that shape with the same APIs the host wires, and proves that redaction applies before
//! a span reaches the buffer, the renderer or JSON.

use std::time::Duration;

use spot_core::SiteId;
use spot_devtools::trace::{Layer, TraceCollector};
use spot_devtools::wasm::{HostCall, WasmCallReport, WasmDebugInfo};

const SECRET: &str = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.c2VjcmV0c2lnbmF0dXJl";

fn collector() -> TraceCollector {
    TraceCollector::new(SiteId::new("acme.example").expect("site id"), 16)
}

#[test]
fn one_request_is_one_trace_nested_through_every_layer() {
    let collector = collector();
    let http = collector.root(
        Layer::Http,
        "POST /api/v1/orders",
        [
            ("request_id".into(), "req-42".into()),
            ("route".into(), "/api/v1/orders".into()),
            ("authorization".into(), format!("Bearer {SECRET}")),
            ("note".into(), format!("Bearer {SECRET}")),
        ],
        Duration::from_millis(100),
    );
    let http_context = http.context().expect("sampled");

    let lifecycle = collector.child(
        &http_context,
        Layer::Lifecycle,
        "lifecycle.on_submit",
        [("doctype".into(), "Order".into())],
        Duration::from_millis(101),
    );
    let lifecycle_context = lifecycle.context().expect("sampled");

    let script = collector.child(
        &lifecycle_context,
        Layer::Script,
        "script:on_submit",
        [("script".into(), "on_submit".into())],
        Duration::from_millis(102),
    );
    let script_context = script.context().expect("sampled");

    let storage = collector.child(
        &script_context,
        Layer::Storage,
        "storage.insert",
        [("table".into(), "orders".into())],
        Duration::from_millis(103),
    );
    let storage_context = storage.context().expect("sampled");

    let wasm = WasmDebugInfo::new(WasmCallReport {
        component_hash: "0123456789abcdef0123456789abcdef".into(),
        export: "on-event".into(),
        fuel_used: 900,
        memory_bytes: 65_536,
        host_calls: vec![HostCall::new("log", 2, 10)],
        wall_time: Duration::from_millis(1),
    })
    .expect("valid report")
    .record(&collector, &storage_context, Duration::from_millis(104));
    wasm.end(Duration::from_millis(105));
    storage.end(Duration::from_millis(106));
    script.end(Duration::from_millis(107));
    lifecycle.end(Duration::from_millis(108));
    http.end(Duration::from_millis(109));

    let trace = collector
        .by_request("req-42")
        .expect("the trace of the request");
    assert_eq!(trace.spans.len(), 5, "one trace, five spans");
    let order = [
        Layer::Http,
        Layer::Lifecycle,
        Layer::Script,
        Layer::Storage,
        Layer::Wasm,
    ];
    for (index, layer) in order.iter().enumerate() {
        let span = trace
            .spans
            .iter()
            .find(|span| span.layer == *layer)
            .unwrap_or_else(|| panic!("missing {layer}"));
        if index == 0 {
            assert_eq!(span.parent, None);
        } else {
            let parent = trace
                .spans
                .iter()
                .find(|candidate| candidate.layer == order[index - 1])
                .expect("parent layer");
            assert_eq!(
                span.parent,
                Some(parent.span_id),
                "{layer} is not a child of {}",
                order[index - 1]
            );
        }
    }
    let json = trace.to_json();
    assert_eq!(json["requestId"], "req-42");
    assert!(json["durationMicros"].is_u64());

    // Redaction: the token never reaches the buffer, the JSON or the text view.
    let rendered = spot_devtools::trace::render_tree(&trace);
    for text in [json.to_string(), rendered] {
        assert!(!text.contains(SECRET), "a secret leaked: {text}");
    }
    assert!(
        trace
            .spans
            .iter()
            .all(|span| !span.attributes.contains_key("authorization")),
        "a sensitive attribute name was kept"
    );
    let note = trace
        .spans
        .iter()
        .find(|span| span.layer == Layer::Http)
        .and_then(|span| span.attributes.get("note"))
        .expect("the ordinary attribute is kept");
    assert!(note.contains("redacted"), "{note}");
}

#[test]
fn the_ring_is_bounded_and_a_trace_is_queryable_by_id() {
    let collector = collector();
    let mut ids = Vec::new();
    for index in 0..20 {
        let span = collector.root(
            Layer::Job,
            &format!("job-{index}"),
            [("request_id".into(), format!("r-{index}"))],
            Duration::from_millis(index),
        );
        if let Some(context) = span.context() {
            ids.push(context.trace_id.to_string());
        }
        span.end(Duration::from_millis(index + 1));
    }
    // 16 kept, 4 evicted from the front.
    let summaries = collector.summaries();
    assert_eq!(summaries.len(), 16);
    assert_eq!(summaries[0].request_id.as_deref(), Some("r-19"));
    assert!(collector.by_request("r-0").is_none());
    let kept = &ids[10];
    assert!(collector.trace(kept).is_some());
    assert!(collector.trace("not-a-trace-id").is_none());
}

#[test]
fn an_unfinished_span_is_recorded_as_incomplete_not_lost() {
    let collector = collector();
    let span = collector.root(Layer::Http, "GET /x", [], Duration::ZERO);
    let id = span.context().expect("sampled").trace_id;
    drop(span);
    let trace = collector.trace(id.as_str()).expect("recorded on drop");
    assert_eq!(trace.spans.len(), 1);
    assert_eq!(
        trace.spans[0].attributes.get("incomplete"),
        Some(&"true".to_owned())
    );
}

#[test]
fn sampling_can_be_turned_off_for_a_soak() {
    let collector = collector().with_sampling(0);
    collector
        .root(Layer::Http, "GET /x", [], Duration::ZERO)
        .end(Duration::from_millis(1));
    assert!(collector.summaries().is_empty());
    assert_eq!(collector.capacity(), 16);
}
