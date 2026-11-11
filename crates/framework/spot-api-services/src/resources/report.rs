//! The delegate behind `run_report` (`POST /api/v1/report/{report}`).

use serde_json::Value as Json;
use spot_http::api::service::{ApiError, BoxFuture, Message as ApiMessage, ReportResult, RequestCtx};

/// Serves `ResourceService::run_report` for an `EngineResources`.
///
/// The delegate runs as the calling user: it receives the request's own [`RequestCtx`] and
/// nothing wider. A delegate that cannot serve a report returns an [`ApiError`]; it never
/// answers success for work it did not do.
pub trait ReportDelegate: Send + Sync {
    /// Runs `report` with `filters`, cut at `limit` rows when one is given.
    fn run<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<ApiMessage>), ApiError>>;
}
