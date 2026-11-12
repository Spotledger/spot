//! The router, the request context and the access log.
//!
//! One middleware wraps every route: it gives the request an id (carried by the error envelope
//! and the `X-Request-Id` header), notes the source address, counts the status and writes one
//! structured access-log line per request: time, id, method, path, status, duration and address.
//! A query string, a header and a body never reach the log, so no key, token or payload can.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::json;

use crate::admin;
use crate::auth::ReqCtx;
use crate::error::{ApiError, REQUEST_ID};
use crate::handlers::{self, MULTIPART_OVERHEAD};
use crate::http::set_header;
use crate::ops::clip;
use crate::service::Service;

async fn context(
    State(service): State<Arc<Service>>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map_or_else(|| "local".to_owned(), |info| info.0.ip().to_string());
    let method = request.method().to_string();
    let path = clip(request.uri().path(), 200);
    request
        .extensions_mut()
        .insert(ReqCtx { addr: addr.clone() });
    let started = Instant::now();
    let mut response = REQUEST_ID
        .scope(request_id.clone(), next.run(request))
        .await;
    set_header(&mut response, "x-request-id", &request_id);
    let status = response.status().as_u16();
    service.core.metrics.count_status(status);
    let milliseconds = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    eprintln!(
        "{}",
        json!({
            "ts": service.core.clock.now(),
            "request_id": request_id,
            "method": method,
            "path": path,
            "status": status,
            "ms": milliseconds,
            "addr": addr,
        })
    );
    response
}

async fn no_route() -> ApiError {
    ApiError::not_found("no such route")
}

async fn wrong_method() -> ApiError {
    ApiError::server(
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        crate::error::codes::METHOD_NOT_ALLOWED,
        "this route does not take that method",
    )
}

/// The whole HTTP surface of the store: the contract's operations under `/store/v1`, the operator
/// routes under `/admin/v1`, `/health` and `/metrics`.
pub fn router(service: Arc<Service>) -> Router {
    let upload_cap = service
        .core
        .config
        .limits
        .max_bundle_bytes
        .saturating_add(MULTIPART_OVERHEAD);
    let upload_cap = usize::try_from(upload_cap).unwrap_or(usize::MAX);
    Router::new()
        .route("/store/v1/apps", get(handlers::app_search))
        .route("/store/v1/apps/{name}", get(handlers::app_get))
        .route(
            "/store/v1/apps/{name}/versions",
            post(handlers::app_publish).layer(DefaultBodyLimit::max(upload_cap)),
        )
        .route(
            "/store/v1/apps/{name}/versions/{version}",
            get(handlers::app_version_get),
        )
        .route(
            "/store/v1/apps/{name}/versions/{version}/bundle",
            get(handlers::app_bundle_download),
        )
        .route(
            "/store/v1/apps/{name}/versions/{version}/yank",
            post(handlers::app_version_yank),
        )
        .route("/store/v1/revocations", get(handlers::revocations_get))
        .route("/store/v1/reviews/{id}", get(handlers::review_get))
        .route("/health", get(handlers::health))
        .route("/metrics", get(admin::metrics))
        .route("/admin/v1/publishers/{id}/keys", post(admin::issue_key))
        .route(
            "/admin/v1/publishers/{id}/revoke",
            post(admin::revoke_publisher),
        )
        .route(
            "/admin/v1/publisher-keys/{key_id}/revoke",
            post(admin::revoke_publisher_key),
        )
        .route(
            "/admin/v1/signing-keys/{key_id}/revoke",
            post(admin::revoke_signing_key),
        )
        .route(
            "/admin/v1/bundles/{digest}/revoke",
            post(admin::revoke_bundle),
        )
        .route(
            "/admin/v1/apps/{name}/versions/{version}/approve",
            post(admin::approve),
        )
        .route(
            "/admin/v1/apps/{name}/versions/{version}/reject",
            post(admin::reject),
        )
        .route(
            "/admin/v1/apps/{name}/versions/{version}/withdraw",
            post(admin::withdraw),
        )
        .route("/admin/v1/reviews/{id}/rerun", post(admin::rerun_review))
        .route("/admin/v1/audit", get(admin::read_audit))
        .fallback(no_route)
        .method_not_allowed_fallback(wrong_method)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&service),
            context,
        ))
        .with_state(service)
}

/// Serves `service` on `listener` until `shutdown` completes, then drains in-flight requests.
///
/// # Errors
/// The listener's own I/O error.
pub async fn serve(
    service: Arc<Service>,
    listener: tokio::net::TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(
        listener,
        router(service).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}
