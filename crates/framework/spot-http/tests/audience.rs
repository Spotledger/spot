//! The audience check at the door (ADR-083 Barrier 1): a credential whose audience the route's
//! mark does not list is refused with 403 before any handler or idempotency work, a missing
//! bearer stays 401, an invalid credential is the auth port's own 401, and a router built from
//! the table is strict about marks.
//!
//! The auth port here is a table of test tokens, each with the audience (or the error) the port
//! reports; it delegates sign-in, sign-out and session to the fake. The real `LoginService`
//! behind the same trait is covered in `spot-login/tests/audience.rs` (spot-login depends on
//! this crate, so this crate cannot depend on it).
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[allow(dead_code)]
mod support;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use spot_core::{Audience, ManualClock, SiteId};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::service::BoxFuture;
use spot_http::api::{
    ApiError, ApiServices, AuthService, Code, LoginBody, RequestCtx, RouteAuth, Session,
    SessionInfo, has_hand_wired_handler, route_table, router, router_for,
};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use support::fake::FakeApp;
use tower::ServiceExt as _;

const REFUSAL: &str = "This credential cannot be used on this route.";

/// Reports the audience (or error) a test token stands for.
struct Audiences {
    inner: Arc<FakeApp>,
    table: HashMap<&'static str, Result<Audience, Code>>,
}

impl AuthService for Audiences {
    fn login<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        body: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>> {
        self.inner.login(ctx, body)
    }

    fn logout<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>> {
        self.inner.logout(ctx)
    }

    fn session<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>> {
        self.inner.session(ctx)
    }

    fn audience_of<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<Audience, ApiError>> {
        let answer = ctx
            .credential
            .as_ref()
            .and_then(|credential| self.table.get(credential.token.as_str()).cloned());
        Box::pin(async move {
            match answer {
                Some(Ok(audience)) => Ok(audience),
                Some(Err(code)) => Err(ApiError::from_code(code)),
                None => Err(ApiError::from_code(Code::AuthInvalidCredentials)),
            }
        })
    }
}

/// The full server router (site resolution included) over the audience table.
fn app() -> Router {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    let auth = Arc::new(Audiences {
        inner: fake.clone(),
        table: HashMap::from([
            ("tok-staff", Ok(Audience::Staff)),
            ("tok-partner", Ok(Audience::Partner)),
            ("tok-consumer", Ok(Audience::Consumer)),
            ("tok-expired", Err(Code::AuthSessionExpired)),
        ]),
    });
    let services = ApiServices::new(fake.clone(), fake.clone(), fake, auth, store);
    let site = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add("erp.acme.com", site.clone())
            .add_site_name(site),
    )
    .site_routes(router(services))
    .build()
}

fn sample(path: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in path.chars() {
        match c {
            '{' => {
                inside = true;
                out.push_str("sample");
            }
            '}' => inside = false,
            _ if inside => {}
            _ => out.push(c),
        }
    }
    out
}

async fn call(app: &Router, method: &str, path: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut request = Request::builder()
        .method(method)
        .uri(sample(path))
        .header("host", "erp.acme.com");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

/// Every table route that has a hand-wired handler and needs a bearer, minus the two that need
/// an optional service this harness does not wire.
fn secured_entries() -> impl Iterator<Item = &'static spot_http::api::RouteEntry> {
    route_table().iter().filter(|e| {
        e.auth == RouteAuth::Required
            && has_hand_wired_handler(e.operation)
            && !matches!(e.operation, "auth_permissions" | "field_reveal")
    })
}

#[tokio::test]
async fn a_consumer_credential_is_refused_with_403_on_every_route_before_any_handler() {
    let app = app();
    for entry in secured_entries() {
        let (status, body) = call(&app, entry.method, entry.path, Some("tok-consumer")).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{} {}",
            entry.method,
            entry.path
        );
        assert!(
            body.contains(REFUSAL),
            "{} {}: {body}",
            entry.method,
            entry.path
        );
        assert!(body.contains("permission.denied"));
        // The body names no audience.
        for word in ["consumer", "partner", "staff"] {
            assert!(!body.to_lowercase().contains(word), "{word} in {body}");
        }
    }
}

#[tokio::test]
async fn partner_and_staff_credentials_pass_the_check_on_every_route_marked_for_them() {
    let app = app();
    for entry in secured_entries() {
        for (token, audience) in [
            ("tok-partner", Audience::Partner),
            ("tok-staff", Audience::Staff),
        ] {
            assert!(entry.audiences.contains(&audience), "{}", entry.operation);
            let (_, body) = call(&app, entry.method, entry.path, Some(token)).await;
            assert!(
                !body.contains(REFUSAL),
                "{token} was refused on {} {}",
                entry.method,
                entry.path
            );
        }
    }
}

#[tokio::test]
async fn no_token_is_401_auth_required_and_the_login_stays_reachable() {
    let app = app();
    for entry in secured_entries() {
        let (status, body) = call(&app, entry.method, entry.path, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{} {}",
            entry.method,
            entry.path
        );
        assert!(body.contains("auth.required"), "{body}");
    }
    let (status, body) = call(&app, "POST", "/api/v1/auth/login", None).await;
    assert!(!body.contains("auth.required"), "{status}: {body}");
}

#[tokio::test]
async fn an_invalid_or_expired_credential_is_the_auth_ports_own_401() {
    let app = app();
    for token in ["tok-expired", "tok-unknown", "spk_consumer_shaped_key"] {
        let (status, body) = call(&app, "GET", "/api/v1/auth/session", Some(token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{token}: {body}");
        assert!(!body.contains(REFUSAL));
    }
}

#[tokio::test]
async fn a_refused_command_reserves_no_idempotency_key() {
    // The refusal is before the idempotency middleware: it never reads the body or the key. A
    // command with a key and a consumer credential is the plain 403 and a retry with a valid
    // credential and the same key is not answered from a stored response.
    let app = app();
    let send = |token: &'static str| {
        let app = app.clone();
        async move {
            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/resource/Note")
                .header("host", "erp.acme.com")
                .header("authorization", format!("Bearer {token}"))
                .header("idempotency-key", "11111111-2222-3333-4444-555555555555")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap();
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            (
                status,
                headers,
                String::from_utf8_lossy(&bytes).into_owned(),
            )
        }
    };
    let (status, _, body) = send("tok-consumer").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains(REFUSAL));
    let (_, headers, body) = send("tok-staff").await;
    assert!(!body.contains(REFUSAL));
    assert!(
        headers
            .get(spot_http::api::router::REPLAYED_HEADER)
            .is_none(),
        "the second call must not be a replay of the refusal"
    );
}

#[tokio::test]
async fn a_router_for_the_consumer_audience_has_no_route_to_check() {
    // Nothing is marked consumer today: absent, not denied.
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    let services = ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store);
    let consumer = router_for(&[Audience::Consumer], services);
    for entry in secured_entries() {
        let (status, _) = call(&consumer, entry.method, entry.path, Some("tok-consumer")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
