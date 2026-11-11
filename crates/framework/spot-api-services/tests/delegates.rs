//! `call_function` and `run_report` hand off to an installed delegate and otherwise answer as
//! they did before delegates existed (WP-PRD-01.T29).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value as Json, json};
use spot_api_services::{Delegates, FunctionDelegate, ReportDelegate};
use spot_core::SiteId;
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, BoxFuture, Message, ReportColumn, ReportResult, RequestCtx, ResourceService,
};
use spot_http::request::RequestId;
use support::App;

fn ctx() -> RequestCtx {
    RequestCtx {
        site: SiteId::new("test").unwrap(),
        credential: Some(spot_http::api::service::Credential::new(
            "resolved-test-user",
        )),
        idempotency_key: Some("key-1".into()),
        language: Some("ur-PK".into()),
        request_id: RequestId::generate(),
        peer: None,
    }
}

#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
    fail: bool,
}

impl FunctionDelegate for Fake {
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<Message>), ApiError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail {
                return Err(ApiError::app("fake.refused", "refused by the delegate"));
            }
            Ok((
                json!({
                    "site": ctx.site.as_str(),
                    "key": ctx.idempotency_key,
                    "path": format!("{app}.{module}.{function}"),
                    "args": args,
                    "via_get": via_get,
                }),
                vec![],
            ))
        })
    }
}

impl ReportDelegate for Fake {
    fn run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        _filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail {
                return Err(ApiError::app("fake.refused", "refused by the delegate"));
            }
            let _ = ctx;
            Ok((
                ReportResult {
                    columns: Vec::<ReportColumn>::new(),
                    rows: vec![vec![json!(report), json!(limit)]],
                    truncated: false,
                },
                vec![],
            ))
        })
    }
}

#[tokio::test]
async fn with_no_delegate_both_operations_answer_as_before() {
    let app = App::new().await;
    let ctx = ctx();
    let function = app
        .resources
        .call_function(&ctx, "a", "m", "f", json!({}), false)
        .await
        .unwrap_err();
    assert_eq!(function.code.as_str(), Code::MethodNotFound.as_str());
    assert_eq!(
        function.message,
        "Function calls are not yet implemented in the engine."
    );
    let report = app
        .resources
        .run_report(&ctx, "r", json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(report.code.as_str(), Code::ReportNotFound.as_str());
    assert_eq!(
        report.message,
        "Reports are not yet implemented in the engine."
    );
}

#[tokio::test]
async fn a_delegate_answers_with_the_request_context_untouched() {
    let fake = Arc::new(Fake::default());
    let app = App::with_delegates(
        Delegates::none()
            .with_function(fake.clone())
            .with_report(fake.clone()),
    )
    .await;
    let ctx = ctx();
    let (data, _) = app
        .resources
        .call_function(&ctx, "a", "m", "f", json!({"x": 1}), true)
        .await
        .unwrap();
    assert_eq!(data["site"], "test");
    assert_eq!(data["key"], "key-1");
    assert_eq!(data["path"], "a.m.f");
    assert_eq!(data["args"], json!({"x": 1}));
    assert_eq!(data["via_get"], true);
    let (result, _) = app
        .resources
        .run_report(&ctx, "r", json!({}), Some(5))
        .await
        .unwrap();
    assert_eq!(result.rows, vec![vec![json!("r"), json!(5)]]);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_delegate_error_is_returned_as_is_and_never_becomes_success() {
    let fake = Arc::new(Fake {
        fail: true,
        ..Fake::default()
    });
    let app = App::with_delegates(
        Delegates::none()
            .with_function(fake.clone())
            .with_report(fake.clone()),
    )
    .await;
    let ctx = ctx();
    let function = app
        .resources
        .call_function(&ctx, "a", "m", "f", json!({}), false)
        .await
        .unwrap_err();
    assert_eq!(function.code.as_str(), "fake.refused");
    assert_eq!(function.message, "refused by the delegate");
    let report = app
        .resources
        .run_report(&ctx, "r", json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(report.code.as_str(), "fake.refused");
}

#[tokio::test]
async fn one_delegate_leaves_the_other_operation_unserved() {
    let fake = Arc::new(Fake::default());
    let app = App::with_delegates(Delegates::none().with_function(fake.clone())).await;
    let report = app
        .resources
        .run_report(&ctx(), "r", json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(report.code.as_str(), Code::ReportNotFound.as_str());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn an_unauthenticated_request_never_reaches_a_delegate() {
    let fake = Arc::new(Fake::default());
    let app = App::with_delegates(
        Delegates::none()
            .with_function(fake.clone())
            .with_report(fake.clone()),
    )
    .await;
    let mut anonymous = ctx();
    anonymous.credential = None;
    let function = app
        .resources
        .call_function(&anonymous, "a", "m", "f", json!({}), false)
        .await
        .unwrap_err();
    assert_eq!(function.code.as_str(), Code::AuthRequired.as_str());
    let report = app
        .resources
        .run_report(&anonymous, "r", json!({}), None)
        .await
        .unwrap_err();
    assert_eq!(report.code.as_str(), Code::AuthRequired.as_str());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}
