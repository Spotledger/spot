//! The `/scim/v2` routes (WP-IAM-06.T2) over a fake [`ScimPort`]: what the route hands the port
//! (method, path, decoded query, `If-Match`, body bytes), what it checks before the port (body
//! bound, query syntax), and how a reply is rendered. The fake decides nothing the real adapter
//! decides; it records what the route passed it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::Value as Json;
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::ports::scim::{ScimCall, ScimPort, ScimReply};
use spot_http::api::router::routes::{RouteState, scim};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const MAX_BODY: usize = 64;

struct Fake {
    /// What the port received: `(method, path, query pairs, if-match, body length, token)`.
    #[allow(clippy::type_complexity)]
    calls: Mutex<
        Vec<(
            String,
            String,
            Vec<(String, String)>,
            Option<String>,
            usize,
            String,
        )>,
    >,
    reply: Mutex<Option<ScimReply>>,
}

impl ScimPort for Fake {
    fn max_body_bytes(&self) -> usize {
        MAX_BODY
    }

    fn serve<'a>(&'a self, ctx: &'a RequestCtx, call: ScimCall) -> BoxFuture<'a, ScimReply> {
        Box::pin(async move {
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .unwrap_or_default();
            self.calls.lock().unwrap().push((
                call.method.clone(),
                call.path.clone(),
                call.query.clone().into_iter().collect(),
                call.if_match.clone(),
                call.body.len(),
                token,
            ));
            self.reply.lock().unwrap().take().unwrap_or(ScimReply {
                status: 200,
                body: "{\"ok\":true}".to_owned(),
                etag: None,
                location: None,
                retry_after: None,
            })
        })
    }
}

struct Fixture {
    router: axum::Router,
    fake: Arc<Fake>,
}

fn fixture() -> Fixture {
    let fake = Arc::new(Fake {
        calls: Mutex::new(Vec::new()),
        reply: Mutex::new(None),
    });
    let mut ports = Ports::new();
    ports.provide::<dyn ScimPort>(fake.clone());
    let state = RouteState::new(ports.subset(scim::required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(scim::routes().with_state(state))
    .build();
    Fixture { router, fake }
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
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

async fn send(
    f: &Fixture,
    method: &str,
    uri: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let request = builder.body(Body::from(body.to_owned())).unwrap();
    let response = f.router.clone().oneshot(request).await.unwrap();
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

#[tokio::test]
async fn the_route_hands_the_port_the_parts_of_the_request() {
    let f = fixture();
    let reply = send(
        &f,
        "PATCH",
        "/scim/v2/Users/abc?filter=userName%20eq%20%22a%2Bb%22&count=5",
        Some("spk_a_b"),
        &[
            ("If-Match", "W/\"3\""),
            ("Content-Type", "application/scim+json"),
        ],
        "{\"x\":1}",
    )
    .await;
    assert_eq!(reply.status, 200);
    let calls = f.fake.calls.lock().unwrap();
    let (method, path, query, if_match, body, token) = &calls[0];
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("PATCH", "/scim/v2/Users/abc")
    );
    assert!(
        query.contains(&("filter".to_owned(), "userName eq \"a+b\"".to_owned())),
        "{query:?}"
    );
    assert!(query.contains(&("count".to_owned(), "5".to_owned())));
    assert_eq!(if_match.as_deref(), Some("W/\"3\""));
    assert_eq!((*body, token.as_str()), (7, "spk_a_b"));
}

#[tokio::test]
async fn the_base_and_every_method_reach_the_port_and_the_route_adds_no_auth_of_its_own() {
    let f = fixture();
    for (method, uri) in [
        ("GET", "/scim/v2"),
        ("GET", "/scim/v2/ServiceProviderConfig"),
        ("POST", "/scim/v2/Users"),
        ("PUT", "/scim/v2/Users/x"),
        ("DELETE", "/scim/v2/Groups/x"),
        ("OPTIONS", "/scim/v2/Users"),
    ] {
        // No token: the route still hands the call to the port, which answers 401 itself.
        assert_eq!(
            send(&f, method, uri, None, &[], "").await.status,
            200,
            "{method} {uri}"
        );
    }
    assert_eq!(f.fake.calls.lock().unwrap().len(), 6);
    assert_eq!(f.fake.calls.lock().unwrap()[0].5, "");
}

#[tokio::test]
async fn nothing_outside_the_scim_base_is_served() {
    let f = fixture();
    for uri in [
        "/api/v1/resource/user",
        "/scim",
        "/scim/v3/Users",
        "/scimv2/Users",
    ] {
        assert_eq!(
            send(&f, "GET", uri, Some("t"), &[], "").await.status,
            404,
            "{uri}"
        );
    }
    assert!(f.fake.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_body_over_the_bound_is_413_in_scim_form_before_the_port() {
    let f = fixture();
    let big = "x".repeat(MAX_BODY + 1);
    let reply = send(&f, "POST", "/scim/v2/Users", Some("t"), &[], &big).await;
    assert_eq!(reply.status, 413);
    assert_eq!(reply.header("content-type"), Some("application/scim+json"));
    let body = reply.json();
    assert_eq!(body["status"], "413");
    assert_eq!(body["scimType"], "tooLarge");
    assert!(!String::from_utf8_lossy(&reply.bytes).contains("xxxx"));
    assert!(f.fake.calls.lock().unwrap().is_empty());
    // Exactly the bound is accepted.
    let ok = send(
        &f,
        "POST",
        "/scim/v2/Users",
        Some("t"),
        &[],
        &"y".repeat(MAX_BODY),
    )
    .await;
    assert_eq!(ok.status, 200);
}

#[tokio::test]
async fn a_reply_is_rendered_as_scim_json_with_its_headers_and_never_cached() {
    let f = fixture();
    *f.fake.reply.lock().unwrap() = Some(ScimReply {
        status: 201,
        body: "{\"id\":\"1\"}".to_owned(),
        etag: Some("W/\"1\"".to_owned()),
        location: Some("/scim/v2/Users/1".to_owned()),
        retry_after: None,
    });
    let created = send(&f, "POST", "/scim/v2/Users", Some("t"), &[], "{}").await;
    assert_eq!(created.status, 201);
    assert_eq!(
        created.header("content-type"),
        Some("application/scim+json")
    );
    assert_eq!(created.header("etag"), Some("W/\"1\""));
    assert_eq!(created.header("location"), Some("/scim/v2/Users/1"));
    assert_eq!(created.header("cache-control"), Some("private, no-store"));
    let mut limited = ScimReply::error(429, Some("tooMany"), "slow down");
    limited.retry_after = Some(9);
    *f.fake.reply.lock().unwrap() = Some(limited);
    let slow = send(&f, "GET", "/scim/v2/Users", Some("t"), &[], "").await;
    assert_eq!(slow.status, 429);
    assert_eq!(slow.header("retry-after"), Some("9"));
    assert_eq!(
        slow.json()["schemas"][0],
        "urn:ietf:params:scim:api:messages:2.0:Error"
    );
}

#[test]
fn the_group_is_not_mounted_without_its_port() {
    let ports = Ports::new();
    let mounted = spot_http::api::router::routes::mount(&[scim::registration()], &ports, &[]);
    assert_eq!(mounted.report.skipped.len(), 1);
    assert_eq!(mounted.report.skipped[0].0, "scim");
}
