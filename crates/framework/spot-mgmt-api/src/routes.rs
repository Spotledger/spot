//! The read-side routes and the router.

use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use spot_mgmt_ops::{OpId, OperationSpec};

use crate::app::{AppState, Deps, Success, Trace, finish};
use crate::error::{MgmtCode, MgmtError};
use crate::invoke::{invoke, read_json};
use crate::lifecycle::CommandRecord;
use crate::ports::OperatorEntry;
use crate::view::{
    invalid_parameter, invocation_from_params, invocation_json, operation_json, parse_query,
    parse_risk, status_json,
};

/// The management API router: `/mgmt/v1/...`. Mount it on the management listener only, behind
/// [`crate::tls::serve`]: the router trusts the peer certificate that listener supplies.
pub fn router(deps: Deps) -> Router {
    let state = Arc::new(AppState::new(deps));
    Router::new()
        .route("/mgmt/v1/health", get(health))
        .route("/mgmt/v1/operations", get(list_operations))
        .route("/mgmt/v1/operations/{id}", get(get_operation))
        .route(
            "/mgmt/v1/operations/{id}/validate",
            post(validate_operation),
        )
        .route("/mgmt/v1/commands", post(invoke))
        .route("/mgmt/v1/commands/{id}", get(get_command))
        .route("/mgmt/v1/commands/{id}/cancel", post(cancel_command))
        .fallback(not_found)
        .with_state(state)
}

async fn health() -> Response {
    (
        StatusCode::OK,
        Json(json!({ "data": { "status": "live" } })),
    )
        .into_response()
}

async fn not_found(State(app): State<Arc<AppState>>) -> Response {
    let request_id = Trace::new(&app, "mgmt.not_found").request_id;
    MgmtError::new(MgmtCode::UnknownOperation).into_response_with(&request_id)
}

/// The spec when it exists and the operator may see it.
fn visible<'a>(
    app: &'a AppState,
    operator: &OperatorEntry,
    id: &str,
) -> Option<(OpId, &'a OperationSpec)> {
    let op_id = OpId::new(id)?;
    let spec = app.catalogue.get(&op_id)?;
    operator.scope.allows(id).then_some((op_id, spec))
}

fn unknown_operation() -> MgmtError {
    MgmtError::with_reason(MgmtCode::UnknownOperation, "mgmt.unknown_operation")
}

async fn list_operations(State(app): State<Arc<AppState>>, request: Request) -> Response {
    let mut trace = Trace::new(&app, "mgmt.operation.list");
    let (parts, _body) = request.into_parts();
    let result = list_inner(&app, &mut trace, &parts);
    finish(&app, trace, result).await
}

fn list_inner(app: &AppState, trace: &mut Trace, parts: &Parts) -> Result<Success, MgmtError> {
    let operator = app.authenticate(trace, parts)?;
    let query = parse_query(parts.uri.query());
    let risk = match query.get("risk") {
        Some(text) => Some(parse_risk(text).ok_or_else(|| MgmtError::parameter("risk"))?),
        None => None,
    };
    let limit = match query.get("limit") {
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=100).contains(n))
            .ok_or_else(|| MgmtError::parameter("limit"))?,
        None => 50,
    };
    let needle = query.get("q").map(|text| text.to_lowercase());
    if needle
        .as_ref()
        .is_some_and(|text| text.chars().count() > 100)
    {
        return Err(MgmtError::parameter("q"));
    }
    let cursor = query.get("cursor").map(String::as_str);
    let mut page: Vec<Value> = Vec::new();
    let mut next_cursor: Option<String> = None;
    for spec in app.catalogue.iter() {
        let id = spec.id.as_str();
        if cursor.is_some_and(|after| id <= after) || !operator.scope.allows(id) {
            continue;
        }
        if risk.is_some_and(|wanted| spec.risk != wanted) {
            continue;
        }
        if let Some(text) = &needle {
            let in_id = id.starts_with(text.as_str());
            let in_title = spec
                .title
                .to_lowercase()
                .split_whitespace()
                .any(|word| word.starts_with(text.as_str()));
            if !in_id && !in_title {
                continue;
            }
        }
        if page.len() == limit {
            next_cursor = page
                .last()
                .and_then(|last| last.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            break;
        }
        let kinds = app.approvals.required(spec);
        page.push(operation_json(spec, &kinds));
    }
    Ok(Success::ok(
        json!({ "data": page, "next_cursor": next_cursor }),
    ))
}

async fn get_operation(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let mut trace = Trace::new(&app, "mgmt.operation.get");
    let (parts, _body) = request.into_parts();
    let result = get_inner(&app, &mut trace, &parts, &id);
    finish(&app, trace, result).await
}

fn get_inner(
    app: &AppState,
    trace: &mut Trace,
    parts: &Parts,
    id: &str,
) -> Result<Success, MgmtError> {
    let operator = app.authenticate(trace, parts)?;
    let (op_id, spec) = visible(app, &operator, id).ok_or_else(unknown_operation)?;
    trace.operation = Some(op_id.as_str().to_owned());
    trace.risk = Some(spec.risk);
    let kinds = app.approvals.required(spec);
    Ok(Success::ok(json!({ "data": operation_json(spec, &kinds) })))
}

async fn validate_operation(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let mut trace = Trace::new(&app, "mgmt.operation.validate");
    let (parts, body) = request.into_parts();
    let result: Result<Success, MgmtError> = async {
        let operator = app.authenticate(&mut trace, &parts)?;
        let document = read_json(&parts, body).await?;
        let (op_id, spec) = visible(&app, &operator, &id).ok_or_else(unknown_operation)?;
        trace.operation = Some(op_id.as_str().to_owned());
        trace.risk = Some(spec.risk);
        let params = document
            .get("params")
            .ok_or_else(|| MgmtError::parameter("params"))?;
        let invocation = invocation_from_params(spec, &op_id, params)?;
        let validated = app
            .catalogue
            .validate(&invocation)
            .map_err(|problems| invalid_parameter(&problems))?;
        let kinds = app.approvals.required(spec);
        Ok(Success::ok(
            json!({ "data": invocation_json(&validated, spec.risk, &kinds) }),
        ))
    }
    .await;
    finish(&app, trace, result).await
}

/// Whether the operator may see (and cancel) this command.
fn may_see(operator: &OperatorEntry, record: &CommandRecord, cancel: bool) -> bool {
    record.principal == operator.principal
        || if cancel {
            operator.cancel_right
        } else {
            operator.audit_right
        }
}

async fn get_command(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let mut trace = Trace::new(&app, "mgmt.command.get");
    let (parts, _body) = request.into_parts();
    let result: Result<Success, MgmtError> = async {
        let operator = app.authenticate(&mut trace, &parts)?;
        let record = load_visible(&app, &operator, &id, false).await?;
        trace.command_id = Some(record.id.clone());
        trace.operation = Some(record.operation.clone());
        Ok(Success::ok(json!({ "data": status_json(&record) })))
    }
    .await;
    finish(&app, trace, result).await
}

fn unknown_command() -> MgmtError {
    MgmtError::with_reason(MgmtCode::UnknownCommand, "mgmt.unknown_command")
}

async fn load_visible(
    app: &AppState,
    operator: &OperatorEntry,
    id: &str,
    cancel: bool,
) -> Result<CommandRecord, MgmtError> {
    let record = app
        .commands
        .get(id)
        .await
        .map_err(|_| MgmtError::with_reason(MgmtCode::Internal, "mgmt.command_store_unavailable"))?
        .ok_or_else(unknown_command)?;
    if may_see(operator, &record, cancel) {
        Ok(record)
    } else {
        Err(unknown_command())
    }
}

async fn cancel_command(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let mut trace = Trace::new(&app, "mgmt.command.cancel");
    let (parts, _body) = request.into_parts();
    let result: Result<Success, MgmtError> = async {
        let operator = app.authenticate(&mut trace, &parts)?;
        let record = load_visible(&app, &operator, &id, true).await?;
        trace.command_id = Some(record.id.clone());
        trace.operation = Some(record.operation.clone());
        if !record.state.is_final() {
            // `Some(cancellable)` when the command is running here; the switch is flipped only
            // when its handler declared cooperative cancel.
            let supported = app.running.lock().ok().and_then(|running| {
                running.get(&record.id).map(|entry| {
                    if entry.cancellable {
                        let _ = entry.cancel.send(true);
                    }
                    entry.cancellable
                })
            });
            if supported == Some(false) {
                return Err(MgmtError::with_reason(
                    MgmtCode::RefusedByPolicy,
                    "mgmt.cancel_not_supported",
                ));
            }
        }
        let current = app
            .commands
            .get(&record.id)
            .await
            .ok()
            .flatten()
            .unwrap_or(record);
        Ok(Success::ok(json!({ "data": status_json(&current) })))
    }
    .await;
    finish(&app, trace, result).await
}
