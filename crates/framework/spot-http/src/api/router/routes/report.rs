//! `GET /api/v1/report` and `GET /api/v1/report/{report}` (`report_list` and `report_describe`,
//! WP-PLT-14.T2): the catalogue of reports the signed-in user may see, and one report described
//! for that user.
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request (the path's report name, no body) and refuse a
//! malformed one before the port is asked. They never decide who the caller is, which reports
//! the caller's roles admit or which columns the caller may report on: the port answers for the
//! caller its credential names, and the routes pass it no user and no role. An unknown report
//! and one the caller may not see are the port's one answer, `404 report.not_found`.
//!
//! Every response is `private, no-store`: a catalogue is per caller.
//!
//! `report_run` (`POST /api/v1/report/{report}/run`) is hand-wired in the router and served by
//! the engine behind `ResourceService::run_report`; it is not in this group.
//!
//! **Registered** under the registry's standard names (`SERVES`, `routes`, `required_ports`), so
//! the served set, the conformance walk and the registry tests know both operations. The registry
//! mounts the group when the router is built over a [`Ports`](crate::api::Ports) that holds the
//! catalogue port. The `spot-cli` host does not hand its report port to the router's `Ports`
//! (that is `host.rs`, not owned by WP-PLT-14.T4), so it still mounts the group itself through
//! [`registration`]: the registry skips the group there (port absent) and the host's mount is the
//! only one. When the host provides the port to the router, the host's own mount goes.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde_json::{Map, Value as Json, json};
use spot_storage_contract::Name;

use super::{Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::report::{
    ParameterOptionsOut, ReportCatalogPort, ReportDescriptionOut, ReportParameterOut,
    ReportSummaryOut,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail};

/// The operation ids this group serves once registered.
pub const SERVES: &[&str] = &["report_list", "report_describe"];

/// The ports this group needs: the catalogue port, whose adapter decides identity, the roles
/// that admit a report and the columns the caller may report on. A group without it is not
/// mounted at all.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn ReportCatalogPort>()];
    &KEYS
}

/// The report routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/report", get(report_list))
        .route("/api/v1/report/{report}", get(report_describe))
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "report",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

/// The port and the request context, or the answer to give without them.
struct Call {
    port: std::sync::Arc<dyn ReportCatalogPort>,
    ctx: RequestCtx,
}

fn call(state: &RouteState, site: &SiteContext, headers: &HeaderMap) -> Result<Call, Failure> {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn ReportCatalogPort>() else {
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

fn summary_json(summary: &ReportSummaryOut) -> Json {
    json!({
        "name": summary.name,
        "label": summary.label,
        "app": summary.app,
        "base_doctype": summary.base_doctype,
    })
}

fn parameter_json(parameter: &ReportParameterOut) -> Json {
    let mut map = Map::new();
    map.insert("name".to_owned(), Json::from(parameter.name.clone()));
    map.insert("label".to_owned(), Json::from(parameter.label.clone()));
    map.insert("type".to_owned(), Json::from(parameter.kind.clone()));
    map.insert("required".to_owned(), Json::Bool(parameter.required));
    if let Some(default) = &parameter.default {
        map.insert("default".to_owned(), default.clone());
    }
    match &parameter.options {
        Some(ParameterOptionsOut::Values(values)) => {
            map.insert(
                "options".to_owned(),
                json!({"source": "values", "values": values}),
            );
        }
        Some(ParameterOptionsOut::Link(doctype)) => {
            map.insert(
                "options".to_owned(),
                json!({"source": "link", "doctype": doctype}),
            );
        }
        None => {}
    }
    Json::Object(map)
}

fn description_json(report: &ReportDescriptionOut) -> Json {
    json!({
        "name": report.name,
        "label": report.label,
        "app": report.app,
        "base_doctype": report.base_doctype,
        "parameters": report.parameters.iter().map(parameter_json).collect::<Vec<_>>(),
        "columns": report.columns.iter().map(|c| json!({
            "name": c.name,
            "label": c.label,
            "type": c.field_type.as_str(),
        })).collect::<Vec<_>>(),
        "max_rows": report.max_rows,
    })
}

async fn report_list(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        let reports = call
            .port
            .list(&call.ctx)
            .await
            .map_err(|e| fail(e, request_id))?;
        let data: Vec<Json> = reports.iter().map(summary_json).collect();
        Ok((StatusCode::OK, axum::Json(json!({ "data": data }))).into_response())
    }
    .await;
    done(result)
}

async fn report_describe(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(report): Path<String>,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let call = call(&state, &site, &headers)?;
        // A name that cannot be a report's is a malformed request, not a lookup.
        if Name::new(&report).is_err() {
            return Err(fail(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    "A report name must be a storage name.",
                ),
                request_id,
            ));
        }
        let described = call
            .port
            .describe(&call.ctx, &report)
            .await
            .map_err(|e| fail(e, request_id))?;
        Ok((
            StatusCode::OK,
            axum::Json(json!({ "data": description_json(&described) })),
        )
            .into_response())
    }
    .await;
    done(result)
}
