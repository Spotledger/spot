//! `process_webhook` (`POST /api/v1/process/hook/{trigger}`), native API 0.12.0, WP-BPM-02.T5:
//! the public receiver of a process's incoming webhook. The other six operations of the process
//! group are in `routes/process.rs`.
//!
//! **A public group, and the only public route of the process and task groups.** The operation
//! has no bearer by contract (`security: []`), so the file declares [`PUBLIC`] (rate class
//! `auth`, per client address): the registry mounts it outside the bearer layer, behind the rate
//! limit, which runs before this handler. The router removes any `Authorization` header before
//! the handler runs, and the handler builds a request context with **no credential**.
//!
//! # What the route does
//!
//! 1. no [`WebhookPort`] attached: `501 service.not_configured`;
//! 2. `Content-Type` must be `application/json` (parameters such as a charset are allowed),
//!    else `415 api.unsupported_media_type`;
//! 3. the body is read with a hard cap of 256 KiB **while it streams**: a declared length over
//!    the cap, or a body that passes the cap (an endless one included), is `413
//!    http.body_too_large` before the port is called, and the read stops at the cap;
//! 4. everything else goes to the port: the trigger, the delivery id, the signature header and
//!    the raw bytes. The port verifies the signature **before** it parses anything; **the route
//!    never parses the body**, so a body whose signature has not verified produces no JSON error;
//! 5. `202 {"accepted": true}` on success, and `401 process.webhook_rejected` for every
//!    rejection: the same status, the same body (but the request id) and the same headers.
//!
//! # Why a malformed request is a `401` too
//!
//! A trigger that is not in the documented form, an `Idempotency-Key` that is not in the
//! documented form or given twice, and a missing, repeated or unusable signature header are
//! all answered `401 process.webhook_rejected`, like an unknown trigger or a bad signature: a
//! `400` would tell an unauthenticated caller which of its inputs was checked first. A missing or
//! unusable signature still goes to the port (as an empty one), so the route takes no branch of
//! its own on it; the port takes one path for every cause.
//!
//! The route never echoes the body, the signature or the trigger, and logs a refusal with the
//! operation id and the problem code only.
//!
//! # The host must exempt this path from its own authentication
//!
//! The route is public by the contract. The spot-cli host (not owned by this task) dispatches
//! requests with its own authentication in front of the router: it must let exactly
//! `POST /api/v1/process/hook/{trigger}` through without a bearer, behind the per-address rate
//! limit this group declares, and nothing else. The registry's `build.rs` already checks that
//! this file mounts exactly that one route.

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::json;

use super::{Public, RateClass, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::webhook::WebhookPort;
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::render_error;

/// The operation ids this group serves.
pub const SERVES: &[&str] = &["process_webhook"];

/// The group is public: no bearer, the `auth` rate class.
pub const PUBLIC: Public = Public {
    rate_class: RateClass::Auth,
};

/// The ports this group needs: the webhook receiver, whose adapter verifies the signature,
/// finds the trigger on the site and audits every delivery.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn WebhookPort>()];
    &KEYS
}

/// The webhook route (state: a [`RouteState`] holding the [`WebhookPort`]).
pub fn routes() -> Router<RouteState> {
    Router::new().route("/api/v1/process/hook/{trigger}", post(process_webhook))
}

/// The hard cap of a delivery's body: 256 KiB.
pub const MAX_BODY_BYTES: usize = 256 * 1024;

/// The longest signature header the contract allows.
const MAX_SIGNATURE_LEN: usize = 512;

/// The operation id, for the refusal log.
const OPERATION: &str = "process_webhook";

/// The trigger's documented form: `^[a-z0-9][a-z0-9_-]{0,63}$`.
fn is_trigger(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && bytes
            .iter()
            .skip(1)
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// The delivery id's documented form: 1 to 128 visible ASCII characters.
fn is_delivery_id(raw: &str) -> bool {
    (1..=128).contains(&raw.len()) && raw.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Every answer of the route is private to the caller and never cached.
fn finish(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A refusal: logged with the operation id and the problem code only, never a value.
fn refuse(error: ApiError, request_id: &RequestId) -> Response {
    tracing::info!(
        operation = OPERATION,
        code = error.code.as_str(),
        "request refused"
    );
    finish(render_error(error, request_id))
}

/// The one rejection: the same for every cause, with no detail.
fn rejected(request_id: &RequestId) -> Response {
    refuse(
        ApiError::from_code(Code::ProcessWebhookRejected),
        request_id,
    )
}

/// `501 service.not_configured`: the site has no webhook receiver.
fn not_configured(request_id: &RequestId) -> Response {
    tracing::info!(
        operation = OPERATION,
        code = "service.not_configured",
        "request refused"
    );
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no process service configured.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = Response::new(axum::body::Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    finish(response)
}

/// Whether the request declares `application/json` (any parameters are ignored).
fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value
        .to_str()
        .ok()
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
}

/// The signature header: exactly one usable value, else the empty string (the port rejects an
/// empty signature like any other bad one, so the route takes no branch of its own on it).
fn signature_of(headers: &HeaderMap) -> String {
    let mut values = headers.get_all("x-spot-signature").iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return String::new();
    };
    match value.to_str() {
        Ok(text) if !text.is_empty() && text.len() <= MAX_SIGNATURE_LEN => text.to_owned(),
        _ => String::new(),
    }
}

/// The delivery id: `Ok(None)` when none was sent, `Err(())` for one given twice or malformed.
fn delivery_of(headers: &HeaderMap) -> Result<Option<String>, ()> {
    let mut values = headers.get_all("idempotency-key").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    match value.to_str() {
        Ok(text) if is_delivery_id(text) => Ok(Some(text.to_owned())),
        _ => Err(()),
    }
}

async fn process_webhook(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(trigger): Path<String>,
    request: Request,
) -> Response {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn WebhookPort>() else {
        return not_configured(request_id);
    };
    if !is_json(&headers) {
        return refuse(
            ApiError::from_code(Code::ApiUnsupportedMediaType),
            request_id,
        );
    }
    let too_large = || refuse(ApiError::from_code(Code::HttpBodyTooLarge), request_id);
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_BODY_BYTES) {
        return too_large();
    }
    // The read stops at the cap, so an endless body is never buffered past it.
    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return too_large();
    };
    if !is_trigger(&trigger) {
        return rejected(request_id);
    }
    let Ok(delivery) = delivery_of(&headers) else {
        return rejected(request_id);
    };
    let signature = signature_of(&headers);
    // No credential: the route is public, and the router removed any bearer.
    let ctx = RequestCtx {
        site: site.site.clone(),
        credential: None,
        idempotency_key: delivery.clone(),
        language: None,
        request_id: request_id.clone(),
        peer: site.peer,
    };
    match port
        .receive(&ctx, &trigger, delivery.as_deref(), &signature, body)
        .await
    {
        Ok(()) => finish(
            (
                StatusCode::ACCEPTED,
                axum::Json(json!({ "accepted": true })),
            )
                .into_response(),
        ),
        Err(_) => rejected(request_id),
    }
}
