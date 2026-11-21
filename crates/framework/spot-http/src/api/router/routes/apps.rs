//! `app_list` (`GET /api/v1/apps`), native API 0.25.0, WP-EXT-05.T3: the installed apps, for an
//! administrator.
//!
//! The route reads the shape of the request (`cursor` and `limit` only, each once) and refuses a
//! malformed one before the port is asked. It never decides who the caller is or whether the
//! caller is an administrator: the [`AppsPort`] answers `403 permission.denied` for anyone else
//! (the route is documented, so `404` is not used). With no port the answer is `501
//! service.not_configured`; a port answer outside the contract's bounds is `500`. The view has
//! no field for bundle contents, a key or a secret. Every response is `private, no-store`.
#![allow(clippy::result_large_err)] // a refusal is a whole response, built once and returned

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value as Json, json};

use super::RouteState;
use crate::api::ports::PortKey;
use crate::api::ports::apps::{AppView, AppsPort, AppsPortError};
use crate::api::registry::Code;
use crate::api::service::ApiError;
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{build_ctx, render_error};

/// The operation ids this group serves.
pub const SERVES: &[&str] = &["app_list"];

/// The ports this group needs.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn AppsPort>()];
    &KEYS
}

/// The route.
pub fn routes() -> Router<RouteState> {
    Router::new().route("/api/v1/apps", get(app_list))
}

const OP: &str = "app_list";

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn refuse(error: ApiError, request_id: &RequestId) -> Response {
    tracing::info!(
        operation = OP,
        code = error.code.as_str(),
        "request refused"
    );
    render_error(error, request_id)
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(raw.get(i + 1..i + 3)?, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn parse_query(query: &str) -> Result<(Option<String>, usize), ApiError> {
    let (mut cursor, mut limit) = (None, None);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "cursor" if cursor.is_none() => {
                cursor = Some(
                    percent_decode(value)
                        .filter(|v| (1..=2048).contains(&v.len()))
                        .filter(|v| v.bytes().all(|b| (0x21..=0x7e).contains(&b)))
                        .ok_or_else(|| invalid("`cursor` must be a cursor this service issued."))?,
                );
            }
            "limit" if limit.is_none() => {
                limit = Some(
                    (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                        .then(|| value.parse::<usize>().ok())
                        .flatten()
                        .filter(|n| (1..=100).contains(n))
                        .ok_or_else(|| invalid("`limit` must be a whole number from 1 to 100."))?,
                );
            }
            _ => {
                return Err(invalid(
                    "`cursor` and `limit` are the only query parameters, each once.",
                ));
            }
        }
    }
    Ok((cursor, limit.unwrap_or(50)))
}

fn port_error(error: AppsPortError) -> ApiError {
    match error {
        AppsPortError::PermissionDenied => ApiError::from_code(Code::PermissionDenied),
        AppsPortError::CursorInvalid => invalid("`cursor` is not one this service issued."),
        AppsPortError::Unauthenticated => ApiError::from_code(Code::AuthSessionExpired),
        AppsPortError::Unavailable => ApiError::from_code(Code::ServerUnavailable),
        AppsPortError::Internal => ApiError::from_code(Code::HttpInternal),
    }
}

fn caps_ok(list: &[String]) -> bool {
    list.len() <= 100 && list.iter().all(|c| (1..=100).contains(&c.chars().count()))
}

/// The contract's `InstalledApp`, or `None` when the view breaks the contract's bounds.
fn app_json(app: &AppView) -> Option<Json> {
    let ok = (1..=100).contains(&app.name.chars().count())
        && (1..=40).contains(&app.version.chars().count())
        && (1..=128).contains(&app.digest.chars().count())
        && app
            .publisher
            .as_deref()
            .is_none_or(|p| p.chars().count() <= 200)
        && caps_ok(&app.capabilities_granted)
        && caps_ok(&app.capabilities_requested)
        && app
            .last_error_code
            .as_deref()
            .is_none_or(|c| (1..=100).contains(&c.chars().count()));
    if !ok {
        return None;
    }
    let mut map = Map::new();
    map.insert("name".to_owned(), Json::from(app.name.clone()));
    map.insert("version".to_owned(), Json::from(app.version.clone()));
    map.insert("digest".to_owned(), Json::from(app.digest.clone()));
    map.insert("state".to_owned(), Json::from(app.state.as_str()));
    if let Some(p) = &app.publisher {
        map.insert("publisher".to_owned(), Json::from(p.clone()));
    }
    map.insert(
        "capabilities_granted".to_owned(),
        json!(app.capabilities_granted),
    );
    map.insert(
        "capabilities_requested".to_owned(),
        json!(app.capabilities_requested),
    );
    map.insert("loaded".to_owned(), Json::from(app.loaded));
    map.insert("last_error_code".to_owned(), json!(app.last_error_code));
    Some(Json::Object(map))
}

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no app service configured.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

async fn app_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = &site.request_id;
    let query = request.uri().query().unwrap_or("").to_owned();
    drop(request);
    let response = async {
        let Some(port) = state.port::<dyn AppsPort>() else {
            return Err(not_configured(request_id));
        };
        let ctx = build_ctx(&site, &headers, None);
        if ctx.credential.is_none() {
            return Err(refuse(ApiError::from_code(Code::AuthRequired), request_id));
        }
        let (cursor, limit) = parse_query(&query).map_err(|e| refuse(e, request_id))?;
        let page = port
            .list(&ctx, cursor.as_deref(), limit)
            .await
            .map_err(|e| refuse(port_error(e), request_id))?;
        let internal = || refuse(ApiError::from_code(Code::HttpInternal), request_id);
        if page.apps.len() > limit
            || page
                .next_cursor
                .as_deref()
                .is_some_and(|c| !(1..=2048).contains(&c.len()))
        {
            return Err(internal());
        }
        let mut data = Vec::with_capacity(page.apps.len());
        for app in &page.apps {
            data.push(app_json(app).ok_or_else(internal)?);
        }
        Ok::<Response, Response>(
            (
                StatusCode::OK,
                axum::Json(json!({ "data": data, "next_cursor": page.next_cursor })),
            )
                .into_response(),
        )
    }
    .await;
    let mut response = response.unwrap_or_else(|r| r);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}
