//! `POST /api/v1/graph/query` and `POST /api/v1/lineage` (`graph_query` and `lineage_trace`,
//! WP-GRA-03.T6) over the in-memory [`MemoryGraph`] and [`MemoryLineage`] fakes: the shape of both
//! answers, truncation, what a hidden node leaves behind, the hard limits (checked before the
//! port is asked), the closed request schema and the security properties of the brief.
//!
//! The fakes decide nothing the real adapters decide beyond what their docs say; the tests prove
//! the routes' half of the bargain: what is checked before the port, what is passed to it and how
//! its answer becomes the response.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::ports::graph::{
    DirectionIn, EdgePropertyOut, GraphAnswerOut, GraphPort, GraphQueryIn, GraphResult,
    LineageAnswerOut, LineageKindIn, LineageLevelOut, LineageNodeOut, LineagePort, LineageQueryIn,
    MemoryGraph, MemoryLineage, NodeRef, Unconfigured,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{RouteState, graph};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST_A: &str = "erp.acme.com";
const HOST_B: &str = "erp.globex.com";
const ALICE: &str = "tok-alice";
const NO_DN: &str = "tok-no-delivery-note";
const ONLY_A: &str = "tok-only-a";

/// A canonical UUIDv7 for test record `n`.
fn id(n: u32) -> String {
    format!("0192f3c4-0000-7000-8000-{n:012x}")
}

fn node(doctype: &str, n: u32) -> NodeRef {
    NodeRef::new(doctype, id(n))
}

fn si() -> NodeRef {
    node("sales_invoice", 1)
}
fn dn() -> NodeRef {
    node("delivery_note", 2)
}
fn so() -> NodeRef {
    node("sales_order", 3)
}
fn pe() -> NodeRef {
    node("payment_entry", 4)
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

fn settled_properties() -> BTreeMap<String, EdgePropertyOut> {
    BTreeMap::from([
        (
            "amount".to_owned(),
            EdgePropertyOut::Text("12.50".to_owned()),
        ),
        ("posted".to_owned(), EdgePropertyOut::Bool(true)),
    ])
}

/// Site A: `so --fulfils--> dn --bills--> si --settled_by--> pe`. Site B: the same without the
/// delivery note (and so without the path from `si` to `so`), which is what the caller who
/// cannot see the delivery note must get on site A.
fn build_graph() -> Arc<MemoryGraph> {
    let g = Arc::new(MemoryGraph::new());
    let (a, b) = (site("acme"), site("globex"));
    g.add_edge(&a, dn(), si(), "bills");
    g.add_edge(&a, so(), dn(), "fulfils");
    g.add_edge_with(&a, si(), pe(), "settled_by", settled_properties());
    g.add_node(&b, so());
    g.add_edge_with(&b, si(), pe(), "settled_by", settled_properties());
    g.grant_all(&a, ALICE);
    g.grant_all_but(&a, NO_DN, &[dn()]);
    g.grant_all(&a, ONLY_A);
    g.grant_all(&b, ALICE);
    g
}

fn item(n: u32) -> NodeRef {
    node("item", n)
}

/// Site A's bill of materials: `item(10)` (1) holds `item(11)` (2.500) and `item(12)` (1.000);
/// `item(11)` holds `item(13)` (0.250). Site B has the same without `item(11)` and `item(13)`.
fn build_lineage() -> Arc<MemoryLineage> {
    let l = Arc::new(MemoryLineage::new());
    let (a, b) = (site("acme"), site("globex"));
    l.add_tree(
        &a,
        item(10),
        Some("1"),
        &[
            (0, item(11), Some("2.500")),
            (0, item(12), Some("1.000")),
            (1, item(13), Some("0.250")),
        ],
    );
    l.add_tree(&b, item(10), Some("1"), &[(0, item(12), Some("1.000"))]);
    l.grant_all(&a, ALICE);
    l.grant_all_but(&a, NO_DN, &[item(11), item(10)]);
    l.grant_all(&b, ALICE);
    l
}

struct Fixture {
    router: axum::Router,
    graph: Arc<MemoryGraph>,
    lineage: Arc<MemoryLineage>,
}

fn router_over(ports: Ports) -> axum::Router {
    let state = RouteState::new(ports);
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST_A, site("acme"))
            .add(HOST_B, site("globex"))
            .add_site_name(site("acme"))
            .add_site_name(site("globex")),
    )
    .site_routes(graph::routes().with_state(state))
    .build()
}

fn fixture() -> Fixture {
    let graph_fake = build_graph();
    let lineage_fake = build_lineage();
    let mut ports = Ports::new();
    ports.provide::<dyn GraphPort>(graph_fake.clone());
    ports.provide::<dyn LineagePort>(lineage_fake.clone());
    let state_ports = ports.subset(graph::required_ports()).unwrap();
    Fixture {
        router: router_over(state_ports),
        graph: graph_fake,
        lineage: lineage_fake,
    }
}

/// A router over arbitrary ports (a hostile fake, or none).
fn custom(
    graph_port: Option<Arc<dyn GraphPort>>,
    lineage_port: Option<Arc<dyn LineagePort>>,
) -> axum::Router {
    let mut ports = Ports::new();
    if let Some(port) = graph_port {
        ports.provide::<dyn GraphPort>(port);
    }
    if let Some(port) = lineage_port {
        ports.provide::<dyn LineagePort>(port);
    }
    router_over(ports)
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    bytes: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_slice(&self.bytes).unwrap_or(Json::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
    /// The answer's `data` object.
    fn data(&self) -> Json {
        self.json()["data"].clone()
    }
    /// Asserts the status and the error code; the failure message carries the whole body.
    fn expect(&self, status: u16, code: &str) {
        assert_eq!(self.status, status, "{}", self.text());
        assert_eq!(self.code(), code, "{}", self.text());
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    /// Status, headers (without the per-request ones) and body (without the request id): what two
    /// answers must share to be indistinguishable.
    fn fingerprint(&self) -> (u16, Vec<(String, String)>, Json) {
        let mut headers: Vec<(String, String)> = self
            .headers
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), "x-request-id" | "date"))
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    value.to_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        headers.sort();
        let mut body = self.json();
        if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
            error.remove("request_id");
        }
        (self.status, headers, body)
    }
}

async fn send(
    router: &axum::Router,
    host: &str,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, host);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let request = builder.body(Body::from(body)).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Reply {
        status,
        headers,
        bytes,
    }
}

/// A POST to site A as ALICE with a chosen content type and raw bytes, for the transport tests.
async fn raw(router: &axum::Router, path: &str, content_type: &str, body: Vec<u8>) -> Reply {
    send(
        router,
        HOST_A,
        path,
        Some(ALICE),
        &[],
        Some(content_type),
        body,
    )
    .await
}

async fn post_to(router: &axum::Router, host: &str, path: &str, token: &str, body: Json) -> Reply {
    send(
        router,
        host,
        path,
        Some(token),
        &[],
        Some("application/json"),
        serde_json::to_vec(&body).unwrap(),
    )
    .await
}

async fn on_graph(router: &axum::Router, host: &str, token: &str, body: Json) -> Reply {
    post_to(router, host, "/api/v1/graph/query", token, body).await
}

async fn on_lineage(router: &axum::Router, host: &str, token: &str, body: Json) -> Reply {
    post_to(router, host, "/api/v1/lineage", token, body).await
}

async fn query(f: &Fixture, token: &str, body: Json) -> Reply {
    on_graph(&f.router, HOST_A, token, body).await
}

async fn lineage(f: &Fixture, token: &str, body: Json) -> Reply {
    on_lineage(&f.router, HOST_A, token, body).await
}

fn ref_json(n: &NodeRef) -> Json {
    json!({"doctype": n.doctype, "id": n.id})
}

fn graph_body(start: &NodeRef, depth: u64, limit: u64) -> Json {
    json!({"start": ref_json(start), "direction": "both", "depth": depth, "limit": limit})
}

fn lineage_body(kind: &str, start: &NodeRef) -> Json {
    json!({"kind": kind, "doctype": start.doctype, "id": start.id})
}

/// The answer with `withheld` forced to `false`, serialised: what two answers must share when the
/// only difference is that something was removed.
fn without_flag(reply: &Reply) -> String {
    let mut body = reply.json();
    body["data"]["withheld"] = Json::Bool(false);
    serde_json::to_string(&body).unwrap()
}

#[tokio::test]
async fn graph_query_answers_the_contract_shape() {
    let f = fixture();
    let r = query(&f, ALICE, graph_body(&si(), 3, 100)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert!(r.header("x-request-id").is_some());
    assert_eq!(
        r.json(),
        json!({"data": {
            "nodes": [ref_json(&si()), ref_json(&dn()), ref_json(&pe()), ref_json(&so())],
            "edges": [
                {"from": ref_json(&dn()), "to": ref_json(&si()), "type": "bills"},
                {"from": ref_json(&si()), "to": ref_json(&pe()), "type": "settled_by"},
                {"from": ref_json(&so()), "to": ref_json(&dn()), "type": "fulfils"},
            ],
            "truncated": false,
            "withheld": false,
        }})
    );
}

#[tokio::test]
async fn graph_query_passes_the_parsed_query_and_only_that() {
    let f = fixture();
    let body = json!({
        "start": ref_json(&si()),
        "direction": "out",
        "edge_types": ["settled_by", "bills"],
        "depth": 2,
        "node_filter": {"and": [
            {"field": "status", "op": "eq", "value": "open"},
            {"field": "total", "in": ["1.50", 2]},
            {"not": {"field": "note", "is_null": true}},
        ]},
        "edge_filter": {"field": "posted", "op": "eq", "value": true},
        "return_edge_properties": true,
        "limit": 50,
    });
    let r = query(&f, ALICE, body).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let seen = f.graph.queries();
    assert_eq!(seen.len(), 1);
    let q = &seen[0];
    assert_eq!(q.start, si());
    assert_eq!(q.direction, DirectionIn::Out);
    assert_eq!(
        q.edge_types,
        vec!["settled_by".to_owned(), "bills".to_owned()]
    );
    assert_eq!((q.depth, q.limit), (2, 50));
    assert!(q.return_edge_properties);
    assert!(q.node_filter.is_some() && q.edge_filter.is_some());
}

#[tokio::test]
async fn graph_query_flags_truncation_with_its_reason() {
    let f = fixture();
    let by_nodes = query(&f, ALICE, graph_body(&si(), 3, 2)).await;
    assert_eq!(by_nodes.status, 200, "{}", by_nodes.text());
    let data = by_nodes.data();
    assert_eq!(data["truncated"], true);
    assert_eq!(data["truncation_reason"], "nodes");
    assert_eq!(data["nodes"].as_array().unwrap().len(), 2);

    let body = json!({"start": ref_json(&so()), "direction": "out", "depth": 1, "limit": 100});
    let by_depth = query(&f, ALICE, body).await;
    let data = by_depth.data();
    assert_eq!(data["truncated"], true);
    assert_eq!(data["truncation_reason"], "depth");

    let whole = query(&f, ALICE, graph_body(&si(), 3, 100)).await;
    let data = whole.data();
    assert_eq!(data["truncated"], false);
    assert!(
        data.get("truncation_reason").is_none(),
        "no reason when whole"
    );
}

#[tokio::test]
async fn edge_properties_appear_only_when_asked_and_decimals_stay_strings() {
    let f = fixture();
    let mut body = json!({
        "start": ref_json(&si()), "direction": "out", "edge_types": ["settled_by"],
        "depth": 1, "limit": 10,
    });
    let bare = query(&f, ALICE, body.clone()).await;
    assert!(bare.json()["data"]["edges"][0].get("properties").is_none());
    body["return_edge_properties"] = Json::Bool(true);
    let with = query(&f, ALICE, body).await;
    assert_eq!(
        with.json()["data"]["edges"][0]["properties"],
        json!({"amount": "12.50", "posted": true}),
        "a decimal is a string, never a number"
    );
    assert!(with.json()["data"]["edges"][0]["properties"]["amount"].is_string());
}

#[tokio::test]
async fn a_hidden_node_leaves_nothing_but_the_flag() {
    let f = fixture();
    let body = graph_body(&si(), 3, 100);
    let hidden = query(&f, NO_DN, body.clone()).await;
    assert_eq!(hidden.status, 200, "{}", hidden.text());
    let data = hidden.data();
    assert_eq!(data["withheld"], true);
    // The node, the nodes behind it (the sales order) and every edge through it are gone.
    assert_eq!(data["nodes"], json!([ref_json(&si()), ref_json(&pe())]));
    assert_eq!(data["edges"].as_array().unwrap().len(), 1);
    let text = hidden.text();
    assert!(!text.contains(&id(2)) && !text.contains(&id(3)), "{text}");
    assert!(!text.contains("delivery_note"), "{text}");

    // The same query on a site whose graph never had the node: the answer is the same bytes
    // apart from the flag: no count, no placeholder.
    let clean = on_graph(&f.router, HOST_B, ALICE, body).await;
    assert_eq!(clean.status, 200, "{}", clean.text());
    assert_eq!(clean.json()["data"]["withheld"], false);
    assert_eq!(without_flag(&hidden), without_flag(&clean));
    let mut keys: Vec<_> = data.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["edges", "nodes", "truncated", "withheld"]);
}

#[tokio::test]
async fn an_unknown_start_and_an_unreadable_start_are_one_answer() {
    let f = fixture();
    let unknown = query(&f, ALICE, graph_body(&node("sales_invoice", 99), 2, 10)).await;
    let unreadable = query(&f, NO_DN, graph_body(&dn(), 2, 10)).await;
    unknown.expect(404, "graph.not_found");
    assert_eq!(unknown.fingerprint(), unreadable.fingerprint());
    assert_eq!(unknown.header("cache-control"), Some("private, no-store"));

    let unknown = lineage(&f, ALICE, lineage_body("bom", &item(99))).await;
    let unreadable = lineage(&f, NO_DN, lineage_body("bom", &item(10))).await;
    unknown.expect(404, "graph.not_found");
    assert_eq!(unknown.fingerprint(), unreadable.fingerprint());
}

#[tokio::test]
async fn lineage_trace_answers_levels_with_exact_decimal_strings() {
    let f = fixture();
    let r = lineage(&f, ALICE, lineage_body("bom", &item(10))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("cache-control"), Some("private, no-store"));
    assert_eq!(
        r.json(),
        json!({"data": {
            "levels": [
                {"depth": 0, "nodes": [{"doctype": "item", "id": id(10), "quantity": "1"}],
                 "node_count": 1, "quantity_total": "1"},
                {"depth": 1, "nodes": [
                    {"doctype": "item", "id": id(11), "quantity": "2.500"},
                    {"doctype": "item", "id": id(12), "quantity": "1.000"}],
                 "node_count": 2, "quantity_total": "3.500"},
                {"depth": 2, "nodes": [{"doctype": "item", "id": id(13), "quantity": "0.250"}],
                 "node_count": 1, "quantity_total": "0.250"},
            ],
            "truncated": false,
            "withheld": false,
        }})
    );
    for level in r.json()["data"]["levels"].as_array().unwrap() {
        assert!(level["quantity_total"].is_string());
        for n in level["nodes"].as_array().unwrap() {
            assert!(
                n["quantity"].is_string(),
                "a quantity is a string, never a number"
            );
        }
    }
}

#[tokio::test]
#[rustfmt::skip]
async fn lineage_trace_passes_each_kind_and_flags_truncation() {
    let f = fixture();
    for (kind, expected) in [("chain", LineageKindIn::Chain), ("trace_forward", LineageKindIn::TraceForward), ("trace_back", LineageKindIn::TraceBack), ("bom", LineageKindIn::Bom), ("where_used", LineageKindIn::WhereUsed)] {
        let r = lineage(&f, ALICE, lineage_body(kind, &item(10))).await;
        assert_eq!(r.status, 200, "{kind}: {}", r.text());
        assert_eq!(f.lineage.queries().last().unwrap().kind, expected);
        assert_eq!(r.data()["levels"][0].get("quantity_total").is_some(), kind == "bom");
    }
    for (field, value, reason) in [("depth", 1, "depth"), ("limit", 2, "nodes")] {
        let mut body = lineage_body("bom", &item(10));
        body[field] = json!(value);
        let data = lineage(&f, ALICE, body).await.data();
        assert_eq!((&data["truncated"], &data["truncation_reason"]), (&json!(true), &json!(reason)));
    }
}
#[tokio::test]
async fn a_hidden_lineage_node_is_removed_with_its_subtree_and_leaves_only_the_flag() {
    // The caller may not see item 11: it and item 13, which hangs under it, are gone.
    let l = build_lineage();
    l.grant_all_but(&site("acme"), "tok-no-11", &[item(11)]);
    let mut ports = Ports::new();
    ports.provide::<dyn LineagePort>(l.clone());
    ports.provide::<dyn GraphPort>(build_graph());
    let router = router_over(ports);
    let hidden = on_lineage(&router, HOST_A, "tok-no-11", lineage_body("bom", &item(10))).await;
    assert_eq!(hidden.status, 200, "{}", hidden.text());
    assert_eq!(hidden.json()["data"]["withheld"], true);
    let text = hidden.text();
    assert!(!text.contains(&id(11)) && !text.contains(&id(13)), "{text}");
    // Site B's tree never had them: the same bytes apart from the flag.
    let clean = on_lineage(&router, HOST_B, ALICE, lineage_body("bom", &item(10))).await;
    assert_eq!(clean.json()["data"]["withheld"], false);
    assert_eq!(without_flag(&hidden), without_flag(&clean));
}

fn deep_not(levels: usize) -> Json {
    let mut filter = json!({"field": "status", "is_null": false});
    for _ in 1..levels {
        filter = json!({"not": filter});
    }
    filter
}

fn leaf(n: usize) -> Json {
    json!({"field": format!("f{n}"), "op": "eq", "value": n})
}

/// A filter of exactly `terms` terms and depth 3 (an `and` of `and`s of leaves), `terms` from 9.
fn wide(terms: usize) -> Json {
    let inner = |n: usize| json!({"and": (0..n).map(leaf).collect::<Vec<_>>()});
    // root (1) + 3 inner (1 + 8 each) = 28, plus leaves for the rest.
    let extra = terms - 28;
    let mut children = vec![inner(8), inner(8), inner(8)];
    children.extend((0..extra).map(leaf));
    json!({"and": children})
}

#[tokio::test]
#[rustfmt::skip]
async fn each_graph_limit_accepts_the_boundary_and_refuses_one_more_before_the_port() {
    let f = fixture();
    let types = |n: usize| (0..n).map(|i| format!("t{i}")).collect::<Vec<_>>();
    // (name, field, value at the limit, value one over)
    let cases: Vec<(&str, &str, Json, Json)> = vec![
        ("depth", "depth", json!(8), json!(9)),
        ("limit", "limit", json!(1000), json!(1001)),
        ("edge types", "edge_types", json!(types(16)), json!(types(17))),
        ("filter depth", "node_filter", deep_not(4), deep_not(5)),
        ("edge filter depth", "edge_filter", deep_not(4), deep_not(5)),
        ("filter terms", "node_filter", wide(32), wide(33)),
    ];
    for (what, field, ok, over) in cases {
        let before = f.graph.queries().len();
        let mut body = graph_body(&si(), 3, 10);
        body[field] = ok;
        let accepted = query(&f, ALICE, body.clone()).await;
        assert_eq!(accepted.status, 200, "{what} at the limit: {}", accepted.text());
        assert_eq!(f.graph.queries().len(), before + 1, "{what}: the port is asked");
        body[field] = over;
        let refused = query(&f, ALICE, body).await;
        refused.expect(422, "graph.too_large");
        assert_eq!(refused.header("cache-control"), Some("private, no-store"), "{what}");
        assert_eq!(f.graph.queries().len(), before + 1, "{what}: the port is not asked");
    }
}

#[tokio::test]
async fn each_lineage_limit_accepts_the_boundary_and_refuses_one_more_before_the_port() {
    let f = fixture();
    for (what, ok, over) in [("depth", 12, 13), ("limit", 5000, 5001)] {
        let mut body = lineage_body("bom", &item(10));
        body[what] = json!(ok);
        let before = f.lineage.queries().len();
        let accepted = lineage(&f, ALICE, body.clone()).await;
        assert_eq!(accepted.status, 200, "{what}: {}", accepted.text());
        body[what] = json!(over);
        let refused = lineage(&f, ALICE, body).await;
        refused.expect(422, "graph.too_large");
        assert_eq!(
            f.lineage.queries().len(),
            before + 1,
            "{what}: the port is not asked"
        );
    }
}

#[tokio::test]
async fn a_hard_limit_is_the_answer_even_when_the_rest_of_the_body_is_wrong() {
    let f = fixture();
    let r = query(
        &f,
        ALICE,
        json!({"depth": 9, "limit": 5, "direction": "sideways", "surprise": 1}),
    )
    .await;
    assert_eq!(r.status, 422);
    assert_eq!(
        r.code(),
        "graph.too_large",
        "the specific code beats the schema check"
    );
    let mut body = lineage_body("nonsense", &item(10));
    body["limit"] = json!(6000);
    let r = lineage(&f, ALICE, body).await;
    assert_eq!(r.code(), "graph.too_large");
    assert!(f.graph.queries().is_empty() && f.lineage.queries().is_empty());
}

#[tokio::test]
#[rustfmt::skip]
async fn an_unknown_field_is_refused_and_an_actor_is_never_read() {
    let f = fixture();
    for field in ["actor", "user", "tenant", "site", "roles", "chains"] {
        let mut body = graph_body(&si(), 2, 10);
        body[field] = json!("system");
        let r = query(&f, ALICE, body).await;
        r.expect(422, "graph.query_invalid");
        assert_eq!(r.json()["error"]["details"]["path"], "body", "{field}");
        assert!(!r.text().contains(field), "the key's name is not echoed: {}", r.text());
        // The chain configuration is server data: a client cannot send one.
        let mut body = lineage_body("chain", &item(10));
        body[field] = json!("system");
        lineage(&f, ALICE, body).await.expect(422, "graph.query_invalid");
    }
    assert!(f.graph.queries().is_empty() && f.lineage.queries().is_empty());
}
#[tokio::test]
#[rustfmt::skip]
async fn an_actor_in_a_header_or_the_query_string_changes_nothing() {
    let f = fixture();
    let body = serde_json::to_vec(&graph_body(&si(), 3, 100)).unwrap();
    let ct = Some("application/json");
    let plain = send(&f.router, HOST_A, "/api/v1/graph/query", Some(NO_DN), &[], ct, body.clone()).await;
    let spoof = [("x-actor", "alice"), ("x-spot-user", "Administrator"), ("x-site", "globex")];
    let path = "/api/v1/graph/query?actor=alice&user=admin&as=system";
    let spoofed = send(&f.router, HOST_A, path, Some(NO_DN), &spoof, ct, body).await;
    assert_eq!((plain.status, spoofed.status), (200, 200), "{}", spoofed.text());
    assert_eq!(plain.bytes, spoofed.bytes, "the caller is the token's, whatever else is sent");
    assert_eq!(spoofed.data()["withheld"], true);
}

#[tokio::test]
#[rustfmt::skip]
async fn a_bad_field_is_named_by_path_and_its_value_is_never_echoed() {
    let f = fixture();
    let secret = "SECRET-VALUE-9f3";
    let start = |id: String| json!({"doctype": "sales_invoice", "id": id});
    // (field to set, or `null` to remove it; value; the path the answer names)
    let cases: Vec<(&str, Json, &str)> = vec![
        ("start", Json::Null, "body.start"),
        ("start", start(secret.to_owned()), "body.start.id"),
        ("start", start(id(1).to_uppercase()), "body.start.id"),
        ("start", start(id(1).replace('-', "")), "body.start.id"),
        ("start", start("0192f3c4-0000-4000-8000-000000000001".to_owned()), "body.start.id"), // a v4
        ("start", start("0192f3c4-0000-7000-c000-000000000001".to_owned()), "body.start.id"), // a bad variant
        ("start", start("../../etc/passwd".to_owned()), "body.start.id"),
        ("start", json!({"doctype": "Sales Invoice", "id": id(1)}), "body.start.doctype"),
        ("direction", json!(secret), "body.direction"),
        ("depth", json!(0), "body.depth"),
        ("depth", json!(1.5), "body.depth"),
        ("limit", json!(0), "body.limit"),
        ("edge_types", json!(["ok", secret]), "body.edge_types[1]"),
        ("node_filter", json!({"field": "a", "op": "like", "value": secret}), "body.node_filter.op"),
        ("node_filter", json!({"and": [{"field": "a", "op": "eq", "value": 1.5}]}), "body.node_filter.and[0].value"),
        ("node_filter", json!({"field": "a", "op": "eq", "value": 1, "regex": secret}), "body.node_filter"),
        ("node_filter", json!({"and": []}), "body.node_filter.and"),
        ("return_edge_properties", json!(secret), "body.return_edge_properties"),
    ];
    for (field, value, path) in cases {
        let mut body = graph_body(&si(), 1, 1);
        if value.is_null() {
            body.as_object_mut().unwrap().remove(field);
        } else {
            body[field] = value;
        }
        let r = query(&f, ALICE, body.clone()).await;
        r.expect(422, "graph.query_invalid");
        assert_eq!(r.json()["error"]["details"]["path"], path, "{body}");
        assert!(!r.text().contains(secret), "{path}: a value was echoed: {}", r.text());
    }
    assert!(f.graph.queries().is_empty(), "no refused request reached the port");
    query(&f, ALICE, json!(["not", "an", "object"])).await.expect(422, "graph.query_invalid");
}

#[tokio::test]
#[rustfmt::skip]
async fn an_unknown_lineage_kind_is_its_own_code() {
    let f = fixture();
    lineage(&f, ALICE, lineage_body("everything", &item(10))).await.expect(422, "lineage.kind_unknown");
    for (field, value, path) in [("kind", json!(7), "body.kind"), ("id", json!("not-an-id"), "body.id")] {
        let mut body = lineage_body("bom", &item(10));
        body[field] = value;
        let r = lineage(&f, ALICE, body).await;
        r.expect(422, "graph.query_invalid");
        assert_eq!(r.json()["error"]["details"]["path"], path);
    }
    assert!(f.lineage.queries().is_empty());
}
#[tokio::test]
#[rustfmt::skip]
async fn a_request_without_a_credential_is_refused_before_the_port() {
    let f = fixture();
    for path in ["/api/v1/graph/query", "/api/v1/lineage"] {
        let body = serde_json::to_vec(&graph_body(&si(), 1, 1)).unwrap();
        let r = send(&f.router, HOST_A, path, None, &[], Some("application/json"), body).await;
        r.expect(401, "auth.required");
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
    }
    assert!(f.graph.queries().is_empty() && f.lineage.queries().is_empty());
}
#[tokio::test]
#[rustfmt::skip]
async fn an_oversized_body_is_refused_and_a_bad_body_is_named() {
    let f = fixture();
    let json_type = "application/json";
    let huge = raw(&f.router, "/api/v1/graph/query", json_type, vec![b' '; 300 * 1024]).await;
    huge.expect(Code::HttpBodyTooLarge.status().as_u16(), "http.body_too_large");
    let text = raw(&f.router, "/api/v1/graph/query", "text/plain", b"{}".to_vec()).await;
    assert_eq!(text.status, 415, "{}", text.text());
    for body in [b"{not json".to_vec(), Vec::new()] {
        raw(&f.router, "/api/v1/lineage", json_type, body).await.expect(400, "api.invalid_json");
    }
    assert!(f.graph.queries().is_empty() && f.lineage.queries().is_empty());
}
#[tokio::test]
#[rustfmt::skip]
async fn a_declared_port_error_keeps_its_code_and_loses_its_text() {
    for (code, status, text) in [(Code::GraphUnavailable, 503, "graph.unavailable"), (Code::GraphNotFound, 404, "graph.not_found"), (Code::GraphTooLarge, 422, "graph.too_large")] {
        let f = fixture();
        f.graph.fail_next(ApiError::new(code, "SECRET-IN-MESSAGE"));
        let r = query(&f, ALICE, graph_body(&si(), 1, 1)).await;
        r.expect(status, text);
        assert!(!r.text().contains("SECRET"), "{}", r.text());
    }
    // A port that tries to carry a value in its refusal: only a path-shaped `path` survives.
    let f = fixture();
    let mut error = ApiError::new(Code::GraphQueryInvalid, "value SECRET-IN-MESSAGE rejected");
    error.details.insert("path".to_owned(), json!("body.depth"));
    error.details.insert("value".to_owned(), json!("SECRET-IN-DETAIL"));
    f.graph.fail_next(error);
    let r = query(&f, ALICE, graph_body(&si(), 1, 1)).await;
    r.expect(422, "graph.query_invalid");
    assert_eq!(r.json()["error"]["details"], json!({"path": "body.depth"}));
    assert!(!r.text().contains("SECRET"), "{}", r.text());
    let mut error = ApiError::from_code(Code::GraphQueryInvalid);
    error.details.insert("path".to_owned(), json!("a value with spaces"));
    f.graph.fail_next(error);
    let r = query(&f, ALICE, graph_body(&si(), 1, 1)).await;
    assert!(r.json()["error"].get("details").is_none(), "{}", r.text());
    f.lineage.fail_next(ApiError::from_code(Code::LineageKindUnknown));
    lineage(&f, ALICE, lineage_body("bom", &item(10))).await.expect(422, "lineage.kind_unknown");
}

#[tokio::test]
async fn an_undeclared_port_error_is_http_internal_without_detail() {
    for error in [
        ApiError::new(Code::HttpTimeout, "SECRET-TEXT"),
        ApiError::new(Code::DocumentNotFound, "SECRET-TEXT"),
        ApiError::new(Code::LineageKindUnknown, "SECRET-TEXT"), // not declared for graph_query
    ] {
        let f = fixture();
        f.graph.fail_next(error);
        let r = query(&f, ALICE, graph_body(&si(), 1, 1)).await;
        r.expect(500, "http.internal");
        assert!(!r.text().contains("SECRET"), "{}", r.text());
        assert_eq!(r.header("cache-control"), Some("private, no-store"));
    }
    let f = fixture();
    f.lineage
        .fail_next(ApiError::new(Code::HttpTimeout, "SECRET-TEXT"));
    let r = lineage(&f, ALICE, lineage_body("bom", &item(10))).await;
    assert_eq!(r.status, 500);
    assert_eq!(r.code(), "http.internal");
}

/// A port that answers outside the contract's bounds: never relayed.
struct Hostile;

impl GraphPort for Hostile {
    fn query<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        query: &'a GraphQueryIn,
    ) -> BoxFuture<'a, GraphResult<GraphAnswerOut>> {
        let nodes = vec![query.start.clone(), si(), dn()]; // more than the query's limit
        Box::pin(async move {
            Ok(GraphAnswerOut {
                nodes,
                edges: Vec::new(),
                truncated: false,
                truncation_reason: None,
                withheld: false,
            })
        })
    }
}

impl LineagePort for Hostile {
    fn trace<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        query: &'a LineageQueryIn,
    ) -> BoxFuture<'a, GraphResult<LineageAnswerOut>> {
        // A float-looking quantity: never sent.
        let nodes = vec![LineageNodeOut {
            node: query.start.clone(),
            quantity: Some("1e5".to_owned()),
        }];
        Box::pin(async move {
            let level = LineageLevelOut {
                depth: 0,
                nodes,
                node_count: 1,
                quantity_total: None,
            };
            Ok(LineageAnswerOut {
                levels: vec![level],
                truncated: false,
                truncation_reason: None,
                withheld: false,
            })
        })
    }
}

#[tokio::test]
async fn an_answer_that_breaks_the_contract_is_never_relayed() {
    let router = custom(Some(Arc::new(Hostile)), Some(Arc::new(Hostile)));
    let r = on_graph(&router, HOST_A, ALICE, graph_body(&si(), 1, 2)).await;
    r.expect(500, "http.internal");
    let r = on_lineage(&router, HOST_A, ALICE, lineage_body("bom", &item(10))).await;
    r.expect(500, "http.internal");
}

#[tokio::test]
#[rustfmt::skip]
async fn a_token_reads_nothing_of_another_site() {
    let f = fixture();
    // ONLY_A was granted on site A only: on site B it is nobody.
    on_graph(&f.router, HOST_B, ONLY_A, graph_body(&si(), 2, 10)).await.expect(404, "graph.not_found");
    on_lineage(&f.router, HOST_B, ONLY_A, lineage_body("bom", &item(10))).await.expect(404, "graph.not_found");
    // On site A the token reads site A's graph; the same record on site B answers from site B's
    // graph (it has no delivery note): the site is the request's, never the body's.
    let on_a = query(&f, ONLY_A, graph_body(&si(), 3, 100)).await;
    assert_eq!(on_a.status, 200);
    assert!(on_a.text().contains(&id(2)), "site A has the delivery note");
    let alice_b = on_graph(&f.router, HOST_B, ALICE, graph_body(&si(), 3, 100)).await;
    assert_eq!(alice_b.status, 200);
    assert!(!alice_b.text().contains(&id(2)), "site B has none");
    let mut body = graph_body(&si(), 3, 100);
    body["site"] = json!("acme");
    on_graph(&f.router, HOST_B, ALICE, body).await.expect(422, "graph.query_invalid");
}

#[tokio::test]
#[rustfmt::skip]
async fn with_no_port_every_operation_is_501_never_an_empty_answer() {
    let neither = custom(None, None);
    let graph_only = custom(Some(build_graph()), None);
    let lineage_only = custom(None, Some(build_lineage()));
    for (router, graph_status, lineage_status) in [(&neither, 501, 501), (&graph_only, 200, 501), (&lineage_only, 501, 200)] {
        let g = on_graph(router, HOST_A, ALICE, graph_body(&si(), 1, 1)).await;
        let l = on_lineage(router, HOST_A, ALICE, lineage_body("bom", &item(10))).await;
        assert_eq!((g.status, l.status), (graph_status, lineage_status), "{} {}", g.text(), l.text());
        for r in [g, l].iter().filter(|r| r.status == 501) {
            r.expect(501, "service.not_configured");
            assert_eq!(r.header("cache-control"), Some("private, no-store"));
        }
    }
    // A host that runs one service mounts the group with the stub for the other.
    let stubbed = custom(Some(Arc::new(Unconfigured)), Some(build_lineage()));
    let g = on_graph(&stubbed, HOST_A, ALICE, graph_body(&si(), 1, 1)).await;
    g.expect(501, "service.not_configured");
    assert_eq!(on_lineage(&stubbed, HOST_A, ALICE, lineage_body("bom", &item(10))).await.status, 200);
    // The credential is still checked first.
    let body = serde_json::to_vec(&lineage_body("bom", &item(10))).unwrap();
    let r = send(&neither, HOST_A, "/api/v1/lineage", None, &[], Some("application/json"), body).await;
    r.expect(401, "auth.required");
}

#[test]
fn the_group_registers_both_operations_and_needs_both_ports() {
    assert_eq!(graph::SERVES, ["graph_query", "lineage_trace"]);
    assert_eq!(graph::required_ports().len(), 2);
    let registered = spot_http::api::router::routes::registry_serves();
    for id in graph::SERVES {
        assert!(registered.contains(id), "{id} is in the registry");
    }
    // The registry mounts the group only when both ports are provided (fail closed).
    let mut ports = Ports::new();
    ports.provide::<dyn GraphPort>(build_graph());
    assert!(ports.first_missing(graph::required_ports()).is_some());
    ports.provide::<dyn LineagePort>(build_lineage());
    assert!(ports.first_missing(graph::required_ports()).is_none());
}
