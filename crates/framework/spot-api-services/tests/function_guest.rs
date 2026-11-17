//! WP-PLT-17.T3: `call_function` lets a request with no credential at all reach the function
//! delegate (which answers a guest only for guest functions), while a request that carries a
//! credential that does not resolve is refused and never reaches it. Reports keep failing closed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value as Json, json};
use spot_api_services::{Delegates, FunctionDelegate, ReportDelegate};
use spot_core::SiteId;
use spot_http::api::registry::Code;
use spot_http::api::service::ResourceService as _;
use spot_http::api::service::{ApiError, BoxFuture, Credential, Message, ReportResult, RequestCtx};
use spot_http::request::RequestId;
use support::App;

fn ctx(credential: Option<&str>) -> RequestCtx {
    RequestCtx {
        site: SiteId::new("test").unwrap(),
        credential: credential.map(Credential::new),
        idempotency_key: None,
        language: None,
        request_id: RequestId::generate(),
        peer: None,
    }
}

#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
}

impl FunctionDelegate for Fake {
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        _app: &'a str,
        _module: &'a str,
        _function: &'a str,
        _args: Json,
        _via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<Message>), ApiError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok((json!({"credential": ctx.credential.is_some()}), vec![])) })
    }
}

impl ReportDelegate for Fake {
    fn run<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        _report: &'a str,
        _filters: Json,
        _limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Err(ApiError::app("fake.ran", "the report delegate ran")) })
    }
}

async fn app(fake: &Arc<Fake>) -> App {
    App::with_delegates(
        Delegates::none()
            .with_function(fake.clone())
            .with_report(fake.clone()),
    )
    .await
}

#[tokio::test]
async fn a_request_with_no_credential_reaches_the_function_delegate() {
    let fake = Arc::new(Fake::default());
    let app = app(&fake).await;
    let (data, _) = app
        .resources
        .call_function(&ctx(None), "a", "m", "f", json!({}), true)
        .await
        .unwrap();
    assert_eq!(data["credential"], false);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_credential_that_does_not_resolve_never_reaches_the_delegate() {
    let fake = Arc::new(Fake::default());
    let app = app(&fake).await;
    for bad in ["expired-or-revoked", "", "resolved-test-user-but-not"] {
        let error = app
            .resources
            .call_function(&ctx(Some(bad)), "a", "m", "f", json!({}), false)
            .await
            .unwrap_err();
        assert_eq!(error.code.as_str(), Code::AuthRequired.as_str(), "{bad:?}");
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    // A credential that resolves still reaches it.
    app.resources
        .call_function(
            &ctx(Some("resolved-test-user")),
            "a",
            "m",
            "f",
            json!({}),
            false,
        )
        .await
        .unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn reports_still_fail_closed_without_a_credential() {
    let fake = Arc::new(Fake::default());
    let app = app(&fake).await;
    let error = app
        .resources
        .run_report(&ctx(None), "r", json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(error.code.as_str(), Code::AuthRequired.as_str());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}
