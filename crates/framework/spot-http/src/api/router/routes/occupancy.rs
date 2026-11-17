//! `/api/v1/occupancy/*` (contract 0.23.0, WP-CON-03.T52): `occupancy_claim`,
//! `occupancy_confirm`, `occupancy_release` and `occupancy_availability` over an
//! [`OccupancyPort`].
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request (the body's keys and types, the instants, the bounds
//! the contract and the port's limits state, the characters of an id) and refuse a malformed one
//! before the port is asked. They never decide who the caller is, who holds an interval, the
//! create, confirm, cancel or read right, or partition ownership: the port answers for the
//! caller its credential names. **The holder of a claim is the caller**: the body has no holder
//! member, and a body that names one (or any member the contract does not list) is
//! `400 api.invalid_request`, never ignored.
//!
//! A conflict answers `409 occupancy.conflict` with the contract's fixed text; nothing of the
//! other holder, interval or expiry is in it. A port error outside the contract's codes is
//! `500 http.internal` with no detail. Every response is `private, no-store`.
//!
//! # Idempotency
//!
//! `occupancy_claim` and `occupancy_confirm` require an `Idempotency-Key` (1 to 128 visible ASCII
//! characters, else `400 api.invalid_request`); the route passes it in `ctx.idempotency_key` and
//! the port replays the first answer. `occupancy_release` takes none: a repeat is a no-op.

use std::collections::HashMap;

use axum::Router;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::{Map, Value as Json, json};
use spot_storage_contract::Name;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

use super::{Registration, RouteState};
use crate::api::codec::{parse_datetime, parse_record_id};
use crate::api::ports::PortKey;
use crate::api::ports::occupancy::{
    AvailabilityOut, CONTRACT_MAX_TTL_SECONDS, ClaimIn, HoldOut, MAX_WINDOW_DAYS, OccupancyError,
    OccupancyPort,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, ErrorCode, RequestCtx};
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail, take_body};

/// The operation ids this group serves.
pub const SERVES: &[&str] = &[
    "occupancy_claim",
    "occupancy_confirm",
    "occupancy_release",
    "occupancy_availability",
];

/// The ports this group needs: the occupancy port, whose adapter decides identity, the rights,
/// partition ownership, limits and audit. A group without it is not mounted at all.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn OccupancyPort>()];
    &KEYS
}

/// The occupancy routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/occupancy/{entity}/holds", post(occupancy_claim))
        .route(
            "/api/v1/occupancy/{entity}/holds/{id}/confirm",
            post(occupancy_confirm),
        )
        .route(
            "/api/v1/occupancy/{entity}/holds/{id}",
            delete(occupancy_release),
        )
        .route(
            "/api/v1/occupancy/{entity}/availability",
            get(occupancy_availability),
        )
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "occupancy",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

const MAX_UNIT: usize = 200;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn entity_ok(entity: &str) -> Result<(), ApiError> {
    if entity.len() <= 200 && Name::new(entity).is_ok() {
        Ok(())
    } else {
        Err(invalid("The occupancy entity must be a storage name."))
    }
}

fn unit_ok(unit: &str) -> bool {
    !unit.is_empty() && unit.chars().count() <= MAX_UNIT && !unit.chars().any(char::is_control)
}

fn hold_id_ok(id: &str) -> Result<(), ApiError> {
    if parse_record_id(id).is_some() {
        Ok(())
    } else {
        Err(invalid("A hold id must be a record id."))
    }
}

/// The `Idempotency-Key` header: required when `required`, else optional; 1 to 128 visible ASCII.
fn key_of(headers: &HeaderMap, required: bool) -> Result<Option<String>, ApiError> {
    let Some(raw) = headers.get("idempotency-key") else {
        return if required {
            Err(invalid("This operation needs an Idempotency-Key header."))
        } else {
            Ok(None)
        };
    };
    let bad = || invalid("The Idempotency-Key header must be 1 to 128 visible ASCII characters.");
    let key = raw.to_str().map_err(|_| bad())?;
    if key.is_empty() || key.len() > 128 || !key.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(bad());
    }
    Ok(Some(key.to_owned()))
}

fn instant(value: &Json, what: &str) -> Result<OffsetDateTime, ApiError> {
    value
        .as_str()
        .filter(|s| s.len() <= 40)
        .and_then(parse_datetime)
        .ok_or_else(|| invalid(format!("`{what}` must be an RFC 3339 instant.")))
}

fn claim_of(body: &Json, port: &dyn OccupancyPort) -> Result<ClaimIn, ApiError> {
    let map = body
        .as_object()
        .ok_or_else(|| invalid("The request body must be a JSON object."))?;
    // No `holder` and no other member: the holder is the caller, never a field of the body.
    if map
        .keys()
        .any(|k| !["unit", "start", "end", "ttl_seconds"].contains(&k.as_str()))
    {
        return Err(invalid(
            "The request body has a field this operation does not accept.",
        ));
    }
    let unit = map
        .get("unit")
        .and_then(Json::as_str)
        .filter(|u| unit_ok(u))
        .ok_or_else(|| invalid("`unit` must be 1 to 200 characters of text."))?;
    let start = instant(
        map.get("start")
            .ok_or_else(|| invalid("The request body needs `start`."))?,
        "start",
    )?;
    let end = instant(
        map.get("end")
            .ok_or_else(|| invalid("The request body needs `end`."))?,
        "end",
    )?;
    let limits = port.limits();
    if end <= start || end - start > limits.max_span {
        return Err(ApiError::from_code(Code::OccupancyInvalidRange));
    }
    let ttl_seconds = match map.get("ttl_seconds") {
        None => None,
        Some(value) => {
            let ttl = value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| (1..=CONTRACT_MAX_TTL_SECONDS).contains(n))
                .ok_or_else(|| {
                    invalid(format!(
                        "`ttl_seconds` must be a whole number from 1 to {CONTRACT_MAX_TTL_SECONDS}."
                    ))
                })?;
            if ttl > limits.max_ttl_seconds {
                return Err(ApiError::from_code(Code::OccupancyTtlTooLong));
            }
            Some(ttl)
        }
    };
    Ok(ClaimIn {
        unit: unit.to_owned(),
        start,
        end,
        ttl_seconds,
    })
}

/// The codes a port may answer these operations with; any other error is `http.internal`.
fn contract_code(code: Code) -> bool {
    matches!(
        code,
        Code::ApiInvalidRequest
            | Code::ApiIdempotencyConflict
            | Code::ApiRateLimited
            | Code::AuthRequired
            | Code::PermissionDenied
            | Code::DocumentNotFound
            | Code::PartitionNotOwner
            | Code::OccupancyConflict
            | Code::OccupancyHoldExpired
            | Code::OccupancyInvalidRange
            | Code::OccupancyTtlTooLong
            | Code::OccupancyNotHolder
            | Code::ServerUnavailable
            | Code::HttpInternal
    )
}

/// The port's error as the response gives it. The typed refusals carry the contract's fixed
/// text and nothing of the other party; an error outside the contract's codes is internal.
fn mapped(error: OccupancyError) -> ApiError {
    match error {
        OccupancyError::Conflict => ApiError::from_code(Code::OccupancyConflict),
        OccupancyError::Expired => ApiError::from_code(Code::OccupancyHoldExpired),
        OccupancyError::NotHolder => ApiError::from_code(Code::OccupancyNotHolder),
        OccupancyError::InvalidRange => ApiError::from_code(Code::OccupancyInvalidRange),
        OccupancyError::TtlTooLong => ApiError::from_code(Code::OccupancyTtlTooLong),
        OccupancyError::NotOwner => ApiError::from_code(Code::PartitionNotOwner),
        OccupancyError::Api(api) => {
            if matches!(&api.code, ErrorCode::Registry(code) if contract_code(*code)) {
                api
            } else {
                ApiError::from_code(Code::HttpInternal)
            }
        }
    }
}

struct Call {
    port: std::sync::Arc<dyn OccupancyPort>,
    ctx: RequestCtx,
}

fn call(
    state: &RouteState,
    site: &SiteContext,
    headers: &HeaderMap,
    key: Option<bool>,
) -> Result<Call, Failure> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn OccupancyPort>() else {
        return Err(fail(ApiError::from_code(Code::HttpInternal), request_id));
    };
    let key = match key {
        Some(required) => key_of(headers, required).map_err(|e| fail(e, request_id))?,
        None => None,
    };
    let ctx = build_ctx(site, headers, key);
    if ctx.credential.is_none() {
        return Err(fail(ApiError::from_code(Code::AuthRequired), request_id));
    }
    Ok(Call { port, ctx })
}

type Handled = Result<Response, Failure>;

fn done(result: Handled) -> Response {
    let mut response = match result {
        Ok(response) => response,
        Err(failure) => failure.into_response(),
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

fn text(at: OffsetDateTime) -> String {
    at.to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn hold_json(hold: &HoldOut) -> Json {
    let mut map = Map::new();
    map.insert("id".to_owned(), json!(hold.id));
    map.insert("unit".to_owned(), json!(hold.unit));
    map.insert("start".to_owned(), json!(text(hold.start)));
    map.insert("end".to_owned(), json!(text(hold.end)));
    map.insert("state".to_owned(), json!(hold.state.as_str()));
    map.insert(
        "expires_at".to_owned(),
        hold.expires_at.map_or(Json::Null, |at| json!(text(at))),
    );
    map.insert(
        "holder".to_owned(),
        json!(hold.holder.chars().take(120).collect::<String>()),
    );
    json!({"data": Json::Object(map)})
}

fn availability_json(out: &AvailabilityOut) -> Json {
    let intervals: Vec<Json> = out
        .intervals
        .iter()
        .take(1000)
        .map(|i| {
            let mut map = Map::new();
            map.insert("start".to_owned(), json!(text(i.start)));
            map.insert("end".to_owned(), json!(text(i.end)));
            map.insert("status".to_owned(), json!(i.status.as_str()));
            if let Some(holder) = &i.holder {
                map.insert(
                    "holder".to_owned(),
                    json!(holder.chars().take(120).collect::<String>()),
                );
            }
            Json::Object(map)
        })
        .collect();
    json!({"data": {
        "unit": out.unit,
        "from": text(out.from),
        "to": text(out.to),
        "intervals": intervals,
    }})
}

async fn occupancy_claim(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(entity): Path<String>,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, Some(true))?;
        entity_ok(&entity).map_err(|e| fail(e, request_id))?;
        let body = take_body(request, true, request_id)
            .await?
            .unwrap_or(Json::Null);
        let claim = claim_of(&body, call.port.as_ref()).map_err(|e| fail(e, request_id))?;
        let hold = call
            .port
            .claim(&call.ctx, &entity, claim)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok((StatusCode::CREATED, axum::Json(hold_json(&hold))).into_response())
    }
    .await;
    done(result)
}

async fn occupancy_confirm(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((entity, id)): Path<(String, String)>,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, Some(true))?;
        entity_ok(&entity).map_err(|e| fail(e, request_id))?;
        hold_id_ok(&id).map_err(|e| fail(e, request_id))?;
        let hold = call
            .port
            .confirm(&call.ctx, &entity, &id)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok((StatusCode::OK, axum::Json(hold_json(&hold))).into_response())
    }
    .await;
    done(result)
}

async fn occupancy_release(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((entity, id)): Path<(String, String)>,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, None)?;
        entity_ok(&entity).map_err(|e| fail(e, request_id))?;
        hold_id_ok(&id).map_err(|e| fail(e, request_id))?;
        call.port
            .release(&call.ctx, &entity, &id)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    done(result)
}

async fn occupancy_availability(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(entity): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers, None)?;
        entity_ok(&entity).map_err(|e| fail(e, request_id))?;
        if query
            .keys()
            .any(|k| !["unit", "from", "to"].contains(&k.as_str()))
        {
            return Err(fail(
                invalid("This operation does not accept that query parameter."),
                request_id,
            ));
        }
        let unit = query
            .get("unit")
            .filter(|u| unit_ok(u))
            .ok_or_else(|| fail(invalid("`unit` is required."), request_id))?;
        let parse = |key: &str| {
            query
                .get(key)
                .filter(|s| s.len() <= 40)
                .and_then(|s| parse_datetime(s))
                .ok_or_else(|| {
                    fail(
                        invalid(format!("`{key}` must be an RFC 3339 instant.")),
                        request_id,
                    )
                })
        };
        let from = parse("from")?;
        let to = parse("to")?;
        if to <= from || to - from > Duration::days(MAX_WINDOW_DAYS) {
            return Err(fail(
                ApiError::from_code(Code::OccupancyInvalidRange),
                request_id,
            ));
        }
        let out = call
            .port
            .availability(&call.ctx, &entity, unit, from, to)
            .await
            .map_err(|e| fail(mapped(e), request_id))?;
        Ok((StatusCode::OK, axum::Json(availability_json(&out))).into_response())
    }
    .await;
    done(result)
}
