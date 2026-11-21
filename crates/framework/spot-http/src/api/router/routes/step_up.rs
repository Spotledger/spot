//! `POST /api/v1/auth/step-up` (`auth_step_up`, WP-IAM-12.T7, ADR-070 section 3): a signed-in
//! person re-authenticates so that the operations that need a recent step-up
//! (`403 auth.step_up_required`) are accepted for a short while.
//!
//! # What the route decides, and what it does not
//!
//! The route reads the shape of a request: a bearer is present and the JSON body is an object
//! holding only `password` (a string of 1 to 1,024 bytes), `totp` (1 to 16 bytes) and `sso` (a
//! boolean), with at most one proof family per call: a `password` with an optional `totp`, **or**
//! `sso: true` alone. Unknown fields, wrong types, an empty object, a `totp` without a
//! `password` and a mixed family are `400 api.invalid_request`, refused **before** the port is
//! asked, so a malformed call never reaches the throttle or a password check. The error never
//! echoes the body.
//!
//! The route never decides who the caller is, that the caller may step up (a staff or partner
//! person, never a consumer and never an API key), whether the proof is right, how often a wrong
//! proof may be tried, what is recorded on the session or what is audited: [`StepUpPort`] does,
//! and the route passes it no user id. A wrong credential is the port's `401
//! auth.invalid_credentials` (the same body whether the account has a second factor or not), a
//! missing code is `401 auth.totp_required`, a throttled call is `429 api.rate_limited` with its
//! `Retry-After`: all of them pass through with their codes. A branch the site cannot serve (the
//! port's `NotConfigured`) is `501 service.not_configured`.
//!
//! # The answer
//!
//! `200` `{"data": {"step_up_until": <RFC 3339 or null>, "redirect_url": <string or null>}}`
//! with `Cache-Control: no-store`. The operation takes no `Idempotency-Key`: a repeat is a repeat
//! of the action, the router never stores or replays its answer, and no password, code or
//! redirect is ever put in a log line, a metric label or an error.
//!
//! # Unserved
//!
//! The group is mounted only with its port; the host provides the port only when every
//! collaborator it needs is present, so a site without it answers as for an unserved operation.
//! The audience is the table's: staff and partner (`auth_step_up` in `routes.rs`); a consumer
//! credential is refused by the bearer layer with `403` before this handler runs.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Map, Value as Json, json};

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::step_up::{StepUpError, StepUpPort, StepUpProof};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail, take_body};

/// The operation ids this group serves.
pub const SERVES: &[&str] = &["auth_step_up"];

/// The longest password the contract allows.
const PASSWORD_MAX: usize = 1024;
/// The longest second-factor code the contract allows.
const TOTP_MAX: usize = 16;

/// The ports this group needs: the step-up port, whose adapter decides identity, audience, proof,
/// throttle, marker and audit. A group without it is not mounted at all.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn StepUpPort>()];
    &KEYS
}

/// The step-up route.
pub fn routes() -> Router<RouteState> {
    Router::new().route("/api/v1/auth/step-up", post(auth_step_up))
}

/// The group as the registry's mount function takes it, for a test that builds the group by hand.
pub fn registration() -> Registration {
    Registration {
        name: "step_up",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

type Handled = Result<Response, Failure>;

/// Every answer, success or refusal, is `no-store`.
fn done(result: Handled) -> Response {
    let mut response = match result {
        Ok(response) => response,
        Err(failure) => failure.into_response(),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// `501 service.not_configured`: not a registry code, so rendered here. Names no branch, no user
/// and no value.
fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site does not serve this kind of step-up.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = (StatusCode::NOT_IMPLEMENTED, axum::Json(body)).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn invalid(request_id: &RequestId) -> Failure {
    fail(
        ApiError::new(
            Code::ApiInvalidRequest,
            "The body does not match the contract: send a password (and a code when you have a \
             second factor), or sso: true, and nothing else.",
        ),
        request_id,
    )
}

/// An optional string field of an object, bounded in bytes. Absent is `None`; present with the
/// wrong type or out of bounds is an error. Never echoes the value.
fn optional_text(
    object: &mut Map<String, Json>,
    name: &str,
    max: usize,
    request_id: &RequestId,
) -> Result<Option<String>, Failure> {
    match object.remove(name) {
        None => Ok(None),
        Some(Json::String(text)) if !text.is_empty() && text.len() <= max => Ok(Some(text)),
        Some(_) => Err(invalid(request_id)),
    }
}

/// The body as one proof family: a `password` with an optional `totp`, or `sso: true` alone.
fn proof_of(body: Json, request_id: &RequestId) -> Result<StepUpProof, Failure> {
    let Json::Object(mut object) = body else {
        return Err(invalid(request_id));
    };
    if object.is_empty()
        || object
            .keys()
            .any(|key| !matches!(key.as_str(), "password" | "totp" | "sso"))
    {
        return Err(invalid(request_id));
    }
    let password = optional_text(&mut object, "password", PASSWORD_MAX, request_id)?;
    let totp = optional_text(&mut object, "totp", TOTP_MAX, request_id)?;
    let sso = match object.remove("sso") {
        None => false,
        Some(Json::Bool(flag)) => flag,
        Some(_) => return Err(invalid(request_id)),
    };
    match (password, totp, sso) {
        // The password family: a code only goes with a password.
        (Some(password), totp, false) => Ok(StepUpProof {
            password: Some(password),
            totp,
            sso: false,
        }),
        // The single sign-on family stands alone.
        (None, None, true) => Ok(StepUpProof {
            password: None,
            totp: None,
            sso: true,
        }),
        // A code without a password, a mixed family, or `{"sso": false}` alone.
        _ => Err(invalid(request_id)),
    }
}

async fn auth_step_up(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let Some(port) = state.port::<dyn StepUpPort>() else {
            return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
        };
        let ctx: RequestCtx = build_ctx(&site, &headers, None);
        if ctx.credential.is_none() {
            return Err(fail(ApiError::from_code(Code::AuthRequired), request_id));
        }
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let proof = proof_of(body, request_id)?;
        let out = match port.step_up(&ctx, proof).await {
            Ok(out) => out,
            Err(StepUpError::Api(error)) => return Err(fail(error, request_id)),
            Err(StepUpError::NotConfigured) => return Ok(not_configured(request_id)),
        };
        Ok((
            StatusCode::OK,
            axum::Json(json!({
                "data": {
                    "step_up_until": out.step_up_until,
                    "redirect_url": out.redirect_url,
                }
            })),
        )
            .into_response())
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

    fn proof(body: Json) -> Option<StepUpProof> {
        proof_of(body, &id()).ok()
    }

    #[test]
    fn a_password_with_an_optional_code_is_the_password_family() {
        assert_eq!(
            proof(json!({ "password": "pw" })),
            Some(StepUpProof {
                password: Some("pw".to_owned()),
                totp: None,
                sso: false
            })
        );
        assert_eq!(
            proof(json!({ "password": "pw", "totp": "123456" })),
            Some(StepUpProof {
                password: Some("pw".to_owned()),
                totp: Some("123456".to_owned()),
                sso: false
            })
        );
        // `sso: false` next to a password is the same family.
        assert!(proof(json!({ "password": "pw", "sso": false })).is_some());
    }

    #[test]
    fn sso_true_stands_alone() {
        assert_eq!(
            proof(json!({ "sso": true })),
            Some(StepUpProof {
                password: None,
                totp: None,
                sso: true
            })
        );
        assert!(proof(json!({ "sso": true, "password": "pw" })).is_none());
        assert!(proof(json!({ "sso": true, "totp": "123456" })).is_none());
    }

    #[test]
    fn everything_else_is_refused() {
        for bad in [
            json!({}),
            json!([]),
            json!(null),
            json!("pw"),
            json!({ "sso": false }),
            json!({ "totp": "123456" }),
            json!({ "password": "" }),
            json!({ "password": "pw", "totp": "" }),
            json!({ "password": "x".repeat(1025) }),
            json!({ "password": "pw", "totp": "1".repeat(17) }),
            json!({ "password": 1 }),
            json!({ "password": null }),
            json!({ "password": "pw", "sso": "yes" }),
            json!({ "password": "pw", "user": "ada" }),
            json!({ "password": "pw", "extra": 1 }),
        ] {
            assert!(proof(bad.clone()).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_bounds_are_inclusive() {
        assert!(proof(json!({ "password": "x".repeat(1024) })).is_some());
        assert!(proof(json!({ "password": "pw", "totp": "1".repeat(16) })).is_some());
    }
}
