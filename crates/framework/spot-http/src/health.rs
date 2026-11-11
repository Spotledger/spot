//! The health endpoints: liveness and readiness.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// One readiness check result.
#[derive(Debug, Clone)]
pub struct CheckResult {
    /// Whether the checked dependency is usable right now.
    pub ok: bool,
    /// Detail (for example an error text); shown for failing checks.
    pub detail: String,
}

impl CheckResult {
    /// A passing result without detail.
    pub fn ok() -> Self {
        CheckResult {
            ok: true,
            detail: String::new(),
        }
    }

    /// A failing result with detail.
    pub fn failed(detail: impl Into<String>) -> Self {
        CheckResult {
            ok: false,
            detail: detail.into(),
        }
    }
}

/// One readiness check. Checks run concurrently with a 2 second budget each.
pub trait ReadinessCheck: Send + Sync + 'static {
    /// Runs the check; it may await (a database probe, a queue ping).
    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>>;
    /// Stable check name, shown in the readiness report.
    fn name(&self) -> &str;
}

/// The readiness handler's shared state.
pub(crate) struct ReadyState {
    /// Configured checks.
    pub(crate) checks: Arc<[Arc<dyn ReadinessCheck>]>,
    /// Flipped while shutting down: readiness drains with the server (blueprint §2.2).
    pub(crate) shutting_down: Arc<AtomicBool>,
}

impl std::fmt::Debug for ReadyState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadyState")
            .field("checks", &self.checks.len())
            .field("shutting_down", &self.shutting_down.load(Ordering::Relaxed))
            .finish()
    }
}

/// Per-check budget, fixed by the server contract.
pub(crate) const READY_CHECK_TIMEOUT: Duration = Duration::from_secs(2);

/// `GET /api/health/live`: 200 with `{"status": "live"}`; no checks run.
pub(crate) async fn live_handler() -> Response {
    axum::Json(json!({ "status": "live" })).into_response()
}

/// `GET /api/health/ready`: every check passes → 200; anything fails or the site is shutting
/// down → 503 with one report per check (or an empty list while shutting down).
pub(crate) async fn ready_handler(State(state): State<Arc<ReadyState>>) -> Response {
    if state.shutting_down.load(Ordering::Relaxed) {
        return ready_response(StatusCode::SERVICE_UNAVAILABLE, json!([]));
    }
    let mut handles = Vec::with_capacity(state.checks.len());
    let mut names = Vec::with_capacity(state.checks.len());
    for check in state.checks.iter() {
        let check = Arc::clone(check);
        let name = check.name().to_owned();
        names.push(name.clone());
        handles.push(tokio::spawn(async move {
            let result = match tokio::time::timeout(READY_CHECK_TIMEOUT, check.check()).await {
                Ok(result) => result,
                Err(_) => CheckResult::failed("timed out after 2s"),
            };
            (name, result)
        }));
    }
    let mut all_ok = true;
    let mut checks = Vec::with_capacity(handles.len());
    for (name, handle) in names.into_iter().zip(handles) {
        let result = match handle.await {
            Ok((_, result)) => result,
            Err(join) => CheckResult::failed(format!("check task crashed: {join}")),
        };
        all_ok &= result.ok;
        checks.push(json!({
            "name": name,
            "ok": result.ok,
            "detail": result.detail,
        }));
    }
    let status = if all_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    ready_response(status, serde_json::Value::Array(checks))
}

/// Shared ready/not-ready body: `{"status": ..., "checks": [...]}`.
fn ready_response(status: StatusCode, checks: serde_json::Value) -> Response {
    let body = if status.is_success() {
        json!({ "status": "ready", "checks": checks })
    } else {
        json!({ "status": "not_ready", "checks": checks })
    };
    (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Must;
    use axum::body::to_bytes;

    fn state_with(checks: Vec<Arc<dyn ReadinessCheck>>, shutting_down: bool) -> Arc<ReadyState> {
        Arc::new(ReadyState {
            checks: Arc::from(checks),
            shutting_down: Arc::new(AtomicBool::new(shutting_down)),
        })
    }

    #[tokio::test]
    async fn ready_without_checks_is_ready() {
        let state = state_with(Vec::new(), false);
        let response = ready_handler(State(Arc::clone(&state))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 4_096).await.must();
        let body = serde_json::from_slice::<serde_json::Value>(&bytes).must();
        assert_eq!(
            body.pointer("/status").and_then(|v| v.as_str()),
            Some("ready")
        );
        assert!(
            body.pointer("/checks")
                .is_some_and(|v| v.as_array().is_some_and(std::vec::Vec::is_empty))
        );
    }

    #[tokio::test]
    async fn ready_during_shutdown_is_503_with_empty_checks() {
        let state = state_with(Vec::new(), true);
        let response = ready_handler(State(Arc::clone(&state))).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(response.into_body(), 4_096).await.must();
        let body = serde_json::from_slice::<serde_json::Value>(&bytes).must();
        assert_eq!(
            body.pointer("/status").and_then(|v| v.as_str()),
            Some("not_ready")
        );
        assert!(
            body.pointer("/checks")
                .is_some_and(|v| v.as_array().is_some_and(std::vec::Vec::is_empty))
        );
    }
}
