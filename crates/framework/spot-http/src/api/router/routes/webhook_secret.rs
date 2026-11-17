//! `PUT|GET /api/v1/webhooks/{name}/signing-secret` (`webhook_signing_secret_put` and
//! `webhook_signing_secret_get`, WP-INT-01.T5): set or rotate a webhook's signing secret, and
//! read back its fingerprint. The value is never readable.
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request: the webhook name in the path, and a JSON body that is
//! exactly `{"secret": "<text>"}` or `{"generate": true}` (never both, never anything else), with
//! a supplied secret between 16 and 256 bytes. They never decide who the caller is or whether the
//! caller may write the webhook's secret group: the port does, and the routes pass it no user and
//! no role. A caller the port refuses is answered `403 permission.denied` whether or not the
//! webhook exists.
//!
//! # What a response shows
//!
//! - `PUT` answers `{"data": {"key_version", "fingerprint"}}`, plus `"secret"` **only** when the
//!   server generated the value. A supplied secret is never echoed.
//! - `GET` answers `{"data": {"set", "key_version", "fingerprint"}}`.
//! - Every answer, success or failure, carries `Cache-Control: private, no-store`.
//! - A secret never appears in a path, a query string, an error message or a log line: this file
//!   logs nothing, and an error built here names the rule, never a value.
//!
//! A site with no secrets vault answers `501 service.not_configured`, as the other optional
//! services do.
//!
//! The group is mounted only when the router's [`Ports`](crate::api::Ports) holds the
//! [`WebhookSecretPort`]; with no port, the two operations are unrouted.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value as Json, json};

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::webhook_secret::{
    MAX_SECRET_BYTES, MIN_SECRET_BYTES, SigningSecretIn, SigningSecretInfo, SigningSecretOut,
    SigningSecretText, WebhookSecretError, WebhookSecretPort, codes,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail, take_body};

/// The operation ids this group serves once registered.
pub const SERVES: &[&str] = &["webhook_signing_secret_put", "webhook_signing_secret_get"];

/// The ports this group needs: the signing-secret port, whose adapter decides identity, the
/// right to write the secret group and the audit of the write. A group without it is not mounted.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn WebhookSecretPort>()];
    &KEYS
}

/// The signing-secret routes.
pub fn routes() -> Router<RouteState> {
    Router::new().route(
        "/api/v1/webhooks/{name}/signing-secret",
        get(signing_secret_get).put(signing_secret_put),
    )
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "webhook_secret",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

/// The longest webhook name (the `name` field of the DocType allows 200 characters).
const MAX_NAME_CHARS: usize = 200;

type Handled = Result<Response, Failure>;

/// Every answer is `private, no-store`: a generated secret is in one of them.
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

/// The port and the request context, or the answer to give without them.
struct Call {
    port: std::sync::Arc<dyn WebhookSecretPort>,
    ctx: RequestCtx,
}

fn call(state: &RouteState, site: &SiteContext, headers: &HeaderMap) -> Result<Call, Failure> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn WebhookSecretPort>() else {
        return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
    };
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(fail(ApiError::from_code(Code::AuthRequired), request_id));
    }
    Ok(Call { port, ctx })
}

fn invalid(reason: &'static str, message: &str, request_id: &RequestId) -> Failure {
    fail(
        ApiError::new(Code::ApiInvalidRequest, message).detail("reason", Json::from(reason)),
        request_id,
    )
}

/// A webhook name is free text of the `name` field: non-empty, bounded, no control characters.
/// It is not a storage name, so `path_identifier` does not apply.
fn check_name(name: &str, request_id: &RequestId) -> Result<(), Failure> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(invalid(
            codes::BAD_REQUEST,
            "The webhook name in the path is not a valid name.",
            request_id,
        ))
    }
}

/// The body of a `PUT`: exactly one of `secret` (a string) and `generate` (`true`). It is taken
/// by value so the supplied text moves into its redacted holder instead of being copied. Nothing
/// in an error echoes the body.
fn body_of(body: Json, request_id: &RequestId) -> Result<SigningSecretIn, Failure> {
    let bad = || {
        invalid(
            codes::BAD_REQUEST,
            "The body must be {\"secret\": \"...\"} or {\"generate\": true}.",
            request_id,
        )
    };
    let Json::Object(mut object) = body else {
        return Err(bad());
    };
    if object.len() != 1 {
        return Err(bad());
    }
    if let Some(value) = object.remove("generate") {
        return match value {
            Json::Bool(true) => Ok(SigningSecretIn::Generate),
            _ => Err(bad()),
        };
    }
    let Some(Json::String(text)) = object.remove("secret") else {
        return Err(bad());
    };
    let text = SigningSecretText::new(text);
    if text.len() < MIN_SECRET_BYTES {
        return Err(invalid(
            codes::TOO_SHORT,
            "A signing secret must be at least 16 bytes.",
            request_id,
        ));
    }
    if text.len() > MAX_SECRET_BYTES {
        return Err(invalid(
            codes::TOO_LONG,
            "A signing secret must be at most 256 bytes.",
            request_id,
        ));
    }
    Ok(SigningSecretIn::Supplied(text))
}

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no secrets vault configured, so signing secrets cannot be used.",
        "retryable": false,
        "request_id": request_id.as_str(),
        "details": { "reason": codes::NOT_CONFIGURED },
    }});
    (StatusCode::NOT_IMPLEMENTED, axum::Json(body)).into_response()
}

/// The answer to a port failure. Only the kind is honoured: no text of the port's reaches the
/// caller.
fn port_failure(error: WebhookSecretError, request_id: &RequestId) -> Handled {
    match error {
        WebhookSecretError::NotConfigured => Ok(not_configured(request_id)),
        WebhookSecretError::Refused => Err(fail(
            ApiError::from_code(Code::PermissionDenied)
                .detail("reason", Json::from(codes::REFUSED)),
            request_id,
        )),
        WebhookSecretError::NotFound => Err(fail(
            ApiError::from_code(Code::DocumentNotFound)
                .detail("reason", Json::from(codes::NOT_FOUND)),
            request_id,
        )),
        WebhookSecretError::Api(error) => Err(fail(error, request_id)),
        WebhookSecretError::Unavailable => Err(fail(
            ApiError::from_code(Code::ServerUnavailable)
                .detail("reason", Json::from(codes::UNAVAILABLE)),
            request_id,
        )),
        WebhookSecretError::Failed => {
            Err(fail(ApiError::from_code(Code::HttpInternal), request_id))
        }
    }
}

fn put_json(out: &SigningSecretOut) -> Json {
    let mut map = Map::new();
    map.insert("key_version".to_owned(), Json::from(out.key_version));
    map.insert(
        "fingerprint".to_owned(),
        Json::from(out.fingerprint.clone()),
    );
    // The one place a value leaves: a generated one, in this response only.
    if let Some(generated) = &out.generated {
        map.insert("secret".to_owned(), Json::from(generated.expose()));
    }
    Json::Object(map)
}

fn info_json(info: &SigningSecretInfo) -> Json {
    json!({
        "set": info.set,
        "key_version": info.key_version,
        "fingerprint": info.fingerprint,
    })
}

async fn signing_secret_put(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(name): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        check_name(&name, request_id)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let input = body_of(body, request_id)?;
        match call.port.put(&call.ctx, &name, input).await {
            Ok(out) => Ok((
                StatusCode::OK,
                axum::Json(json!({ "data": put_json(&out) })),
            )
                .into_response()),
            Err(error) => port_failure(error, request_id),
        }
    }
    .await;
    done(result)
}

async fn signing_secret_get(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        check_name(&name, request_id)?;
        match call.port.get(&call.ctx, &name).await {
            Ok(info) => Ok((
                StatusCode::OK,
                axum::Json(json!({ "data": info_json(&info) })),
            )
                .into_response()),
            Err(error) => port_failure(error, request_id),
        }
    }
    .await;
    done(result)
}
