//! `POST /api/v1/auth/totp/enrol`, `/confirm`, `/disable` and
//! `POST /api/v1/auth/recovery-codes/regenerate` (WP-PLT-02.T9, contract 0.18.0): a signed-in
//! person's own second factor.
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request: a bearer is present, the JSON body has exactly the
//! contract's fields with the contract's bounds (`code` 6 to 16 bytes on confirm, up to 32 on the
//! others; `password` up to 1,024), and nothing else. A malformed body is `api.invalid_request`
//! and is refused **before** the port is asked, so it never reaches a throttle or a password
//! check. The routes never decide who the caller is, whether the proof is right, or what a
//! change does to other sessions: the port does, and the routes pass it no user id.
//!
//! Nothing in an error echoes the body. Every response is `private, no-store` (with `Pragma:
//! no-cache`): the provisioning secret and the recovery codes are in them, once. These
//! operations take no `Idempotency-Key` and the router does not store their responses, so no
//! replay cache ever holds a secret.
//!
//! # Unserved
//!
//! The group is mounted only with its port ([`totp_required_ports`]), and the host provides the
//! port only when every collaborator it needs (the sign-in service, its throttle and its audit
//! sink) is present, never half wired. Without the port the four operations answer as unserved
//! operations do.
//!
//! **Not registered in the route registry yet,** like the session group: registering it would put
//! its operations into the served set of the conformance walk (`tests/conformance.rs`, not owned
//! by this task). The host mounts it through [`registration`]; to register it, rename the
//! `TOTP_*` items to `SERVES`, `routes` and `required_ports` and add the walk cases.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Map, Value as Json, json};

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::totp::{CodesOut, TotpPort};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail, take_body};

/// The operation ids this group serves once registered.
pub const TOTP_SERVES: &[&str] = &[
    "auth_totp_enrol",
    "auth_totp_confirm",
    "auth_totp_disable",
    "auth_recovery_codes_regenerate",
];

/// The longest password the contract allows.
const PASSWORD_MAX: usize = 1024;
/// The bounds of a confirm code.
const CONFIRM_CODE: (usize, usize) = (6, 16);
/// The bounds of a disable or regenerate code (a TOTP code or a recovery code).
const PROOF_CODE: (usize, usize) = (1, 32);

/// The ports this group needs: the second-factor port, whose adapter decides identity, proof,
/// throttle, session effects and audit. A group without it is not mounted at all.
pub fn totp_required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn TotpPort>()];
    &KEYS
}

/// The second-factor routes.
pub fn totp_routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/auth/totp/enrol", post(auth_totp_enrol))
        .route("/api/v1/auth/totp/confirm", post(auth_totp_confirm))
        .route("/api/v1/auth/totp/disable", post(auth_totp_disable))
        .route(
            "/api/v1/auth/recovery-codes/regenerate",
            post(auth_recovery_codes_regenerate),
        )
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "totp",
        serves: TOTP_SERVES,
        required_ports: totp_required_ports,
        routes: totp_routes,
        public: None,
    }
}

type Handled = Result<Response, Failure>;

/// Every answer is `private, no-store`: a secret or a set of codes is in one of them.
fn done(result: Handled) -> Response {
    let mut response = match result {
        Ok(response) => response,
        Err(failure) => failure.into_response(),
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

struct Call {
    port: std::sync::Arc<dyn TotpPort>,
    ctx: RequestCtx,
}

fn call(state: &RouteState, site: &SiteContext, headers: &HeaderMap) -> Result<Call, Failure> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn TotpPort>() else {
        return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
    };
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(fail(ApiError::from_code(Code::AuthRequired), request_id));
    }
    Ok(Call { port, ctx })
}

fn invalid(message: &str, request_id: &RequestId) -> Failure {
    fail(ApiError::new(Code::ApiInvalidRequest, message), request_id)
}

/// A string field of an object, bounded in bytes. Never echoes the value.
fn text_field(
    object: &mut Map<String, Json>,
    name: &str,
    bounds: (usize, usize),
    request_id: &RequestId,
) -> Result<String, Failure> {
    match object.remove(name) {
        Some(Json::String(text)) if text.len() >= bounds.0 && text.len() <= bounds.1 => Ok(text),
        _ => Err(invalid(
            "The body does not match the contract: a required field is missing or out of bounds.",
            request_id,
        )),
    }
}

/// The body as an object holding exactly the contract's fields.
fn exact_object(
    body: Json,
    fields: usize,
    request_id: &RequestId,
) -> Result<Map<String, Json>, Failure> {
    match body {
        Json::Object(object) if object.len() == fields => Ok(object),
        _ => Err(invalid(
            "The body does not match the contract: it holds missing or unknown fields.",
            request_id,
        )),
    }
}

fn codes_json(out: &CodesOut) -> Json {
    json!({ "data": { "recovery_codes": out.recovery_codes } })
}

async fn auth_totp_enrol(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let out = call
            .port
            .enrol(&call.ctx)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok((
            StatusCode::OK,
            axum::Json(json!({
                "data": { "secret": out.secret, "provisioning_uri": out.provisioning_uri }
            })),
        )
            .into_response())
    }
    .await;
    done(result)
}

async fn auth_totp_confirm(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let mut object = exact_object(body, 1, request_id)?;
        let code = text_field(&mut object, "code", CONFIRM_CODE, request_id)?;
        let out = call
            .port
            .confirm(&call.ctx, &code)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok((StatusCode::OK, axum::Json(codes_json(&out))).into_response())
    }
    .await;
    done(result)
}

/// The body of disable and regenerate: the password and a code.
fn proof_of(body: Json, request_id: &RequestId) -> Result<(String, String), Failure> {
    let mut object = exact_object(body, 2, request_id)?;
    let password = text_field(&mut object, "password", (1, PASSWORD_MAX), request_id)?;
    let code = text_field(&mut object, "code", PROOF_CODE, request_id)?;
    Ok((password, code))
}

async fn auth_totp_disable(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let (password, code) = proof_of(body, request_id)?;
        call.port
            .disable(&call.ctx, &password, &code)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    done(result)
}

async fn auth_recovery_codes_regenerate(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let (password, code) = proof_of(body, request_id)?;
        let out = call
            .port
            .regenerate(&call.ctx, &password, &code)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok((StatusCode::OK, axum::Json(codes_json(&out))).into_response())
    }
    .await;
    done(result)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn id() -> RequestId {
        RequestId::generate()
    }

    #[test]
    fn a_proof_body_has_exactly_a_password_and_a_code() {
        let ok = proof_of(json!({ "password": "pw", "code": "123456" }), &id());
        assert_eq!(ok.ok(), Some(("pw".to_owned(), "123456".to_owned())));
        for bad in [
            json!({ "password": "pw" }),
            json!({ "password": "pw", "code": "123456", "extra": 1 }),
            json!({ "password": "", "code": "123456" }),
            json!({ "password": "pw", "code": "" }),
            json!({ "password": "pw", "code": "x".repeat(33) }),
            json!({ "password": "x".repeat(1025), "code": "123456" }),
            json!({ "password": 1, "code": "123456" }),
            json!([]),
            json!(null),
        ] {
            assert!(proof_of(bad, &id()).is_err());
        }
    }

    #[test]
    fn a_confirm_code_is_six_to_sixteen_bytes() {
        let take = |code: &str| {
            let mut object = exact_object(json!({ "code": code }), 1, &id())
                .ok()
                .unwrap_or_default();
            text_field(&mut object, "code", CONFIRM_CODE, &id())
        };
        assert!(take("123456").is_ok());
        assert!(take(&"1".repeat(16)).is_ok());
        assert!(take("12345").is_err());
        assert!(take(&"1".repeat(17)).is_err());
    }
}
