//! `auth_invitation_accept` (`POST /api/v1/auth/invitations/accept`), WP-IAM-12.T5.
//!
//! The invitation token in the body is the only credential: no bearer is read, no cookie, no
//! query (a query is refused before the body is read, so a token in a URL is never accepted).
//! The body is a closed object `{token, password, full_name?}`; an unknown field is refused and
//! the refusal names none. A success is `200 {"data":{"accepted":true}}`; the person then signs
//! in with `auth_login`.
//!
//! **One answer for every bad token.** An unknown, expired, revoked or used token is the one
//! `400 identity.invitation_invalid`; a password that fails the tenant's policy is `422
//! auth.password_policy` and leaves the token usable. Every answer is `Cache-Control:
//! no-store`. No token and no password is logged or put in an error.
//!
//! **A public group.** The operation has no bearer by contract (`security: []`), so the file
//! declares [`PUBLIC`] (rate class `auth`): the registry mounts it outside the bearer layer,
//! behind the per-client-address rate limit, which runs before this handler reads the body. The
//! router removes any `Authorization` header before the handler runs. The route takes no
//! `Idempotency-Key`: a repeat is a repeat of the action, and the second use of a token is
//! refused.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

use super::{Public, RateClass, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::invitation_accept::{AcceptError, AcceptIn, InvitationAcceptPort};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::render_error;

/// The operation ids this group serves.
pub const SERVES: &[&str] = &["auth_invitation_accept"];

/// The group is public: no bearer, the `auth` rate class.
pub const PUBLIC: Public = Public {
    rate_class: RateClass::Auth,
};

/// The ports this group needs.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn InvitationAcceptPort>()];
    &KEYS
}

/// The acceptance route (state: a [`RouteState`] holding the [`InvitationAcceptPort`]).
pub fn routes() -> Router<RouteState> {
    Router::new().route(
        "/api/v1/auth/invitations/accept",
        post(auth_invitation_accept),
    )
}

/// The largest body (a closed object of three bounded texts).
const MAX_BODY: usize = 8 * 1024;
const TOKEN_MIN: usize = 16;
const TOKEN_MAX: usize = 512;
const PASSWORD_MAX: usize = 1024;
const NAME_MAX: usize = 200;

fn finish(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn failure(error: ApiError, request_id: &RequestId) -> Response {
    finish(render_error(error, request_id))
}

fn bad(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

/// The body as the port's input, or the refusal. Names no field and echoes no value.
async fn read_input(request: Request) -> Result<AcceptIn, ApiError> {
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    if content_type.is_some_and(|ct| ct != "application/json") {
        return Err(ApiError::from_code(Code::ApiUnsupportedMediaType));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| ApiError::from_code(Code::HttpBodyTooLarge))?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::new(
            Code::ApiInvalidJson,
            "The request body must be a JSON document.",
        )
    })?;
    let map = value
        .as_object()
        .ok_or_else(|| bad("The body must be a JSON object."))?;
    if map
        .keys()
        .any(|k| !matches!(k.as_str(), "token" | "password" | "full_name"))
    {
        return Err(bad("The body has a field this operation does not accept."));
    }
    let token = map
        .get("token")
        .and_then(Value::as_str)
        .filter(|t| {
            (TOKEN_MIN..=TOKEN_MAX).contains(&t.len())
                && t.bytes().all(|b| (0x21..=0x7e).contains(&b))
        })
        .ok_or_else(|| bad("`token` is missing or malformed."))?;
    let password = map
        .get("password")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty() && p.chars().count() <= PASSWORD_MAX)
        .ok_or_else(|| bad("`password` is missing or too long."))?;
    let full_name = match map.get("full_name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name))
            if !name.trim().is_empty()
                && name.chars().count() <= NAME_MAX
                && !name.chars().any(char::is_control) =>
        {
            Some(name.trim().to_owned())
        }
        Some(_) => return Err(bad("`full_name` is not a valid name.")),
    };
    Ok(AcceptIn {
        token: token.to_owned(),
        password: password.to_owned(),
        full_name,
    })
}

async fn auth_invitation_accept(State(state): State<RouteState>, request: Request) -> Response {
    let site = request.extensions().get::<SiteContext>().cloned();
    let request_id = &site
        .as_ref()
        .map(|site| site.request_id.clone())
        .or_else(|| request.extensions().get::<RequestId>().cloned())
        .unwrap_or_else(RequestId::generate);
    let Some(site) = site else {
        tracing::error!("missing SiteContext on the invitation acceptance route");
        return failure(ApiError::from_code(Code::HttpInternal), request_id);
    };
    let Some(port) = state.port::<dyn InvitationAcceptPort>() else {
        return failure(ApiError::from_code(Code::HttpInternal), request_id);
    };
    // The call has no query: refuse one before the body is read.
    if request.uri().query().is_some_and(|q| !q.is_empty()) {
        return failure(
            ApiError::new(Code::ApiInvalidRequest, "This call takes no query."),
            request_id,
        );
    }
    let input = match read_input(request).await {
        Ok(input) => input,
        Err(error) => return failure(error, request_id),
    };
    // The bearer is neither needed nor read: the context carries no credential.
    let ctx = RequestCtx {
        site: site.site.clone(),
        credential: None,
        idempotency_key: None,
        language: None,
        request_id: request_id.clone(),
        peer: site.peer,
    };
    match port.accept(&ctx, input).await {
        Ok(()) => finish(
            (
                StatusCode::OK,
                axum::Json(json!({ "data": { "accepted": true } })),
            )
                .into_response(),
        ),
        Err(AcceptError::Invalid) => failure(
            ApiError::from_code(Code::IdentityInvitationInvalid),
            request_id,
        ),
        Err(AcceptError::PasswordPolicy) => {
            failure(ApiError::from_code(Code::AuthPasswordPolicy), request_id)
        }
        Err(AcceptError::Unavailable) => {
            failure(ApiError::from_code(Code::ServerUnavailable), request_id)
        }
    }
}
