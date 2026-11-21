//! `GET /api/v1/resource/{doctype}/{id}/ai-timeline` (`resource_ai_timeline`, WP-AI-11.T11,
//! contract 0.28.0, ADR-066): the AI actions that touched one document, as the caller may see
//! them.
//!
//! # What the route decides, and what it does not
//!
//! The route reads the shape of a request and the shape of an answer. A bearer is present (the
//! table also marks the operation for staff and partner only, so a consumer credential is refused
//! with `403` before this handler runs, as every resource route refuses it), the query holds at
//! most one `limit`, a whole number from 1 to 100 (default 50), and nothing else: any other query
//! parameter, a repeated `limit` and a `limit` outside the range are `400 api.invalid_request`,
//! refused **before** the port is asked.
//!
//! The route never decides who the caller is, whether the caller may read the document, which
//! argument values the caller may see or whether the access can be decided: [`AiTimelinePort`]
//! does, and the route passes it no viewer id. It builds the answer from the port's rows and from
//! nothing else: no count, no hint of how many entries were hidden, no access level.
//!
//! # A malformed path is not an error
//!
//! The contract declares the `doctype` and `id` path parameters loose on purpose: a name that
//! matches no DocType, an id that matches no document, an id of the wrong shape, a value that is
//! not valid text and a value longer than the contract's 128 characters all answer the same `200`
//! `{"data": {"entries": []}}` as a missing document or one the caller may not read, so that an
//! unreadable document cannot be told apart from a missing one. The route therefore answers the
//! empty list itself for a path it cannot even pass on (not valid text, empty or too long) and
//! hands every other value to the port, which answers the same empty list for a name or an id that
//! names nothing.
//!
//! # The answer
//!
//! `200` `{"data": {"entries": [...]}}` with `Cache-Control: private, no-store`, on every answer
//! including errors. An entry is exactly the contract's `AiTimelineEntry`; a row that breaks the
//! contract's bounds (a text out of its length, an unknown outcome, more than 32 arguments, a
//! value over 128 characters) is `500 http.internal`, never a clipped or invented entry. The
//! port's refusal passes through with its code and status: the undecidable access is `503
//! server.unavailable` with `Retry-After`, never entries and never an empty list.
//!
//! # Unserved
//!
//! The group is mounted only with its port; the host provides the port only when the AI audit
//! chain is on the site, so a site without it answers as for an unserved operation.
#![allow(clippy::result_large_err)] // a refusal is a whole response, built once and returned

use axum::Router;
use axum::extract::rejection::PathRejection;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Value as Json, json};

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::ai_timeline::{AiTimelinePort, TimelineRow};
use crate::api::registry::Code;
use crate::api::service::ApiError;
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::{build_ctx, render_error};

/// The operation ids this group serves.
pub const SERVES: &[&str] = &["resource_ai_timeline"];

/// The contract's `limit` default.
const DEFAULT_LIMIT: usize = 50;
/// The contract's `limit` maximum.
const MAX_LIMIT: usize = 100;
/// The contract's bound on the `doctype` and `id` path parameters, in characters.
const MAX_PATH_CHARS: usize = 128;
/// The contract's most arguments in one entry.
const MAX_ARGUMENTS: usize = 32;
/// The contract's longest argument value, in characters.
const MAX_VALUE_CHARS: usize = 128;
/// The contract's longest instant text.
const MAX_AT_CHARS: usize = 40;

/// The ports this group needs: the timeline port, whose adapter decides identity, access and
/// masking. A group without it is not mounted at all.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn AiTimelinePort>()];
    &KEYS
}

/// The timeline route.
pub fn routes() -> Router<RouteState> {
    Router::new().route(
        "/api/v1/resource/{doctype}/{id}/ai-timeline",
        get(resource_ai_timeline),
    )
}

/// The group as the registry's mount function takes it, for a test that builds the group by hand.
pub fn registration() -> Registration {
    Registration {
        name: "ai_timeline",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

/// The `limit` of the query: at most once, a whole number from 1 to 100, and no other parameter.
/// Never echoes a value.
fn parse_limit(query: &str) -> Result<usize, ApiError> {
    let mut limit: Option<usize> = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "limit" || limit.is_some() {
            return Err(invalid(
                "`limit` is the only query parameter, at most once.",
            ));
        }
        limit = Some(
            (!value.is_empty() && value.len() <= 4 && value.bytes().all(|b| b.is_ascii_digit()))
                .then(|| value.parse::<usize>().ok())
                .flatten()
                .filter(|n| (1..=MAX_LIMIT).contains(n))
                .ok_or_else(|| invalid("`limit` must be a whole number from 1 to 100."))?,
        );
    }
    Ok(limit.unwrap_or(DEFAULT_LIMIT))
}

fn chars_within(text: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&text.chars().count())
}

/// The contract's `AiTimelineEntry`, or `None` when the row breaks the contract's bounds.
fn entry_json(row: &TimelineRow) -> Option<Json> {
    let fits = chars_within(&row.at, 1, MAX_AT_CHARS)
        && chars_within(&row.kind, 1, 64)
        && chars_within(&row.agent, 1, 128)
        && chars_within(&row.acting_user, 1, 128)
        && chars_within(&row.tool, 1, 128)
        && matches!(row.outcome.as_str(), "ok" | "refused" | "failed")
        && row.arguments.len() <= MAX_ARGUMENTS
        && row.arguments.iter().all(|(name, value)| {
            chars_within(name, 1, 128) && chars_within(value, 0, MAX_VALUE_CHARS)
        });
    if !fits {
        return None;
    }
    let arguments: Vec<Json> = row
        .arguments
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    Some(json!({
        "seq": row.seq,
        "at": row.at,
        "kind": row.kind,
        "agent": row.agent,
        "acting_user": row.acting_user,
        "tool": row.tool,
        "outcome": row.outcome,
        "arguments": arguments,
    }))
}

fn answer(entries: Vec<Json>) -> Response {
    (
        StatusCode::OK,
        axum::Json(json!({ "data": { "entries": entries } })),
    )
        .into_response()
}

fn failure(error: ApiError, request_id: &RequestId) -> Response {
    render_error(error, request_id)
}

async fn resource_ai_timeline(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let request_id = &site.request_id;
    let result: Result<Response, Response> = async {
        let Some(port) = state.port::<dyn AiTimelinePort>() else {
            return Err(failure(ApiError::from_code(Code::HttpInternal), request_id));
        };
        let ctx = build_ctx(&site, &headers, None);
        if ctx.credential.is_none() {
            return Err(failure(ApiError::from_code(Code::AuthRequired), request_id));
        }
        let limit = parse_limit(query.as_deref().unwrap_or(""))
            .map_err(|error| failure(error, request_id))?;
        // A path that is not valid text, or that is empty or too long for the contract, names no
        // document: the same empty answer as a missing one, never a `400`, and the port is not
        // asked.
        let Ok(Path((doctype, id))) = path else {
            return Ok(answer(Vec::new()));
        };
        if !chars_within(&doctype, 1, MAX_PATH_CHARS) || !chars_within(&id, 1, MAX_PATH_CHARS) {
            return Ok(answer(Vec::new()));
        }
        let rows = port
            .timeline(&ctx, &doctype, &id, limit)
            .await
            .map_err(|error| failure(error, request_id))?;
        // Never more than was asked for; a port that answers more is a bug, not an answer.
        if rows.len() > limit {
            return Err(failure(ApiError::from_code(Code::HttpInternal), request_id));
        }
        let mut entries = Vec::with_capacity(rows.len());
        for row in &rows {
            match entry_json(row) {
                Some(entry) => entries.push(entry),
                None => return Err(failure(ApiError::from_code(Code::HttpInternal), request_id)),
            }
        }
        Ok(answer(entries))
    }
    .await;
    let mut response = result.unwrap_or_else(|refusal| refusal);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> TimelineRow {
        TimelineRow {
            seq: 7,
            at: "2026-11-21T09:05:00.123456Z".to_owned(),
            kind: "tool_call".to_owned(),
            agent: "agent-1".to_owned(),
            acting_user: "user-1".to_owned(),
            tool: "update_doc".to_owned(),
            outcome: "ok".to_owned(),
            arguments: vec![("title".to_owned(), "Q3 plan".to_owned())],
        }
    }

    #[test]
    fn the_limit_is_one_to_a_hundred_once_and_alone() {
        assert_eq!(parse_limit("").ok(), Some(50));
        assert_eq!(parse_limit("limit=1").ok(), Some(1));
        assert_eq!(parse_limit("limit=100").ok(), Some(100));
        assert_eq!(parse_limit("limit=07").ok(), Some(7));
        for bad in [
            "limit=0",
            "limit=101",
            "limit=",
            "limit",
            "limit=-1",
            "limit=+5",
            "limit=1.5",
            "limit=ten",
            "limit=10000",
            "limit=5&limit=6",
            "limit=5&other=1",
            "other=1",
            "Limit=5",
            "limit=%35",
        ] {
            let error = parse_limit(bad).err();
            assert!(error.is_some(), "{bad:?} must be refused");
            if let Some(error) = error {
                assert_eq!(
                    error.code.as_str(),
                    Code::ApiInvalidRequest.as_str(),
                    "{bad:?}"
                );
            }
        }
    }

    #[test]
    fn an_entry_is_exactly_the_contracts_shape() {
        let entry = entry_json(&row());
        assert_eq!(
            entry,
            Some(json!({
                "seq": 7,
                "at": "2026-11-21T09:05:00.123456Z",
                "kind": "tool_call",
                "agent": "agent-1",
                "acting_user": "user-1",
                "tool": "update_doc",
                "outcome": "ok",
                "arguments": [{ "name": "title", "value": "Q3 plan" }],
            }))
        );
    }

    #[test]
    fn a_row_that_breaks_the_contract_is_never_clipped_into_an_entry() {
        let mut broken = Vec::new();
        let mut r = row();
        r.outcome = "maybe".to_owned();
        broken.push(r);
        let mut r = row();
        r.agent = String::new();
        broken.push(r);
        let mut r = row();
        r.kind = "k".repeat(65);
        broken.push(r);
        let mut r = row();
        r.at = "2".repeat(41);
        broken.push(r);
        let mut r = row();
        r.arguments = (0..33).map(|i| (format!("a{i}"), "v".to_owned())).collect();
        broken.push(r);
        let mut r = row();
        r.arguments = vec![("a".to_owned(), "v".repeat(129))];
        broken.push(r);
        let mut r = row();
        r.arguments = vec![(String::new(), "v".to_owned())];
        broken.push(r);
        for r in &broken {
            assert!(entry_json(r).is_none(), "{r:?}");
        }
        // The bounds themselves are fine, and an empty argument value is a value.
        let mut r = row();
        r.arguments = (0..32)
            .map(|i| (format!("a{i}"), "v".repeat(128)))
            .collect();
        assert!(entry_json(&r).is_some());
        let mut r = row();
        r.arguments = vec![("a".to_owned(), String::new())];
        assert!(entry_json(&r).is_some());
    }

    #[test]
    fn the_group_serves_the_one_operation_authenticated() {
        assert_eq!(SERVES, &["resource_ai_timeline"]);
        let registration = registration();
        assert!(registration.public.is_none());
        assert_eq!((registration.required_ports)().len(), 1);
    }
}
