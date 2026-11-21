//! The second-factor routes (WP-PLT-02.T9, contract 0.18.0): the shape of a request and of an
//! answer, with a fake port. Who the caller is, what proof a change needs and what it does to
//! other sessions belong to the port and are tested with the sign-in service.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::ports::totp::{CodesOut, EnrolOut, TotpPort, TotpResult};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{self, RouteState, totp};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::api::{PortKey, Ports};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";

/// Answers canned values and counts the calls it gets.
#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
    last_proof: std::sync::Mutex<Option<(String, String)>>,
    refuse: bool,
}

impl TotpPort for Fake {
    fn enrol<'a>(&'a self, _ctx: &'a RequestCtx) -> BoxFuture<'a, TotpResult<EnrolOut>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let refuse = self.refuse;
        Box::pin(async move {
            if refuse {
                return Err(ApiError::from_code(Code::IdentityTotpState));
            }
            Ok(EnrolOut {
                secret: "JBSWY3DPEHPK3PXP".into(),
                provisioning_uri: "otpauth://totp/Spot:ada?secret=JBSWY3DPEHPK3PXP".into(),
            })
        })
    }

    fn confirm<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<CodesOut>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_proof.lock().unwrap() = Some((String::new(), code.to_owned()));
        Box::pin(async {
            Ok(CodesOut {
                recovery_codes: vec!["AAAA-BBBB".into(), "CCCC-DDDD".into()],
            })
        })
    }

    fn disable<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        password: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<()>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_proof.lock().unwrap() = Some((password.to_owned(), code.to_owned()));
        Box::pin(async { Ok(()) })
    }

    fn regenerate<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        password: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<CodesOut>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last_proof.lock().unwrap() = Some((password.to_owned(), code.to_owned()));
        Box::pin(async {
            Ok(CodesOut {
                recovery_codes: vec!["EEEE-FFFF".into()],
            })
        })
    }
}

struct Fixture {
    router: axum::Router,
    fake: Arc<Fake>,
}

fn fixture(fake: Fake) -> Fixture {
    let fake = Arc::new(fake);
    let mut ports = Ports::new();
    ports.provide::<dyn TotpPort>(fake.clone());
    let keys: &[PortKey] = totp::totp_required_ports();
    let state = RouteState::new(ports.subset(keys).unwrap());
    let acme = SiteId::new("acme").unwrap();
    let router = ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(totp::totp_routes().with_state(state))
    .build();
    Fixture { router, fake }
}

struct Reply {
    status: u16,
    headers: axum::http::HeaderMap,
    body: Json,
}

async fn post(f: &Fixture, path: &str, token: Option<&str>, body: Json) -> Reply {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = f
        .router
        .clone()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body: serde_json::from_slice(&bytes).unwrap_or(Json::Null),
    }
}

#[tokio::test]
async fn the_four_operations_answer_the_contract_shapes_and_are_never_cached() {
    let f = fixture(Fake::default());
    let enrol = post(&f, "/api/v1/auth/totp/enrol", Some("t"), json!({})).await;
    assert_eq!(enrol.status, 200);
    assert_eq!(enrol.body["data"]["secret"], "JBSWY3DPEHPK3PXP");
    assert!(
        enrol.body["data"]["provisioning_uri"]
            .as_str()
            .unwrap()
            .starts_with("otpauth://")
    );

    let confirm = post(
        &f,
        "/api/v1/auth/totp/confirm",
        Some("t"),
        json!({ "code": "123456" }),
    )
    .await;
    assert_eq!(confirm.status, 200);
    assert_eq!(
        confirm.body["data"]["recovery_codes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let disable = post(
        &f,
        "/api/v1/auth/totp/disable",
        Some("t"),
        json!({ "password": "pw", "code": "123456" }),
    )
    .await;
    assert_eq!(disable.status, 204);

    let regenerate = post(
        &f,
        "/api/v1/auth/recovery-codes/regenerate",
        Some("t"),
        json!({ "password": "pw", "code": "AAAA-BBBB" }),
    )
    .await;
    assert_eq!(regenerate.status, 200);
    assert_eq!(regenerate.body["data"]["recovery_codes"][0], "EEEE-FFFF");

    for reply in [&enrol, &confirm, &disable, &regenerate] {
        let cache = reply
            .headers
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cache.contains("no-store"), "{cache}");
    }
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_malformed_body_is_refused_before_the_port_is_asked() {
    let f = fixture(Fake::default());
    for (path, body) in [
        ("/api/v1/auth/totp/confirm", json!({})),
        ("/api/v1/auth/totp/confirm", json!({ "code": "123" })),
        (
            "/api/v1/auth/totp/confirm",
            json!({ "code": "123456", "x": 1 }),
        ),
        ("/api/v1/auth/totp/disable", json!({ "password": "pw" })),
        (
            "/api/v1/auth/totp/disable",
            json!({ "password": "pw", "code": "1", "x": 1 }),
        ),
        (
            "/api/v1/auth/recovery-codes/regenerate",
            json!({ "code": "123456" }),
        ),
        ("/api/v1/auth/recovery-codes/regenerate", json!([1, 2])),
    ] {
        let reply = post(&f, path, Some("t"), body.clone()).await;
        assert_eq!(reply.status, 400, "{path} {body}");
        assert_eq!(reply.body["error"]["code"], "api.invalid_request");
        // The answer never echoes the body.
        assert!(!reply.body.to_string().contains("123456"));
    }
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn no_bearer_is_401_and_the_port_is_not_asked() {
    let f = fixture(Fake::default());
    let reply = post(&f, "/api/v1/auth/totp/enrol", None, json!({})).await;
    assert_eq!(reply.status, 401);
    assert_eq!(f.fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_proof_reaches_the_port_unchanged() {
    let f = fixture(Fake::default());
    post(
        &f,
        "/api/v1/auth/totp/disable",
        Some("t"),
        json!({ "password": "correct horse", "code": "654321" }),
    )
    .await;
    assert_eq!(
        f.fake.last_proof.lock().unwrap().clone(),
        Some(("correct horse".to_owned(), "654321".to_owned()))
    );
}

#[tokio::test]
async fn a_port_refusal_keeps_its_code_and_status() {
    let f = fixture(Fake {
        refuse: true,
        ..Fake::default()
    });
    let reply = post(&f, "/api/v1/auth/totp/enrol", Some("t"), json!({})).await;
    assert_eq!(reply.status, 409);
    assert_eq!(reply.body["error"]["code"], "identity.totp_state");
    assert!(reply.headers.get(header::CACHE_CONTROL).is_some());
}

#[test]
fn a_group_without_its_port_is_not_mounted() {
    let ports = Ports::new();
    let mounted = routes::mount(&[totp::registration()], &ports, &[]);
    assert!(
        mounted
            .report
            .skipped
            .iter()
            .any(|(name, _)| *name == "totp"),
        "the totp group must be skipped when its port is absent"
    );
}
