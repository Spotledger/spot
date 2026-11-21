//! Fixture and wire helpers of the task inbox tests (WP-BPM-05.T7).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Request, header};
use serde_json::Value as Json;
use spot_core::SiteId;
use spot_http::api::ports::task::{MemoryTaskPort, TaskPort, TaskSeed};
use spot_http::api::router::routes::{RouteState, task};
use spot_http::api::{PortKey, Ports};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tower::ServiceExt as _;

pub const HOST: &str = "erp.acme.com";
pub const OTHER_HOST: &str = "erp.beta.com";

pub struct Fixture {
    pub router: axum::Router,
    pub port: Arc<MemoryTaskPort>,
}

fn sites() -> StaticSites {
    let acme = SiteId::new("acme").unwrap();
    let beta = SiteId::new("beta").unwrap();
    StaticSites::new()
        .add(HOST, acme.clone())
        .add_site_name(acme)
        .add(OTHER_HOST, beta.clone())
        .add_site_name(beta)
}

fn build(ports: Ports) -> axum::Router {
    let keys: &[PortKey] = task::required_ports();
    let state = RouteState::new(ports.subset(keys).unwrap_or_default());
    ServerBuilder::new(ServerConfig::default(), sites())
        .site_routes(task::routes().with_state(state))
        .build()
}

/// Acme: alice and dave are the assignees of `t_one` (an expense, overdue; dave created its
/// document), alice alone of `t_two` (a leave request); `mgr` supervises both; bob and carol are
/// users with no part in either. Beta has its own alice with `t_beta`.
pub fn fixture() -> Fixture {
    let port = Arc::new(MemoryTaskPort::new(
        OffsetDateTime::parse("2026-11-12T00:00:00Z", &Rfc3339).unwrap(),
    ));
    for (token, user) in [
        ("tok-alice", "alice"),
        ("tok-bob", "bob"),
        ("tok-carol", "carol"),
        ("tok-dave", "dave"),
        ("tok-mgr", "mgr"),
    ] {
        port.grant("acme", token, user);
    }
    port.grant("beta", "tok-beta", "alice");
    let mut one = TaskSeed::new(
        "t_one",
        "expense",
        "Approve the expense",
        &["alice", "dave"],
    );
    one.supervisors = vec!["mgr".to_owned()];
    one.segregated = vec!["dave".to_owned()];
    one.view.due_at = Some("2026-11-01T00:00:00Z".to_owned());
    port.add_task("acme", one);
    let mut two = TaskSeed::new("t_two", "leave", "Approve the leave", &["alice"]);
    two.supervisors = vec!["mgr".to_owned()];
    port.add_task("acme", two);
    port.add_task(
        "beta",
        TaskSeed::new("t_beta", "expense", "Beta only", &["alice"]),
    );
    let mut ports = Ports::new();
    ports.provide::<dyn TaskPort>(port.clone());
    Fixture {
        router: build(ports),
        port,
    }
}

pub fn unconfigured() -> axum::Router {
    build(Ports::new())
}

pub struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Json,
}

pub async fn send_on(
    router: &axum::Router,
    host: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<Json>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in extra {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Json::Null),
    }
}

pub async fn send(
    f: &Fixture,
    method: &str,
    path: &str,
    token: Option<&str>,
    extra: &[(&str, &str)],
    body: Option<Json>,
) -> Reply {
    send_on(&f.router, HOST, method, path, token, extra, body).await
}

pub fn code(reply: &Reply) -> &str {
    reply.body["error"]["code"].as_str().unwrap_or("")
}

pub fn etag(reply: &Reply) -> String {
    reply
        .headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

pub fn ids(reply: &Reply) -> Vec<String> {
    reply.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_owned())
        .collect()
}

/// A reply with what legitimately differs between two requests removed: the request id.
pub fn stripped(reply: &Reply) -> (u16, Json, Vec<(String, String)>) {
    let mut body = reply.body.clone();
    if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
        error.remove("request_id");
    }
    let mut headers: Vec<(String, String)> = reply
        .headers
        .iter()
        .filter(|(name, _)| !matches!(name.as_str(), "x-request-id" | "date"))
        .map(|(name, value)| (name.to_string(), value.to_str().unwrap_or("").to_owned()))
        .collect();
    headers.sort();
    (reply.status, body, headers)
}

pub fn if_match(version: &str) -> (&'static str, String) {
    ("if-match", format!("\"{version}\""))
}

pub async fn post_with(
    f: &Fixture,
    path: &str,
    token: &str,
    version: &str,
    key: Option<&str>,
    body: Option<Json>,
) -> Reply {
    let (name, value) = if_match(version);
    let mut extra = vec![(name, value.as_str())];
    if let Some(key) = key {
        extra.push(("idempotency-key", key));
    }
    send(f, "POST", path, Some(token), &extra, body).await
}
