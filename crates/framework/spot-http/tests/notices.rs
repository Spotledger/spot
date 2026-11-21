//! `notification_list` (contract 0.10.0, WP-CON-03.T14): the caller's own notices, oldest
//! first, paged by `after` and `limit`, never an existence oracle.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::ports::notices::{MemoryNoticeSource, Notice, NoticeSource};
use spot_http::api::router::routes::{RouteState, notices};
use spot_http::api::{PortKey, Ports};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const OTHER_HOST: &str = "erp.beta.com";

fn notice(n: usize) -> Notice {
    let mut data = serde_json::Map::new();
    data.insert("title".into(), json!(format!("Notice {n}")));
    Notice {
        event_id: format!("ev{n:03}"),
        event: "user_notice".into(),
        data,
        created_at: format!("2026-10-31T08:{:02}:00Z", n % 60),
    }
}

struct Fixture {
    router: axum::Router,
    source: Arc<MemoryNoticeSource>,
}

fn fixture() -> Fixture {
    let source = Arc::new(MemoryNoticeSource::new());
    let mut ports = Ports::new();
    ports.provide::<dyn NoticeSource>(source.clone());
    let keys: &[PortKey] = notices::required_ports();
    let state = RouteState::new(ports.subset(keys).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let beta = SiteId::new("beta").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme)
            .add(OTHER_HOST, beta.clone())
            .add_site_name(beta),
    )
    .site_routes(notices::routes().with_state(state))
    .build();
    Fixture { router, source }
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    body: Json,
}

async fn get(f: &Fixture, host: &str, path: &str, token: Option<&str>) -> Reply {
    let mut builder = Request::builder()
        .method("GET")
        .uri(path)
        .header(header::HOST, host);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = f
        .router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
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

fn ids(reply: &Reply) -> Vec<String> {
    reply.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["event_id"].as_str().unwrap().to_owned())
        .collect()
}

fn seeded(count: usize) -> Fixture {
    let f = fixture();
    f.source.grant("acme", "tok-alice", "alice");
    f.source.grant("acme", "tok-bob", "bob");
    f.source.grant("beta", "tok-beta", "alice");
    for n in 1..=count {
        f.source.push("acme", "alice", notice(n));
    }
    f.source.push("acme", "bob", notice(900));
    f.source.push("beta", "alice", notice(800));
    f
}

#[tokio::test]
async fn the_feed_is_oldest_first_with_the_contract_item_shape() {
    let f = seeded(3);
    let r = get(&f, HOST, "/api/v1/notifications", Some("tok-alice")).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(ids(&r), ["ev001", "ev002", "ev003"]);
    assert_eq!(
        r.body["items"][0],
        json!({"event_id": "ev001", "event": "user_notice",
               "data": {"title": "Notice 1"}, "created_at": "2026-10-31T08:01:00Z"})
    );
    assert_eq!(r.body["next"], "ev003");
    assert_eq!(
        r.headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    assert!(r.headers.get("x-request-id").is_some());
}

#[tokio::test]
async fn limit_defaults_to_50_and_caps_at_100() {
    let f = seeded(120);
    let r = get(&f, HOST, "/api/v1/notifications", Some("tok-alice")).await;
    assert_eq!(ids(&r).len(), 50);
    let r = get(
        &f,
        HOST,
        "/api/v1/notifications?limit=100",
        Some("tok-alice"),
    )
    .await;
    assert_eq!(ids(&r).len(), 100);
    let r = get(&f, HOST, "/api/v1/notifications?limit=2", Some("tok-alice")).await;
    assert_eq!(ids(&r), ["ev001", "ev002"]);
    assert_eq!(r.body["next"], "ev002");
    for bad in [
        "0",
        "101",
        "-1",
        "abc",
        "",
        "1.5",
        "+5",
        "99999999999999999999",
    ] {
        let r = get(
            &f,
            HOST,
            &format!("/api/v1/notifications?limit={bad}"),
            Some("tok-alice"),
        )
        .await;
        assert_eq!(r.status, 400, "limit={bad}");
        assert_eq!(
            r.body["error"]["code"], "api.invalid_request",
            "limit={bad}"
        );
    }
}

#[tokio::test]
async fn after_pages_through_the_feed_without_gaps_or_repeats() {
    let f = seeded(5);
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let path = match &after {
            Some(id) => format!("/api/v1/notifications?limit=2&after={id}"),
            None => "/api/v1/notifications?limit=2".to_owned(),
        };
        let r = get(&f, HOST, &path, Some("tok-alice")).await;
        assert_eq!(r.status, 200);
        let page = ids(&r);
        if page.is_empty() {
            break;
        }
        seen.extend(page);
        after = r.body["next"].as_str().map(str::to_owned);
    }
    assert_eq!(seen, ["ev001", "ev002", "ev003", "ev004", "ev005"]);
}

#[tokio::test]
async fn an_empty_page_has_no_next() {
    let f = seeded(2);
    let r = get(
        &f,
        HOST,
        "/api/v1/notifications?after=ev002",
        Some("tok-alice"),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body, json!({"items": []}));
    assert!(r.body.get("next").is_none());
    // A user with no notices at all.
    let f = fixture();
    f.source.grant("acme", "tok-new", "new");
    let r = get(&f, HOST, "/api/v1/notifications", Some("tok-new")).await;
    assert_eq!(r.body, json!({"items": []}));
}

#[tokio::test]
async fn a_caller_sees_only_their_own_notices_on_their_own_site() {
    let f = seeded(2);
    let bob = get(&f, HOST, "/api/v1/notifications", Some("tok-bob")).await;
    assert_eq!(ids(&bob), ["ev900"]);
    let alice = get(&f, HOST, "/api/v1/notifications", Some("tok-alice")).await;
    assert!(!ids(&alice).contains(&"ev900".to_owned()));
    // The same user name on another site has another feed.
    let beta = get(&f, OTHER_HOST, "/api/v1/notifications", Some("tok-beta")).await;
    assert_eq!(ids(&beta), ["ev800"]);
    // A token of one site is nothing on another.
    let wrong = get(&f, OTHER_HOST, "/api/v1/notifications", Some("tok-alice")).await;
    assert_eq!(wrong.status, 401);
}

#[tokio::test]
async fn another_users_after_id_is_the_same_answer_as_a_forged_one() {
    let f = seeded(3);
    let forged = get(
        &f,
        HOST,
        "/api/v1/notifications?after=nope",
        Some("tok-alice"),
    )
    .await;
    let others = get(
        &f,
        HOST,
        "/api/v1/notifications?after=ev900",
        Some("tok-alice"),
    )
    .await;
    let cross_site = get(
        &f,
        HOST,
        "/api/v1/notifications?after=ev800",
        Some("tok-alice"),
    )
    .await;
    for r in [&forged, &others, &cross_site] {
        assert_eq!(r.status, 400);
        assert_eq!(r.body["error"]["code"], "api.invalid_request");
    }
    // Identical but for the request id: nothing tells the three apart.
    let strip = |r: &Reply| {
        let mut b = r.body.clone();
        b["error"].as_object_mut().unwrap().remove("request_id");
        b
    };
    assert_eq!(strip(&forged), strip(&others));
    assert_eq!(strip(&forged), strip(&cross_site));
}

#[tokio::test]
async fn malformed_queries_are_invalid_requests() {
    let f = seeded(1);
    let long = "a".repeat(129);
    for q in [
        "after=".to_owned(),
        format!("after={long}"),
        "after=%ZZ".to_owned(),
        "after=a%20b".to_owned(),
        "after=ev001&after=ev001".to_owned(),
        "limit=5&limit=5".to_owned(),
        "user=bob".to_owned(),
        "room=x".to_owned(),
    ] {
        let r = get(
            &f,
            HOST,
            &format!("/api/v1/notifications?{q}"),
            Some("tok-alice"),
        )
        .await;
        assert_eq!(r.status, 400, "{q}");
        assert_eq!(r.body["error"]["code"], "api.invalid_request", "{q}");
    }
    // A percent-encoded id decodes to the stored one.
    let f = fixture();
    f.source.grant("acme", "t", "u");
    let mut n = notice(1);
    n.event_id = "a/b".into();
    f.source.push("acme", "u", n);
    f.source.push("acme", "u", notice(2));
    let r = get(&f, HOST, "/api/v1/notifications?after=a%2Fb", Some("t")).await;
    assert_eq!(ids(&r), ["ev002"]);
}

#[tokio::test]
async fn no_credential_or_an_unknown_one_is_refused() {
    let f = seeded(1);
    let none = get(&f, HOST, "/api/v1/notifications", None).await;
    assert_eq!(none.status, 401);
    assert_eq!(none.body["error"]["code"], "auth.required");
    let unknown = get(&f, HOST, "/api/v1/notifications", Some("tok-forged")).await;
    assert_eq!(unknown.status, 401);
    assert!(unknown.body.get("items").is_none());
}

#[tokio::test]
async fn an_item_outside_the_contract_is_an_internal_error_not_a_page() {
    let f = fixture();
    f.source.grant("acme", "t", "u");
    let mut big = notice(1);
    big.data.insert("blob".into(), json!("x".repeat(3000)));
    f.source.push("acme", "u", big);
    let r = get(&f, HOST, "/api/v1/notifications", Some("t")).await;
    assert_eq!(r.status, 500);
    assert!(r.body.get("items").is_none());
    let f = fixture();
    f.source.grant("acme", "t", "u");
    let mut bad = notice(1);
    bad.event = "Not A Token".into();
    f.source.push("acme", "u", bad);
    assert_eq!(
        get(&f, HOST, "/api/v1/notifications", Some("t"))
            .await
            .status,
        500
    );
}
