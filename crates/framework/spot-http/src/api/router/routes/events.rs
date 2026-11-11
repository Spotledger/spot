//! `/api/v1/events/*` (contract 0.4.0, WP-INT-02.T4): the named-consumer event stream of ADR-078
//! at delivery level **D1** (at-least-once, resumable; there is no exactly-once claim).
//!
//! Eight operations over one [`EventStreamPort`]: `event_consumer_list`, `_put`, `_get`,
//! `_delete`, `_pull`, `_ack`, `_restart` and the server-sent-events binding `event_stream`.
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request (names, bodies, positions, the lease header, the
//! query of the stream) and refuse a malformed one before the port is asked. They never decide
//! who the caller is, which consumers the caller may see or which events a consumer's owner may
//! read: the port answers for the caller its credential names, and the routes pass it no user
//! and no tenant, so a route cannot be asked to act for someone else. A position is opaque to
//! the routes; they check only its published pattern.
//!
//! # The stream
//!
//! `event_stream` answers `200 text/event-stream` with `Cache-Control: no-store` and the
//! consumer's lease in `X-Consumer-Lease`. The body is pulled from the port's feed frame by
//! frame; the route adds a `: keepalive` comment whenever nothing was sent for the heartbeat
//! interval and ends the stream when no event was delivered for the idle limit (heartbeats do
//! not count). Dropping the body (the client left, the server stopped) drops the feed, which
//! gives the lease back. Authentication is the `Authorization` header and nothing else; no
//! query parameter can carry a token.
//!
//! **Registered.** The file declares the registry's three items (`SERVES`, `routes`,
//! `required_ports`), so the eight operations leave the conformance walk's computed gap and
//! answer within the contract in the same walk as every other operation. The real host does
//! not put the events port into `ApiServices`, so the registry skips the group there (port
//! absent, logged once) and the host mounts it itself through [`registration`] over its own
//! ports; the group is never mounted twice.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use hyper::body::{Body as HttpBody, Frame};
use serde_json::{Map, Value as Json, json};
use tokio::time::Instant;

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::events::{
    CheckpointOut, ConsumerOut, ConsumerSpecIn, EventFeed, EventStreamPort, FeedItem,
    PayloadModeIn, PullIn, StartIn, StreamIn, StreamLimits,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, FieldIssue, RequestCtx};
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail, path_identifier, take_body};

/// The operation ids this group serves (registered under the registry's standard name).
pub const SERVES: &[&str] = &[
    "event_consumer_list",
    "event_consumer_put",
    "event_consumer_get",
    "event_consumer_delete",
    "event_consumer_pull",
    "event_consumer_ack",
    "event_consumer_restart",
    "event_stream",
];

/// The ports this group needs: the event stream port, whose adapter decides identity,
/// ownership, permission and audit. A group without it is not mounted at all.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn EventStreamPort>()];
    &KEYS
}

/// The event routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/events/consumers", get(event_consumer_list))
        .route(
            "/api/v1/events/consumers/{name}",
            get(event_consumer_get)
                .put(event_consumer_put)
                .delete(event_consumer_delete),
        )
        .route(
            "/api/v1/events/consumers/{name}/pull",
            post(event_consumer_pull),
        )
        .route(
            "/api/v1/events/consumers/{name}/ack",
            post(event_consumer_ack),
        )
        .route(
            "/api/v1/events/consumers/{name}/restart",
            post(event_consumer_restart),
        )
        .route("/api/v1/events/stream", get(event_stream))
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "events",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

/// The lease header, as the contract names it.
pub const LEASE_HEADER: &str = "x-consumer-lease";

/// The SSE keepalive comment (contract: every 25 seconds).
const KEEPALIVE: &[u8] = b": keepalive\n\n";

/// `1` to `64` topics, `1` to `64` partitions, and the other bounds of the contract.
const MAX_TOPICS: usize = 64;
const MAX_PARTITION: u64 = 63;
const MAX_PARTITIONS: u64 = 64;
const MAX_FILTER: usize = 2000;
const MAX_LIMIT: u64 = 1000;
const DEFAULT_LIMIT: u32 = 100;
const MAX_WAIT_MS: u64 = 30_000;
const MAX_LEASE: usize = 512;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn spec_invalid(field: &str, message: &str) -> ApiError {
    ApiError::new(Code::EventstreamSpecInvalid, message).with_field(FieldIssue::new(
        field,
        Code::EventstreamSpecInvalid.as_str(),
        message,
    ))
}

fn position_invalid() -> ApiError {
    ApiError::new(
        Code::EventstreamPositionInvalid,
        "A position must be one this consumer was given.",
    )
}

fn private_no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

/// Whether `text` is an `EventPosition`: `v1.` and 1 to 255 URL-safe base64 characters.
fn position_shape(text: &str) -> bool {
    let Some(token) = text.strip_prefix("v1.") else {
        return false;
    };
    (1..=255).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `text` is a topic selector: dotted lower-case segments, the last possibly `*`.
fn topic_shape(text: &str) -> bool {
    let segments: Vec<&str> = text.split('.').collect();
    let (last, rest) = match segments.split_last() {
        Some(split) => split,
        None => return false,
    };
    let word = |s: &str| {
        let mut chars = s.chars();
        matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    if !rest.iter().all(|s| word(s)) {
        return false;
    }
    if *last == "*" {
        return !rest.is_empty();
    }
    word(last)
}

/// The lease header, when sent: one header, 1 to 512 visible ASCII characters.
fn lease_header(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let mut values = headers.get_all(HeaderName::from_static(LEASE_HEADER)).iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(invalid("`X-Consumer-Lease` can be given only once."));
    }
    let text = first
        .to_str()
        .ok()
        .filter(|t| (1..=MAX_LEASE).contains(&t.len()))
        .filter(|t| t.bytes().all(|b| (0x21..=0x7e).contains(&b)))
        .ok_or_else(|| invalid("`X-Consumer-Lease` is not a lease this server issued."))?;
    Ok(Some(text.to_owned()))
}

/// Reads a JSON object that has exactly the keys in `allowed` (and at least `required`).
fn object_of<'a>(
    body: &'a Json,
    allowed: &[&str],
    required: &[&str],
) -> Result<&'a Map<String, Json>, ApiError> {
    let map = body
        .as_object()
        .ok_or_else(|| invalid("The request body must be a JSON object."))?;
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(
            "The request body has a field this operation does not accept.",
        ));
    }
    if let Some(missing) = required.iter().find(|k| !map.contains_key(**k)) {
        return Err(invalid(format!("The request body needs `{missing}`.")));
    }
    Ok(map)
}

fn whole(value: &Json, what: &str, max: u64, min: u64) -> Result<u32, ApiError> {
    value
        .as_u64()
        .filter(|n| (min..=max).contains(n))
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| invalid(format!("`{what}` must be a whole number from {min} to {max}.")))
}

fn start_of(value: &Json) -> Result<StartIn, ApiError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid("`start` must be an object with a `kind`."))?;
    let kind = map
        .get("kind")
        .and_then(Json::as_str)
        .ok_or_else(|| invalid("`start.kind` must be tail, from_beginning or from_position."))?;
    match kind {
        "tail" | "from_beginning" => {
            object_of(value, &["kind"], &["kind"])?;
            Ok(if kind == "tail" {
                StartIn::Tail
            } else {
                StartIn::FromBeginning
            })
        }
        "from_position" => {
            object_of(value, &["kind", "position"], &["kind", "position"])?;
            let position = map
                .get("position")
                .and_then(Json::as_str)
                .filter(|p| position_shape(p))
                .ok_or_else(position_invalid)?;
            Ok(StartIn::FromPosition(position.to_owned()))
        }
        _ => Err(invalid(
            "`start.kind` must be tail, from_beginning or from_position.",
        )),
    }
}

fn spec_of(body: &Json) -> Result<ConsumerSpecIn, ApiError> {
    let map = object_of(
        body,
        &["topics", "filter", "partitions", "payload_mode", "start"],
        &["topics", "partitions", "payload_mode"],
    )?;
    let topics = map
        .get("topics")
        .and_then(Json::as_array)
        .ok_or_else(|| invalid("`topics` must be an array of topic names."))?;
    if topics.is_empty() || topics.len() > MAX_TOPICS {
        return Err(spec_invalid("topics", "A consumer needs 1 to 64 topics."));
    }
    let mut names = Vec::with_capacity(topics.len());
    for topic in topics {
        let name = topic
            .as_str()
            .ok_or_else(|| invalid("`topics` must be an array of topic names."))?;
        if !topic_shape(name) {
            return Err(spec_invalid(
                "topics",
                "A topic is dotted lower-case words, or a prefix ending in `.*`.",
            ));
        }
        names.push(name.to_owned());
    }
    let filter = match map.get("filter") {
        None | Some(Json::Null) => None,
        Some(Json::String(text)) if text.chars().count() <= MAX_FILTER => Some(text.clone()),
        Some(Json::String(_)) => {
            return Err(spec_invalid("filter", "A filter is at most 2000 characters."));
        }
        Some(_) => return Err(invalid("`filter` must be a string or null.")),
    };
    let partitions = map
        .get("partitions")
        .and_then(Json::as_u64)
        .ok_or_else(|| invalid("`partitions` must be a whole number."))?;
    if !(1..=MAX_PARTITIONS).contains(&partitions) {
        return Err(spec_invalid("partitions", "partitions must be 1 to 64."));
    }
    let payload_mode = match map.get("payload_mode").and_then(Json::as_str) {
        Some("ids_only") => PayloadModeIn::IdsOnly,
        Some("fields") => PayloadModeIn::Fields,
        _ => return Err(invalid("`payload_mode` must be ids_only or fields.")),
    };
    let start = map.get("start").map(start_of).transpose()?;
    Ok(ConsumerSpecIn {
        topics: names,
        filter,
        partitions: u32::try_from(partitions).unwrap_or(1),
        payload_mode,
        start,
    })
}

fn consumer_json(consumer: &ConsumerOut) -> Json {
    json!({
        "name": consumer.name,
        "owner": consumer.owner,
        "topics": consumer.topics,
        "filter": consumer.filter,
        "partitions": consumer.partitions,
        "payload_mode": consumer.payload_mode.as_str(),
        "partitions_state": consumer
            .partitions_state
            .iter()
            .map(|p| json!({
                "partition": p.partition,
                "checkpoint": p.checkpoint,
                "parked": p.parked,
            }))
            .collect::<Vec<_>>(),
    })
}

fn checkpoint_json(checkpoint: &CheckpointOut) -> Json {
    json!({ "data": { "partition": checkpoint.partition, "position": checkpoint.position } })
}

/// The port and the request context, or the answer to give without them.
struct Call {
    port: std::sync::Arc<dyn EventStreamPort>,
    ctx: RequestCtx,
}

fn call(state: &RouteState, site: &SiteContext, headers: &HeaderMap) -> Result<Call, Failure> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn EventStreamPort>() else {
        return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
    };
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(fail(ApiError::from_code(Code::AuthRequired), request_id));
    }
    Ok(Call { port, ctx })
}

type Handled = Result<Response, Failure>;

fn done(result: Handled) -> Response {
    match result {
        Ok(response) => private_no_store(response),
        Err(failure) => private_no_store(failure.into_response()),
    }
}

async fn event_consumer_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let call = call(&state, &site, &headers)?;
        let consumers = call
            .port
            .list(&call.ctx)
            .await
            .map_err(|e| fail(e, &site.request_id))?;
        let data: Vec<Json> = consumers.iter().map(consumer_json).collect();
        Ok((StatusCode::OK, axum::Json(json!({ "data": data }))).into_response())
    }
    .await;
    done(result)
}

async fn event_consumer_put(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(name): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        path_identifier(&name, "consumer", request_id)?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let spec = spec_of(&body).map_err(|e| fail(e, request_id))?;
        let (consumer, created) = call
            .port
            .put(&call.ctx, &name, spec)
            .await
            .map_err(|e| fail(e, request_id))?;
        let status = if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        };
        Ok((status, axum::Json(json!({ "data": consumer_json(&consumer) }))).into_response())
    }
    .await;
    done(result)
}

async fn event_consumer_get(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let call = call(&state, &site, &headers)?;
        path_identifier(&name, "consumer", &site.request_id)?;
        let consumer = call
            .port
            .get(&call.ctx, &name)
            .await
            .map_err(|e| fail(e, &site.request_id))?;
        Ok((
            StatusCode::OK,
            axum::Json(json!({ "data": consumer_json(&consumer) })),
        )
            .into_response())
    }
    .await;
    done(result)
}

async fn event_consumer_delete(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let call = call(&state, &site, &headers)?;
        path_identifier(&name, "consumer", &site.request_id)?;
        call.port
            .delete(&call.ctx, &name)
            .await
            .map_err(|e| fail(e, &site.request_id))?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    done(result)
}

async fn event_consumer_pull(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(name): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        path_identifier(&name, "consumer", request_id)?;
        let lease = lease_header(&headers).map_err(|e| fail(e, request_id))?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let map = object_of(&body, &["partition", "limit", "wait_ms"], &["partition"])
            .map_err(|e| fail(e, request_id))?;
        let pull = PullIn {
            partition: whole(&map["partition"], "partition", MAX_PARTITION, 0)
                .map_err(|e| fail(e, request_id))?,
            limit: match map.get("limit") {
                None => DEFAULT_LIMIT,
                Some(v) => whole(v, "limit", MAX_LIMIT, 1).map_err(|e| fail(e, request_id))?,
            },
            wait_ms: match map.get("wait_ms") {
                None => 0,
                Some(v) => whole(v, "wait_ms", MAX_WAIT_MS, 0).map_err(|e| fail(e, request_id))?,
            },
        };
        let out = call
            .port
            .pull(&call.ctx, &name, lease.as_deref(), pull)
            .await
            .map_err(|e| fail(e, request_id))?;
        if !position_shape(&out.next_position) || out.lease.is_empty() {
            tracing::error!("the event stream port returned a value outside the contract");
            return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
        }
        let mut body = Map::new();
        body.insert(
            "data".to_owned(),
            json!({
                "events": out.events,
                "next_position": out.next_position,
                "skipped": out.skipped,
                "window_full": out.window_full,
            }),
        );
        if out.window_full {
            body.insert(
                "messages".to_owned(),
                json!([{
                    "level": "info",
                    "code": Code::EventstreamWindowFull.as_str(),
                    "message": "Acknowledge the events already applied before pulling more.",
                }]),
            );
        }
        let mut response = (StatusCode::OK, axum::Json(Json::Object(body))).into_response();
        let lease_value = HeaderValue::from_str(&out.lease).map_err(|_| {
            tracing::error!("the event stream port returned a lease that is not a header value");
            fail(ApiError::from_code(Code::HttpInternal), request_id)
        })?;
        response
            .headers_mut()
            .insert(HeaderName::from_static(LEASE_HEADER), lease_value);
        Ok(response)
    }
    .await;
    done(result)
}

async fn event_consumer_ack(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(name): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        path_identifier(&name, "consumer", request_id)?;
        let lease = lease_header(&headers).map_err(|e| fail(e, request_id))?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let map = object_of(&body, &["partition", "position"], &["partition", "position"])
            .map_err(|e| fail(e, request_id))?;
        let partition = whole(&map["partition"], "partition", MAX_PARTITION, 0)
            .map_err(|e| fail(e, request_id))?;
        let position = map["position"]
            .as_str()
            .filter(|p| position_shape(p))
            .ok_or_else(|| fail(position_invalid(), request_id))?;
        let checkpoint = call
            .port
            .ack(&call.ctx, &name, lease.as_deref(), partition, position)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok((StatusCode::OK, axum::Json(checkpoint_json(&checkpoint))).into_response())
    }
    .await;
    done(result)
}

async fn event_consumer_restart(
    State(state): State<RouteState>,
    site: SiteContext,
    Path(name): Path<String>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        path_identifier(&name, "consumer", request_id)?;
        let lease = lease_header(&headers).map_err(|e| fail(e, request_id))?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let map = object_of(&body, &["partition", "from"], &["partition", "from"])
            .map_err(|e| fail(e, request_id))?;
        let partition = whole(&map["partition"], "partition", MAX_PARTITION, 0)
            .map_err(|e| fail(e, request_id))?;
        let from = start_of(&map["from"]).map_err(|e| fail(e, request_id))?;
        let checkpoint = call
            .port
            .restart(&call.ctx, &name, lease.as_deref(), partition, from)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok((StatusCode::OK, axum::Json(checkpoint_json(&checkpoint))).into_response())
    }
    .await;
    done(result)
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

/// The stream's query: `consumer` and `partition` are required, `after` is optional, and
/// nothing else is accepted (a token in a URL is never read, whatever it is called).
fn stream_query(query: &str) -> Result<(String, u32, Option<String>), ApiError> {
    let mut consumer: Option<String> = None;
    let mut partition: Option<u32> = None;
    let mut after: Option<String> = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        match key {
            "consumer" if consumer.is_none() => {
                let value = percent_decode(value)
                    .filter(|v| consumer_name_shape(v))
                    .ok_or_else(|| invalid("`consumer` must be a consumer name."))?;
                consumer = Some(value);
            }
            "partition" if partition.is_none() => {
                let n = (!value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| value.parse::<u64>().ok())
                    .flatten()
                    .filter(|n| *n <= MAX_PARTITION)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or_else(|| invalid("`partition` must be a whole number from 0 to 63."))?;
                partition = Some(n);
            }
            "after" if after.is_none() => {
                let value = percent_decode(value)
                    .filter(|v| position_shape(v))
                    .ok_or_else(position_invalid)?;
                after = Some(value);
            }
            "consumer" | "partition" | "after" => {
                return Err(invalid("A query parameter can be given only once."));
            }
            _ => {
                return Err(invalid(
                    "`consumer`, `partition` and `after` are the only query parameters of the stream.",
                ));
            }
        }
    }
    let consumer = consumer.ok_or_else(|| invalid("`consumer` is required."))?;
    let partition = partition.ok_or_else(|| invalid("`partition` is required."))?;
    Ok((consumer, partition, after))
}

/// Whether `text` is a consumer name: `[a-z][a-z0-9_]{0,62}`.
fn consumer_name_shape(text: &str) -> bool {
    let mut chars = text.chars();
    text.len() <= 63
        && matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

async fn event_stream(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    // Read before the future: a `Request` body is not `Sync`, so it cannot be borrowed by it.
    let query = request.uri().query().unwrap_or("").to_owned();
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let (consumer, partition, after_query) =
            stream_query(&query).map_err(|e| fail(e, request_id))?;
        // `Last-Event-ID` is preferred over `after`.
        let after = match headers.get("last-event-id") {
            None => after_query,
            Some(raw) => Some(
                raw.to_str()
                    .ok()
                    .filter(|t| position_shape(t))
                    .map(str::to_owned)
                    .ok_or_else(|| fail(position_invalid(), request_id))?,
            ),
        };
        let limits = call.port.limits();
        let feed = call
            .port
            .open_stream(
                &call.ctx,
                StreamIn {
                    consumer,
                    partition,
                    after,
                },
            )
            .await
            .map_err(|e| fail(e, request_id))?;
        let lease = HeaderValue::from_str(feed.lease()).map_err(|_| {
            tracing::error!("the event stream port returned a lease that is not a header value");
            fail(ApiError::from_code(Code::HttpInternal), request_id)
        })?;
        let mut response = Response::new(Body::new(EventBody::new(feed, limits)));
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream; charset=utf-8"),
        );
        headers.insert(HeaderName::from_static(LEASE_HEADER), lease);
        headers.insert(
            HeaderName::from_static("x-accel-buffering"),
            HeaderValue::from_static("no"),
        );
        Ok(response)
    }
    .await;
    match result {
        Ok(mut response) => {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            response
        }
        Err(failure) => private_no_store(failure.into_response()),
    }
}

/// A feed step in progress: the feed moves into the future and comes back with its answer.
type Pending = Pin<Box<dyn std::future::Future<Output = (Box<dyn EventFeed>, FeedItem)> + Send>>;

/// An SSE response body: the port's feed, frame by frame, with the route's heartbeat and idle
/// limit around it. Dropping the body drops the feed (and so the consumer's lease).
struct EventBody {
    feed: Option<Box<dyn EventFeed>>,
    pending: Option<Pending>,
    heartbeat: Duration,
    max_idle: Duration,
    last_sent: Instant,
    last_event: Instant,
    done: bool,
}

impl EventBody {
    fn new(feed: Box<dyn EventFeed>, limits: StreamLimits) -> Self {
        let now = Instant::now();
        EventBody {
            feed: Some(feed),
            pending: None,
            heartbeat: limits.heartbeat,
            max_idle: limits.max_idle,
            last_sent: now,
            last_event: now,
            done: false,
        }
    }
}

impl HttpBody for EventBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let this = self.get_mut();
        loop {
            if this.done {
                return Poll::Ready(None);
            }
            if this.pending.is_none() {
                let Some(mut feed) = this.feed.take() else {
                    this.done = true;
                    return Poll::Ready(None);
                };
                this.pending = Some(Box::pin(async move {
                    let item = feed.next().await;
                    (feed, item)
                }));
            }
            let Some(future) = this.pending.as_mut() else {
                continue;
            };
            let (feed, item) = match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(ready) => ready,
            };
            this.pending = None;
            match item {
                FeedItem::Closed => {
                    this.done = true;
                    return Poll::Ready(None);
                }
                FeedItem::Frame(bytes) => {
                    this.feed = Some(feed);
                    let now = Instant::now();
                    this.last_sent = now;
                    this.last_event = now;
                    if bytes.is_empty() {
                        continue;
                    }
                    return Poll::Ready(Some(Ok(Frame::data(Bytes::from(bytes)))));
                }
                FeedItem::Idle => {
                    this.feed = Some(feed);
                    let now = Instant::now();
                    if now.duration_since(this.last_event) >= this.max_idle {
                        this.done = true;
                        this.feed = None;
                        return Poll::Ready(None);
                    }
                    if now.duration_since(this.last_sent) >= this.heartbeat {
                        this.last_sent = now;
                        return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(KEEPALIVE)))));
                    }
                    // Yield, so a feed that answers `Idle` at once cannot hold the executor.
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            }
        }
    }
}
