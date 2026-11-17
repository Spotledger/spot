//! The administration half of `sso_routes.rs` (split to stay under the 1,000-line gate): the fake
//! `SsoAdminPort` and the `sso_admin_*` tests. Outside the owned paths of WP-IAM-05.T10: see the
//! report.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use serde_json::{Map, Value as Json, json};
use spot_http::api::Code;
use spot_http::api::ports::sso::{AdminPage, AdminReply, SsoAdminPort};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};

use super::*;

// --- the fake administration port -------------------------------------------------------------

#[derive(Default)]
pub struct FakeAdmin {
    pub calls: Mutex<Vec<String>>,
    pub step_up: Mutex<bool>,
    pub deny_with: Mutex<Option<ApiError>>,
}

impl FakeAdmin {
    fn note(&self, what: &str) {
        self.calls.lock().unwrap().push(what.to_owned());
    }
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn reply(&self) -> Result<AdminReply, ApiError> {
        match self.deny_with.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(AdminReply {
                data: json!({"id": "p1", "version": 3}),
                etag: Some("\"3\"".into()),
                next_cursor: None,
            }),
        }
    }
}

impl SsoAdminPort for FakeAdmin {
    fn step_up_current<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<bool, ApiError>> {
        Box::pin(async move { Ok(*self.step_up.lock().unwrap()) })
    }
    fn list_providers<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        page: &'a AdminPage,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            self.note(&format!("list {:?} {}", page.cursor, page.limit));
            Ok(AdminReply {
                data: json!([]),
                etag: None,
                next_cursor: Some(None),
            })
        })
    }
    fn create_provider<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        key: &'a str,
        _body: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            self.note(&format!("create {key}"));
            self.reply()
        })
    }
    fn get_provider<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            self.note(&format!("get {id}"));
            self.reply()
        })
    }
    fn put_provider<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        id: &'a str,
        if_match: &'a str,
        _body: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            self.note(&format!("put {id} {if_match}"));
            self.reply()
        })
    }
    fn delete_provider<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        id: &'a str,
        _if_match: &'a str,
    ) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move {
            self.note(&format!("delete {id}"));
            Ok(())
        })
    }
    fn test_provider<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            self.note(&format!("test {id}"));
            self.reply()
        })
    }
    fn get_policy<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            self.note("policy_get");
            self.reply()
        })
    }
    fn put_policy<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        _if_match: &'a str,
        _body: &'a Map<String, Json>,
    ) -> BoxFuture<'a, Result<AdminReply, ApiError>> {
        Box::pin(async move {
            self.note("policy_put");
            self.reply()
        })
    }
}

// --- administration ---------------------------------------------------------------------------

fn provider_body() -> String {
    json!({
        "kind": "oidc", "name": "Okta", "audience": ["staff"],
        "oidc": {"issuer": "https://idp.example.com", "client_id": "cid", "scopes": ["openid"]}
    })
    .to_string()
}

#[tokio::test]
async fn a_write_without_a_recent_step_up_is_refused_before_the_operation() {
    let f = fixture();
    *f.admin.step_up.lock().unwrap() = false;
    let h = [BEARER, JSON_CT, ("idempotency-key", "k1")];
    let r = send(
        &f,
        "POST",
        "/api/v1/identity/sso/providers",
        &h,
        &provider_body(),
    )
    .await;
    assert_eq!(r.status, 403);
    assert_eq!(r.code(), "auth.step_up_required");
    let h = [BEARER, JSON_CT, ("if-match", "\"3\"")];
    let r = send(
        &f,
        "PUT",
        "/api/v1/identity/sso/providers/p1",
        &h,
        &provider_body(),
    )
    .await;
    assert_eq!(r.code(), "auth.step_up_required");
    let r = send(
        &f,
        "DELETE",
        "/api/v1/identity/sso/providers/p1",
        &[BEARER, ("if-match", "\"3\"")],
        "",
    )
    .await;
    assert_eq!(r.code(), "auth.step_up_required");
    let r = send(
        &f,
        "POST",
        "/api/v1/identity/sso/providers/p1/test",
        &[BEARER],
        "",
    )
    .await;
    assert_eq!(r.code(), "auth.step_up_required");
    let body = r#"{"mode":"local","break_glass_users":[]}"#;
    let r = send(
        &f,
        "PUT",
        "/api/v1/identity/sso/policy",
        &[BEARER, JSON_CT, ("if-match", "\"1\"")],
        body,
    )
    .await;
    assert_eq!(r.code(), "auth.step_up_required");
    assert!(f.admin.calls().is_empty(), "{:?}", f.admin.calls());
    // Reads need none.
    let r = send(
        &f,
        "GET",
        "/api/v1/identity/sso/providers/p1",
        &[BEARER],
        "",
    )
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn the_administration_operations_are_mounted_and_shaped() {
    let f = fixture();
    let r = send(
        &f,
        "GET",
        "/api/v1/identity/sso/providers?limit=5",
        &[BEARER],
        "",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    assert_eq!(r.json()["next_cursor"], Json::Null);
    let h = [BEARER, JSON_CT, ("idempotency-key", "k1")];
    let r = send(
        &f,
        "POST",
        "/api/v1/identity/sso/providers",
        &h,
        &provider_body(),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.raw);
    assert_eq!(r.headers.get(header::ETAG).unwrap(), "\"3\"");
    assert_sso_headers(&r);
    let h = [BEARER, JSON_CT, ("if-match", "\"3\"")];
    let r = send(
        &f,
        "PUT",
        "/api/v1/identity/sso/providers/p1",
        &h,
        &provider_body(),
    )
    .await;
    assert_eq!(r.status, 200);
    let r = send(
        &f,
        "DELETE",
        "/api/v1/identity/sso/providers/p1",
        &[BEARER, ("if-match", "\"3\"")],
        "",
    )
    .await;
    assert_eq!(r.status, 204);
    let r = send(
        &f,
        "POST",
        "/api/v1/identity/sso/providers/p1/test",
        &[BEARER],
        "",
    )
    .await;
    assert_eq!(r.status, 200);
    let r = send(&f, "GET", "/api/v1/identity/sso/policy", &[BEARER], "").await;
    assert_eq!(r.status, 200);
    let body = r#"{"mode":"sso_optional","break_glass_users":["u1"]}"#;
    let r = send(
        &f,
        "PUT",
        "/api/v1/identity/sso/policy",
        &[BEARER, JSON_CT, ("if-match", "\"1\"")],
        body,
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        f.admin.calls(),
        [
            "list None 5",
            "create k1",
            "put p1 \"3\"",
            "delete p1",
            "test p1",
            "policy_get",
            "policy_put"
        ]
    );
}

#[tokio::test]
async fn administration_inputs_are_bounded_and_unknown_fields_refused() {
    let f = fixture();
    let h = [BEARER, JSON_CT, ("idempotency-key", "k1")];
    // No key.
    let r = send(
        &f,
        "POST",
        "/api/v1/identity/sso/providers",
        &[BEARER, JSON_CT],
        &provider_body(),
    )
    .await;
    assert_eq!(r.code(), "api.invalid_request");
    // Unknown field: only a fixed path goes back, never the key.
    let body = json!({"kind":"oidc","name":"x","audience":[],"zzz-secret":1}).to_string();
    let r = send(&f, "POST", "/api/v1/identity/sso/providers", &h, &body).await;
    assert_eq!(r.status, 422);
    assert_eq!(r.code(), "sso.provider_invalid");
    assert!(!r.raw.contains("zzz-secret"));
    // Name too long.
    let body = json!({"kind":"saml","name":"n".repeat(101),"audience":["staff"]}).to_string();
    let r = send(&f, "POST", "/api/v1/identity/sso/providers", &h, &body).await;
    assert_eq!(r.code(), "sso.provider_invalid");
    // If-Match required and shaped.
    let r = send(
        &f,
        "PUT",
        "/api/v1/identity/sso/providers/p1",
        &[BEARER, JSON_CT],
        &provider_body(),
    )
    .await;
    assert_eq!(r.status, 428);
    let r = send(
        &f,
        "DELETE",
        "/api/v1/identity/sso/providers/p1",
        &[BEARER, ("if-match", "W/\"1\"")],
        "",
    )
    .await;
    assert_eq!(r.status, 428);
    // Page bounds.
    for q in [
        "limit=0",
        "limit=101",
        "limit=x",
        "cursor=",
        "other=1",
        "limit=1&limit=2",
    ] {
        let r = send(
            &f,
            "GET",
            &format!("/api/v1/identity/sso/providers?{q}"),
            &[BEARER],
            "",
        )
        .await;
        assert_eq!(r.status, 400, "{q}");
    }
    // Policy shape.
    let r = send(
        &f,
        "PUT",
        "/api/v1/identity/sso/policy",
        &[BEARER, JSON_CT, ("if-match", "\"1\"")],
        r#"{"mode":"weird","break_glass_users":[]}"#,
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(f.admin.calls().is_empty());
}

#[tokio::test]
async fn a_port_error_is_rendered_only_for_the_codes_of_the_contract() {
    let f = fixture();
    *f.admin.deny_with.lock().unwrap() = Some(
        ApiError::new(Code::SsoWouldLockOut, "would lock out")
            .detail("reason", json!("min_break_glass")),
    );
    let r = send(
        &f,
        "GET",
        "/api/v1/identity/sso/providers/p1",
        &[BEARER],
        "",
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (409, "sso.would_lock_out"));
    *f.admin.deny_with.lock().unwrap() =
        Some(ApiError::new(Code::PrintRenderFailed, "secret detail"));
    let r = send(
        &f,
        "GET",
        "/api/v1/identity/sso/providers/p1",
        &[BEARER],
        "",
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (500, "http.internal"));
    assert!(!r.raw.contains("secret detail"));
}

#[tokio::test]
async fn an_administration_call_without_a_bearer_never_reaches_the_port() {
    let f = fixture();
    let r = send(&f, "GET", "/api/v1/identity/sso/policy", &[], "").await;
    assert_eq!(r.status, 401);
    assert!(f.admin.calls().is_empty());
}
