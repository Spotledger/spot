//! `auth_invitation_accept` (WP-IAM-12.T5): the one-use invitation token, one answer for every
//! bad token, a password failing the policy leaves the token usable, no secret out of place,
//! and the group is registered as public in the `auth` rate class.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::Ports;
use spot_http::api::ports::invitation_accept::{
    AcceptIn, InvitationAcceptPort, MemoryInvitationAccept,
};
use spot_http::api::router::routes::{REGISTRY, RateClass, RouteState, invitation_accept};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use time::macros::datetime;
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const OTHER_HOST: &str = "erp.beta.com";
const TOKEN: &str = "tok_0123456789abcdefghij";
const OTHER_TOKEN: &str = "tok_zyxwvutsrqponmlkjihg";
const PASSWORD: &str = "correct horse battery staple";

struct Fixture {
    router: axum::Router,
    port: Arc<MemoryInvitationAccept>,
    clock: Arc<ManualClock>,
}

fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-13 08:00:00 UTC)));
    let port = Arc::new(MemoryInvitationAccept::new(clock.clone(), 12));
    let mut ports = Ports::new();
    ports.provide::<dyn InvitationAcceptPort>(port.clone());
    let state = RouteState::new(ports.subset(invitation_accept::required_ports()).unwrap());
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
    .site_routes(invitation_accept::routes().with_state(state))
    .build();
    port.invite("acme", TOKEN, datetime!(2026-11-16 08:00:00 UTC));
    port.invite("acme", OTHER_TOKEN, datetime!(2026-11-13 09:00:00 UTC));
    Fixture {
        router,
        port,
        clock,
    }
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    raw: String,
    body: Json,
}

async fn post(f: &Fixture, host: &str, uri: &str, content_type: Option<&str>, body: &str) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::HOST, host);
    if let Some(ct) = content_type {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    let request = builder.body(Body::from(body.to_owned())).unwrap();
    let response = f.router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    Reply {
        status,
        headers,
        body: serde_json::from_str(&raw).unwrap_or(Json::Null),
        raw,
    }
}

async fn accept(f: &Fixture, token: &str, password: &str) -> Reply {
    accept_on(f, HOST, token, password).await
}

async fn accept_on(f: &Fixture, host: &str, token: &str, password: &str) -> Reply {
    post(
        f,
        host,
        "/api/v1/auth/invitations/accept",
        Some("application/json"),
        &json!({"token": token, "password": password}).to_string(),
    )
    .await
}

fn code(r: &Reply) -> &str {
    r.body["error"]["code"].as_str().unwrap_or("")
}

#[tokio::test]
async fn a_valid_token_sets_the_password_once() {
    let f = fixture();
    let r = accept(&f, TOKEN, PASSWORD).await;
    assert_eq!(r.status, 200, "{}", r.raw);
    assert_eq!(r.body, json!({"data": {"accepted": true}}));
    assert_eq!(
        r.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store",
        "every answer is no-store"
    );
    assert_eq!(f.port.password_of(TOKEN).as_deref(), Some(PASSWORD));
    // The second use of the same token is refused, and does not change the password.
    let again = accept(&f, TOKEN, "another long password 1").await;
    assert_eq!(again.status, 400, "{}", again.raw);
    assert_eq!(code(&again), "identity.invitation_invalid");
    assert_eq!(f.port.password_of(TOKEN).as_deref(), Some(PASSWORD));
}

#[tokio::test]
async fn every_bad_token_is_the_same_answer() {
    let f = fixture();
    f.port.revoke(OTHER_TOKEN);
    f.port.invite(
        "acme",
        "tok_expired_000000000000",
        datetime!(2026-11-13 07:00:00 UTC),
    );
    let used = accept(&f, TOKEN, PASSWORD).await;
    assert_eq!(used.status, 200);
    let answers = [
        accept(&f, "tok_unknown_00000000000", PASSWORD).await,
        accept(&f, OTHER_TOKEN, PASSWORD).await,
        accept(&f, "tok_expired_000000000000", PASSWORD).await,
        accept(&f, TOKEN, PASSWORD).await,
        // A token of another site is unknown here.
        accept_on(&f, OTHER_HOST, "tok_0123456789abcdefghij", PASSWORD).await,
    ];
    for r in &answers {
        assert_eq!(r.status, 400, "{}", r.raw);
        assert_eq!(code(r), "identity.invitation_invalid", "{}", r.raw);
        let mut first = answers[0].body.clone();
        let mut this = r.body.clone();
        // The request id names the request, never the cause.
        first["error"]["request_id"] = Json::Null;
        this["error"]["request_id"] = Json::Null;
        assert_eq!(this, first, "one answer for every cause");
    }
}

#[tokio::test]
async fn an_expired_token_is_refused_by_the_injected_clock() {
    let f = fixture();
    f.clock.advance(std::time::Duration::from_secs(2 * 3600));
    let r = accept(&f, OTHER_TOKEN, PASSWORD).await;
    assert_eq!(r.status, 400, "{}", r.raw);
    assert_eq!(code(&r), "identity.invitation_invalid");
    // The other token (three days) is still good.
    assert_eq!(accept(&f, TOKEN, PASSWORD).await.status, 200);
}

#[tokio::test]
async fn a_weak_password_is_422_and_leaves_the_token_usable() {
    let f = fixture();
    let weak = accept(&f, TOKEN, "short").await;
    assert_eq!(weak.status, 422, "{}", weak.raw);
    assert_eq!(code(&weak), "auth.password_policy");
    assert!(f.port.password_of(TOKEN).is_none());
    let ok = accept(&f, TOKEN, PASSWORD).await;
    assert_eq!(ok.status, 200, "{}", ok.raw);
    // A weak password on a bad token is the token's answer, not the policy's.
    let bad = accept(&f, "tok_unknown_00000000000", "short").await;
    assert_eq!(code(&bad), "identity.invitation_invalid");
}

#[tokio::test]
async fn the_body_is_a_closed_bounded_object() {
    let f = fixture();
    let uri = "/api/v1/auth/invitations/accept";
    let cases: [(&str, Option<&str>, String); 8] = [
        ("not json", Some("application/json"), "{".to_owned()),
        ("an array", Some("application/json"), "[]".to_owned()),
        (
            "an unknown field",
            Some("application/json"),
            json!({"token": TOKEN, "password": PASSWORD, "user": "x"}).to_string(),
        ),
        (
            "a short token",
            Some("application/json"),
            json!({"token": "short", "password": PASSWORD}).to_string(),
        ),
        (
            "no password",
            Some("application/json"),
            json!({"token": TOKEN}).to_string(),
        ),
        (
            "an empty password",
            Some("application/json"),
            json!({"token": TOKEN, "password": ""}).to_string(),
        ),
        (
            "a long name",
            Some("application/json"),
            json!({"token": TOKEN, "password": PASSWORD, "full_name": "n".repeat(201)}).to_string(),
        ),
        (
            "a form body",
            Some("application/x-www-form-urlencoded"),
            format!("token={TOKEN}"),
        ),
    ];
    for (what, content_type, body) in cases {
        let r = post(&f, HOST, uri, content_type, &body).await;
        assert!(
            (400..500).contains(&r.status),
            "{what}: expected a client error, got {} {}",
            r.status,
            r.raw
        );
        assert!(
            !r.raw.contains(TOKEN) && !r.raw.contains(PASSWORD),
            "{what}: echoes a secret"
        );
    }
    // None of those spent the token.
    assert_eq!(accept(&f, TOKEN, PASSWORD).await.status, 200);
}

#[tokio::test]
async fn a_token_in_the_query_is_refused_and_not_spent() {
    let f = fixture();
    let r = post(
        &f,
        HOST,
        &format!("/api/v1/auth/invitations/accept?token={TOKEN}"),
        Some("application/json"),
        &json!({"token": TOKEN, "password": PASSWORD}).to_string(),
    )
    .await;
    assert_eq!(r.status, 400, "{}", r.raw);
    assert!(!r.raw.contains(TOKEN));
    assert_eq!(accept(&f, TOKEN, PASSWORD).await.status, 200);
}

#[tokio::test]
async fn the_input_hides_its_secrets_in_debug() {
    let input = AcceptIn {
        token: TOKEN.to_owned(),
        password: PASSWORD.to_owned(),
        full_name: Some("Ada".to_owned()),
    };
    let text = format!("{input:?}");
    assert!(!text.contains(TOKEN) && !text.contains(PASSWORD), "{text}");
    assert!(!text.contains("Ada"), "{text}");
}

#[test]
fn the_group_is_registered_public_in_the_auth_class() {
    let group = REGISTRY
        .iter()
        .find(|r| r.name == "invitation_accept")
        .expect("the group is registered");
    assert_eq!(group.serves, ["auth_invitation_accept"]);
    assert_eq!(
        group.public.map(|p| p.rate_class),
        Some(RateClass::Auth),
        "the group runs behind the per-address rate limit"
    );
    assert_eq!((group.required_ports)().len(), 1);
}
