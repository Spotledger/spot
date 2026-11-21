//! The single sign-on routes (contract 0.17.0, WP-CON-03.T40; WP-IAM-05.T10): the public
//! sign-in group over [`SsoPort`]; the administration group lives in `sso_admin.rs`.
//!
//! # What the layer decides, and what it does not
//!
//! `spot-http` owns routing, cookies, request limits, rate classes and the error envelope. It
//! decides nothing about identity: state, nonce, PKCE, signatures, audiences, clocks, the
//! sign-in policy and the session are the port's (the host implements it, WP-IAM-05.T11).
//!
//! - **Strict parsing.** Form bodies are capped at 64 KiB, `Content-Type` is checked, a
//!   duplicate or unknown parameter is refused, nothing is read from a query or form beyond the
//!   contract's names. A request that is not shaped as the contract says is refused before the
//!   port is called.
//! - **Cookies.** `begin` sets the binding cookie (`__Host-spot_sso_binding`, `HttpOnly`,
//!   `Secure`, `SameSite=Lax`, path `/`, 10 minutes). The callback and the assertion consumer
//!   require it (a request without exactly one is `400 sso.state_invalid` **before the port is
//!   called**) and clear it on every outcome. A successful callback sets the one-time hand-over
//!   cookie (`__Host-spot_sso_handover`, 60 s) and `sso_complete` is the only place the session
//!   token appears: in a JSON body, never in a URL, never in a `Set-Cookie`. The binding cookie
//!   is not the session (no fixation).
//! - **Redirects.** Only URLs the port returned, and only after the layer checked their shape:
//!   a relative path of this site for the callback (never built from request input), an `https`
//!   URL for the provider. A `return_to` that is not a relative path is refused at `begin`
//!   (`400 sso.request_invalid`) and never reaches a `Location`.
//! - **Headers.** Every response of these routes carries `Cache-Control: no-store` and
//!   `Referrer-Policy: no-referrer`.
//! - **Errors.** A refusal is a stable `sso.*` code with the registry's generic message; the
//!   response never says which of state, nonce, signature, audience or clock failed (the port
//!   returns a closed [`SsoFailure`], the layer maps it). No query string, form body, cookie
//!   or token is logged or put in an error.
//!
//! # Administration
//!
//! `sso_admin_*` call the port with the authenticated caller (the layer passes no actor,
//! tenant or role) and, before every write, ask `SsoAdminPort::step_up_current`: no recent
//! step-up is `403 auth.step_up_required` and the operation is not called. The port checks the
//! Tenant Administrator permission, scopes to the tenant and audits every outcome.
//!
//! # Registration
//!
//! This file is a registry route file: `SERVES`, `routes()`, `required_ports()` and `PUBLIC` (rate
//! class `auth`, per client address, the strictest class: the contract's `interactive`
//! operations are held to it too). Every operation here is `security: []` in the contract; the
//! authenticated administration group is `sso_admin.rs` (the build script refuses a file that
//! mixes the two).

use std::collections::BTreeMap;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

use super::{Public, RateClass, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::sso::{
    AcsParams, CallbackParams, SloBinding, SloOutcome, SloParams, SsoFailure, SsoPort,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx, Session};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::render_error;

/// The public operation ids.
pub const SERVES: &[&str] = &[
    "sso_provider_list",
    "sso_begin",
    "sso_callback",
    "sso_acs",
    "sso_complete",
    "sso_backchannel_logout",
    "sso_slo_get",
    "sso_slo_post",
    "sso_saml_metadata",
];

/// The ports the public group needs. A group without its port is not mounted at all, so the
/// public sign-in answers exactly as an unserved operation does.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn SsoPort>()];
    &KEYS
}

/// The public sign-in routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/auth/sso/providers", get(sso_provider_list))
        .route("/api/v1/auth/sso/{provider}/begin", post(sso_begin))
        .route("/api/v1/auth/sso/{provider}/callback", get(sso_callback))
        .route("/api/v1/auth/sso/{provider}/acs", post(sso_acs))
        .route("/api/v1/auth/sso/complete", post(sso_complete))
        .route(
            "/api/v1/auth/sso/{provider}/backchannel-logout",
            post(sso_backchannel_logout),
        )
        .route(
            "/api/v1/auth/sso/{provider}/slo",
            get(sso_slo_get).post(sso_slo_post),
        )
        .route(
            "/api/v1/auth/sso/{provider}/saml-metadata",
            get(sso_saml_metadata),
        )
}

/// The group is public: no bearer, the `auth` rate class per client address.
pub const PUBLIC: Public = Public {
    rate_class: RateClass::Auth,
};

// --- constants --------------------------------------------------------------------------------

/// The binding cookie set by `begin` and required by the callback and the assertion consumer.
pub const BINDING_COOKIE: &str = "__Host-spot_sso_binding";
/// The one-time hand-over cookie `sso_complete` exchanges for the session.
pub const HANDOVER_COOKIE: &str = "__Host-spot_sso_handover";
/// The binding cookie's lifetime in seconds.
pub const BINDING_MAX_AGE_SECS: u32 = 600;
/// The hand-over cookie's lifetime in seconds (the contract's 60).
pub const HANDOVER_MAX_AGE_SECS: u32 = 60;
/// The most a form or JSON body of these routes may carry.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_COOKIE_VALUE: usize = 256;
const MAX_PROVIDER_ID: usize = 200;
const MAX_URL: usize = 4096;
const MAX_LOCATION: usize = 2048;
const MAX_METADATA: usize = 262_144;
const MAX_FORM_HTML: usize = 65_536;

// --- helpers ----------------------------------------------------------------------------------

pub(super) fn finish(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

pub(super) fn failure(error: ApiError, request_id: &RequestId) -> Response {
    finish(render_error(error, request_id))
}

fn sso_failure(failure_kind: SsoFailure, request_id: &RequestId) -> Response {
    let code = match failure_kind {
        SsoFailure::SignInFailed => Code::SsoSignInFailed,
        SsoFailure::NotAllowed => Code::SsoNotAllowed,
        SsoFailure::Required => Code::SsoRequired,
        SsoFailure::StateInvalid => Code::SsoStateInvalid,
        SsoFailure::RequestInvalid => Code::SsoRequestInvalid,
        SsoFailure::ProviderNotFound => Code::SsoProviderNotFound,
        SsoFailure::ProviderUnavailable => Code::SsoProviderUnavailable,
        SsoFailure::LogoutRejected => Code::SsoLogoutRejected,
        SsoFailure::Unavailable => Code::ServerUnavailable,
    };
    // The registry's generic message always: never the reason.
    failure(ApiError::from_code(code), request_id)
}

pub(super) fn code_failure(code: Code, request_id: &RequestId) -> Response {
    failure(ApiError::from_code(code), request_id)
}

pub(super) fn internal(request_id: &RequestId, what: &str) -> Response {
    tracing::error!("{what}");
    code_failure(Code::HttpInternal, request_id)
}

/// The request id the answer is reported under.
pub(super) fn request_id_of(site: &SiteContext) -> &RequestId {
    &site.request_id
}

/// A context with no credential: a sign-in has no caller yet, and any `Authorization` header is
/// not looked at.
fn public_ctx(site: &SiteContext) -> RequestCtx {
    RequestCtx {
        site: site.site.clone(),
        credential: None,
        idempotency_key: None,
        language: None,
        request_id: site.request_id.clone(),
        peer: site.peer,
    }
}

/// A provider id from the path: 1 to 200 visible ASCII characters. Anything else is the same
/// `404 sso.provider_not_found` as an unknown provider.
pub(super) fn valid_provider(id: &str) -> bool {
    (1..=MAX_PROVIDER_ID).contains(&id.len()) && id.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Whether `value` is a cookie value the layer will put in a header: non-empty cookie-octets.
fn cookie_safe(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_COOKIE_VALUE
        && value
            .bytes()
            .all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b'"' | b',' | b';' | b'\\'))
}

/// The value of cookie `name`, only when it is sent exactly once and is well formed.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found: Option<&str> = None;
    for header_value in headers.get_all(header::COOKIE) {
        let Ok(text) = header_value.to_str() else {
            continue;
        };
        for pair in text.split(';') {
            let Some((cookie_name, value)) = pair.trim().split_once('=') else {
                continue;
            };
            if cookie_name == name {
                if found.is_some() {
                    return None;
                }
                found = Some(value);
            }
        }
    }
    found.filter(|v| cookie_safe(v)).map(str::to_owned)
}

fn set_cookie(name: &str, value: &str, max_age: u32) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!(
        "{name}={value}; Max-Age={max_age}; Path=/; Secure; HttpOnly; SameSite=Lax"
    ))
    .ok()
}

fn clear_cookie(name: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// A relative path of this site: one leading `/` not followed by `/` or `\`, visible ASCII, at
/// most 2048 characters. `/` alone is allowed where the port returns a location (the default).
fn relative_path(value: &str, allow_root: bool) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_LOCATION || bytes[0] != b'/' {
        return false;
    }
    if bytes.len() == 1 {
        return allow_root;
    }
    !matches!(bytes[1], b'/' | b'\\') && bytes.iter().all(|b| (0x21..=0x7e).contains(b))
}

/// An absolute `https` URL of visible ASCII, at most `max` characters (the provider URLs).
fn https_url(value: &str, max: usize) -> bool {
    value.len() <= max
        && value.len() > "https://".len()
        && value.starts_with("https://")
        && value.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = raw.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Parses `a=b&c=d` into a map. Refused (`None`): a parameter not in `allowed`, one given twice,
/// one that does not decode, or a value longer than its bound in `bounds` (same order as
/// `allowed`).
pub(super) fn parse_fields(
    raw: &str,
    allowed: &[(&str, usize)],
) -> Option<BTreeMap<String, String>> {
    let mut out: BTreeMap<String, String> = BTreeMap::new();
    for pair in raw.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let key = percent_decode(key)?;
        let bound = allowed.iter().find(|(n, _)| *n == key)?.1;
        let value = percent_decode(value)?;
        if value.len() > bound || out.insert(key, value).is_some() {
            return None;
        }
    }
    Some(out)
}

/// The media type of `Content-Type` (lower case, parameters dropped); `None` when absent or
/// sent more than once.
pub(super) fn media_type(headers: &HeaderMap) -> Option<String> {
    let mut all = headers.get_all(header::CONTENT_TYPE).iter();
    let first = all.next()?;
    if all.next().is_some() {
        return None;
    }
    let text = first.to_str().ok()?;
    Some(
        text.split(';')
            .next()
            .unwrap_or(text)
            .trim()
            .to_ascii_lowercase(),
    )
}

/// The body of `request`, at most `limit` bytes. Too large is `413 http.body_too_large`.
pub(super) async fn read_body(
    request: Request,
    limit: usize,
) -> Result<(HeaderMap, axum::body::Bytes), ApiError> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| ApiError::from_code(Code::HttpBodyTooLarge))?;
    Ok((parts.headers, bytes))
}

/// A form body (`application/x-www-form-urlencoded`) as fields, or the failure to answer with.
async fn read_form(
    request: Request,
    allowed: &[(&str, usize)],
) -> Result<BTreeMap<String, String>, ApiError> {
    let (headers, bytes) = read_body(request, MAX_BODY_BYTES).await?;
    if media_type(&headers).as_deref() != Some("application/x-www-form-urlencoded") {
        return Err(ApiError::from_code(Code::ApiUnsupportedMediaType));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| ApiError::from_code(Code::SsoRequestInvalid))?;
    parse_fields(text, allowed).ok_or_else(|| ApiError::from_code(Code::SsoRequestInvalid))
}

fn handover_response(location: &str, cookie: &str, request_id: &RequestId) -> Response {
    if !relative_path(location, true) || !cookie_safe(cookie) {
        return internal(
            request_id,
            "the single sign-on port returned a location or cookie outside the contract",
        );
    }
    let Some(set) = set_cookie(HANDOVER_COOKIE, cookie, HANDOVER_MAX_AGE_SECS) else {
        return internal(request_id, "the hand-over cookie could not be built");
    };
    let Ok(location) = HeaderValue::from_str(location) else {
        return internal(request_id, "the location could not be built");
    };
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    headers.insert(header::LOCATION, location);
    headers.append(header::SET_COOKIE, set);
    headers.append(header::SET_COOKIE, clear_cookie(BINDING_COOKIE));
    finish(response)
}

/// Adds the cookie that clears the (single use) binding cookie.
fn with_binding_cleared(mut response: Response) -> Response {
    response
        .headers_mut()
        .append(header::SET_COOKIE, clear_cookie(BINDING_COOKIE));
    response
}

fn session_body(session: &Session) -> Value {
    json!({
        "data": {
            "token": session.token,
            "expires_at": session.expires_at,
            "user": {
                "id": session.user.id.to_string(),
                "email": session.user.email,
                "full_name": session.user.full_name,
            },
        }
    })
}

// --- public handlers --------------------------------------------------------------------------

async fn sso_provider_list(
    State(state): State<RouteState>,
    site: SiteContext,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if request.uri().query().is_some_and(|q| !q.is_empty()) {
        return failure(
            ApiError::new(Code::ApiInvalidRequest, "This call takes no query."),
            request_id,
        );
    }
    let list = match port.providers(&public_ctx(&site)).await {
        Ok(list) => list,
        Err(kind) => return sso_failure(kind, request_id),
    };
    let ok = list.providers.len() <= 20
        && list.providers.iter().all(|p| {
            (1..=64).contains(&p.id.len())
                && (1..=100).contains(&p.name.chars().count())
                && matches!(p.kind.as_str(), "oidc" | "saml")
        });
    if !ok {
        return internal(
            request_id,
            "the single sign-on port returned a provider list outside the contract",
        );
    }
    let providers: Vec<Value> = list
        .providers
        .iter()
        .map(|p| json!({"id": p.id, "name": p.name, "kind": p.kind}))
        .collect();
    let body = json!({"data": {"providers": providers, "password_login": list.password_login}});
    finish((StatusCode::OK, axum::Json(body)).into_response())
}

async fn sso_begin(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(provider): Path<String>,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if !valid_provider(&provider) {
        return sso_failure(SsoFailure::ProviderNotFound, request_id);
    }
    if request.uri().query().is_some_and(|q| !q.is_empty()) {
        return sso_failure(SsoFailure::RequestInvalid, request_id);
    }
    let (headers, bytes) = match read_body(request, MAX_BODY_BYTES).await {
        Ok(read) => read,
        Err(error) => return failure(error, request_id),
    };
    let mut return_to: Option<String> = None;
    if !bytes.is_empty() {
        if media_type(&headers).as_deref() != Some("application/json") {
            return code_failure(Code::ApiUnsupportedMediaType, request_id);
        }
        let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&bytes) else {
            return sso_failure(SsoFailure::RequestInvalid, request_id);
        };
        for (key, value) in &map {
            match (key.as_str(), value) {
                ("return_to", Value::String(path)) if relative_path(path, false) => {
                    return_to = Some(path.clone());
                }
                _ => return sso_failure(SsoFailure::RequestInvalid, request_id),
            }
        }
    }
    let begin = match port
        .begin(&public_ctx(&site), &provider, return_to.as_deref())
        .await
    {
        Ok(begin) => begin,
        Err(kind) => return sso_failure(kind, request_id),
    };
    // The redirect is the port's, and must be a provider URL; the binding must be a cookie
    // value. Neither is built from request input.
    if !https_url(&begin.redirect_url, MAX_URL) || !cookie_safe(&begin.binding) {
        return internal(
            request_id,
            "the single sign-on port returned a redirect or binding outside the contract",
        );
    }
    let Some(cookie) = set_cookie(BINDING_COOKIE, &begin.binding, BINDING_MAX_AGE_SECS) else {
        return internal(request_id, "the binding cookie could not be built");
    };
    let body = json!({"data": {"redirect_url": begin.redirect_url}});
    let mut response = (StatusCode::OK, axum::Json(body)).into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    finish(response)
}

async fn sso_callback(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(provider): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if !valid_provider(&provider) {
        return sso_failure(SsoFailure::ProviderNotFound, request_id);
    }
    // The binding cookie is required before anything else is looked at.
    let Some(binding) = cookie_value(&headers, BINDING_COOKIE) else {
        return with_binding_cleared(sso_failure(SsoFailure::StateInvalid, request_id));
    };
    let query = request.uri().query().unwrap_or("");
    let Some(mut fields) = parse_fields(query, &[("code", 2048), ("state", 512), ("error", 100)])
    else {
        return with_binding_cleared(sso_failure(SsoFailure::RequestInvalid, request_id));
    };
    let params = CallbackParams {
        code: fields.remove("code"),
        state: fields.remove("state"),
        error: fields.remove("error"),
        binding,
    };
    match port.callback(&public_ctx(&site), &provider, &params).await {
        Ok(handover) => handover_response(&handover.location, &handover.cookie, request_id),
        Err(kind) => with_binding_cleared(sso_failure(kind, request_id)),
    }
}

async fn sso_acs(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(provider): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if !valid_provider(&provider) {
        return sso_failure(SsoFailure::ProviderNotFound, request_id);
    }
    let Some(binding) = cookie_value(&headers, BINDING_COOKIE) else {
        return with_binding_cleared(sso_failure(SsoFailure::StateInvalid, request_id));
    };
    let mut fields =
        match read_form(request, &[("SAMLResponse", 262_144), ("RelayState", 512)]).await {
            Ok(fields) => fields,
            Err(error) => return with_binding_cleared(failure(error, request_id)),
        };
    let Some(saml_response) = fields.remove("SAMLResponse").filter(|v| !v.is_empty()) else {
        return with_binding_cleared(sso_failure(SsoFailure::RequestInvalid, request_id));
    };
    let params = AcsParams {
        saml_response,
        relay_state: fields.remove("RelayState"),
        binding,
    };
    match port.acs(&public_ctx(&site), &provider, &params).await {
        Ok(handover) => handover_response(&handover.location, &handover.cookie, request_id),
        Err(kind) => with_binding_cleared(sso_failure(kind, request_id)),
    }
}

async fn sso_complete(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    // No body and no query: the one-time cookie is the credential.
    if request.uri().query().is_some_and(|q| !q.is_empty()) {
        return sso_failure(SsoFailure::RequestInvalid, request_id);
    }
    match read_body(request, 1024).await {
        Ok((_, bytes)) if bytes.is_empty() => {}
        Ok(_) => return sso_failure(SsoFailure::RequestInvalid, request_id),
        Err(error) => return failure(error, request_id),
    }
    let Some(handover) = cookie_value(&headers, HANDOVER_COOKIE) else {
        return sso_failure(SsoFailure::StateInvalid, request_id);
    };
    match port.complete(&public_ctx(&site), &handover).await {
        Ok(signed_in) => {
            let session = &signed_in.session;
            if session.token.is_empty() {
                return internal(
                    request_id,
                    "the single sign-on port returned an empty session token",
                );
            }
            // The token is shown once, here. No `Set-Cookie`: the session is not a cookie.
            finish((StatusCode::OK, axum::Json(session_body(session))).into_response())
        }
        Err(kind) => sso_failure(kind, request_id),
    }
}

async fn sso_backchannel_logout(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(provider): Path<String>,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if !valid_provider(&provider) {
        return sso_failure(SsoFailure::ProviderNotFound, request_id);
    }
    let mut fields = match read_form(request, &[("logout_token", 16_384)]).await {
        Ok(fields) => fields,
        Err(error) if error.status() == StatusCode::BAD_REQUEST => {
            return sso_failure(SsoFailure::LogoutRejected, request_id);
        }
        Err(error) => return failure(error, request_id),
    };
    let Some(token) = fields.remove("logout_token").filter(|t| !t.is_empty()) else {
        return sso_failure(SsoFailure::LogoutRejected, request_id);
    };
    match port
        .backchannel_logout(&public_ctx(&site), &provider, &token)
        .await
    {
        Ok(()) => finish(StatusCode::OK.into_response()),
        Err(kind) => sso_failure(kind, request_id),
    }
}

fn slo_response(outcome: SloOutcome, request_id: &RequestId) -> Response {
    match outcome {
        SloOutcome::Redirect(url) => {
            if !https_url(&url, MAX_URL) {
                return internal(
                    request_id,
                    "the single sign-on port returned a logout redirect outside the contract",
                );
            }
            let Ok(location) = HeaderValue::from_str(&url) else {
                return internal(request_id, "the logout redirect could not be built");
            };
            let mut response = StatusCode::SEE_OTHER.into_response();
            response.headers_mut().insert(header::LOCATION, location);
            finish(response)
        }
        SloOutcome::Form(html) => {
            if html.is_empty() || html.len() > MAX_FORM_HTML {
                return internal(
                    request_id,
                    "the single sign-on port returned a logout form outside the contract",
                );
            }
            let mut response = (StatusCode::OK, html).into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'none'; form-action https:; script-src 'unsafe-inline'; base-uri 'none'",
                ),
            );
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            finish(response)
        }
        SloOutcome::Done => finish(StatusCode::OK.into_response()),
    }
}

async fn sso_slo_get(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(provider): Path<String>,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if !valid_provider(&provider) {
        return sso_failure(SsoFailure::ProviderNotFound, request_id);
    }
    let query = request.uri().query().unwrap_or("");
    let Some(mut fields) = parse_fields(
        query,
        &[
            ("SAMLRequest", 16_384),
            ("SAMLResponse", 16_384),
            ("RelayState", 512),
        ],
    ) else {
        return sso_failure(SsoFailure::LogoutRejected, request_id);
    };
    let params = SloParams {
        binding: SloBinding::Redirect,
        saml_request: fields.remove("SAMLRequest"),
        saml_response: fields.remove("SAMLResponse"),
        relay_state: fields.remove("RelayState"),
    };
    if params.saml_request.is_none() == params.saml_response.is_none() {
        // Exactly one of the two: a request or a response, never both and never neither.
        return sso_failure(SsoFailure::LogoutRejected, request_id);
    }
    match port
        .single_logout(&public_ctx(&site), &provider, &params)
        .await
    {
        Ok(outcome) => slo_response(outcome, request_id),
        Err(kind) => sso_failure(kind, request_id),
    }
}

async fn sso_slo_post(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(provider): Path<String>,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if !valid_provider(&provider) {
        return sso_failure(SsoFailure::ProviderNotFound, request_id);
    }
    let mut fields = match read_form(
        request,
        &[
            ("SAMLRequest", 262_144),
            ("SAMLResponse", 262_144),
            ("RelayState", 512),
        ],
    )
    .await
    {
        Ok(fields) => fields,
        Err(error) if error.status() == StatusCode::BAD_REQUEST => {
            return sso_failure(SsoFailure::LogoutRejected, request_id);
        }
        Err(error) => return failure(error, request_id),
    };
    let params = SloParams {
        binding: SloBinding::Post,
        saml_request: fields.remove("SAMLRequest"),
        saml_response: fields.remove("SAMLResponse"),
        relay_state: fields.remove("RelayState"),
    };
    if params.saml_request.is_none() == params.saml_response.is_none() {
        return sso_failure(SsoFailure::LogoutRejected, request_id);
    }
    match port
        .single_logout(&public_ctx(&site), &provider, &params)
        .await
    {
        Ok(outcome) => slo_response(outcome, request_id),
        Err(kind) => sso_failure(kind, request_id),
    }
}

async fn sso_saml_metadata(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(provider): Path<String>,
    request: Request,
) -> Response {
    let request_id = request_id_of(&site);
    let Some(port) = state.port::<dyn SsoPort>() else {
        return code_failure(Code::HttpInternal, request_id);
    };
    if !valid_provider(&provider) {
        return sso_failure(SsoFailure::ProviderNotFound, request_id);
    }
    if request.uri().query().is_some_and(|q| !q.is_empty()) {
        return failure(
            ApiError::new(Code::ApiInvalidRequest, "This call takes no query."),
            request_id,
        );
    }
    match port.saml_metadata(&public_ctx(&site), &provider).await {
        Ok(xml) => {
            if xml.is_empty() || xml.len() > MAX_METADATA {
                return internal(
                    request_id,
                    "the single sign-on port returned metadata outside the contract",
                );
            }
            let mut response = (StatusCode::OK, xml).into_response();
            let headers = response.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/samlmetadata+xml"),
            );
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            finish(response)
        }
        Err(kind) => sso_failure(kind, request_id),
    }
}
