//! `notification_list` (`GET /api/v1/notifications?after=&limit=`), contract 0.10.0,
//! WP-CON-03.T14: the caller's own notices after a given one, oldest first.
//!
//! The route takes no user, room or record, so a caller cannot ask for another user's notices;
//! the [`NoticeSource`] port answers for the caller its credential names. An `after` that is
//! malformed, or that the caller's feed does not hold, is one `api.invalid_request`; whether the
//! id belongs to another user, was forged or was pruned is never told.
//!
//! Registered under the registry's standard names (`SERVES`, `routes`, `required_ports`); the
//! authentication layer the router puts around every registered group guards the route, and
//! every answer is `private, no-store`.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value as Json, json};

use super::RouteState;
use crate::api::ports::notices::{Notice, NoticeError, NoticeSource};
use crate::api::ports::PortKey;
use crate::api::registry::Code;
use crate::api::service::ApiError;
use crate::site::SiteContext;

use super::super::{build_ctx, render_error};

/// The operation ids this group serves.
pub const SERVES: &[&str] = &["notification_list"];

/// The ports this group needs.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn NoticeSource>()];
    &KEYS
}

/// The feed route.
pub fn routes() -> Router<RouteState> {
    Router::new().route("/api/v1/notifications", get(notification_list))
}

/// The contract's `limit` default.
const DEFAULT_LIMIT: usize = 50;
/// The contract's `limit` maximum.
const MAX_LIMIT: usize = 100;
/// The contract's `data` bound, serialised.
const MAX_DATA_BYTES: usize = 2048;

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The two query parameters, each at most once, nothing else.
fn parse_query(query: &str) -> Result<(Option<String>, usize), ApiError> {
    let mut after: Option<String> = None;
    let mut limit: Option<usize> = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "after" => {
                if after.is_some() {
                    return Err(invalid("`after` can be given only once."));
                }
                let value = percent_decode(value)
                    .filter(|v| (1..=128).contains(&v.len()))
                    .filter(|v| v.bytes().all(|b| (0x21..=0x7e).contains(&b)))
                    .ok_or_else(|| invalid("`after` must be an event id."))?;
                after = Some(value);
            }
            "limit" => {
                if limit.is_some() {
                    return Err(invalid("`limit` can be given only once."));
                }
                let n = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| value.parse::<usize>().ok())
                    .flatten()
                    .filter(|n| (1..=MAX_LIMIT).contains(n))
                    .ok_or_else(|| invalid("`limit` must be a whole number from 1 to 100."))?;
                limit = Some(n);
            }
            _ => {
                return Err(invalid(
                    "`after` and `limit` are the only query parameters of the feed.",
                ));
            }
        }
    }
    Ok((after, limit.unwrap_or(DEFAULT_LIMIT)))
}

/// The contract's `NotificationItem`, or `None` when a port item breaks the contract's bounds.
fn item_json(notice: &Notice) -> Option<Json> {
    let event_ok = {
        let b = notice.event.as_bytes();
        (1..=63).contains(&b.len())
            && b[0].is_ascii_lowercase()
            && b[1..]
                .iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
    };
    let id_ok = (1..=128).contains(&notice.event_id.len());
    let data = Json::Object(notice.data.clone());
    let data_ok = serde_json::to_vec(&data).is_ok_and(|v| v.len() <= MAX_DATA_BYTES);
    let time_ok = notice.created_at.ends_with('Z') && notice.created_at.len() <= 40;
    (event_ok && id_ok && data_ok && time_ok).then(|| {
        json!({
            "event_id": notice.event_id,
            "event": notice.event,
            "data": data,
            "created_at": notice.created_at,
        })
    })
}

fn with_cache_control(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

async fn notification_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let request_id = &site.request_id;
    let fail = |error: ApiError| with_cache_control(render_error(error, request_id));
    let Some(source) = state.port::<dyn NoticeSource>() else {
        return fail(ApiError::from_code(Code::HttpInternal));
    };
    let (after, limit) = match parse_query(request.uri().query().unwrap_or("")) {
        Ok(parsed) => parsed,
        Err(error) => return fail(error),
    };
    let ctx = build_ctx(&site, &headers, None);
    if ctx.credential.is_none() {
        return fail(ApiError::from_code(Code::AuthRequired));
    }
    let notices = match source.list(&ctx, after.as_deref(), limit).await {
        Ok(notices) => notices,
        Err(NoticeError::UnknownAfter) => {
            return fail(invalid("`after` is not an event id this feed holds."));
        }
        Err(NoticeError::Unauthenticated) => {
            return fail(ApiError::from_code(Code::AuthSessionExpired));
        }
        Err(NoticeError::Unavailable) => {
            return fail(ApiError::from_code(Code::ServerUnavailable));
        }
    };
    let mut items = Vec::with_capacity(notices.len().min(limit));
    for notice in notices.iter().take(limit) {
        match item_json(notice) {
            Some(item) => items.push(item),
            None => {
                tracing::error!("the notice source returned an item outside the contract");
                return fail(ApiError::from_code(Code::HttpInternal));
            }
        }
    }
    let next = notices.iter().take(limit).next_back().map(|n| n.event_id.clone());
    let mut body = serde_json::Map::new();
    body.insert("items".to_owned(), Json::Array(items));
    if let Some(next) = next {
        body.insert("next".to_owned(), Json::String(next));
    }
    with_cache_control((StatusCode::OK, axum::Json(Json::Object(body))).into_response())
}
