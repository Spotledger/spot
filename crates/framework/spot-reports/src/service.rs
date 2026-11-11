//! The native API's report service: the bridge between `POST /api/v1/report/{report}/run` and
//! the report engine.
//!
//! The contract's `run_report` lives on `spot_http`'s `ResourceService` (there is no separate
//! `ReportService` trait in `spot-http`), so this module provides two things:
//!
//! - [`ReportService`], the report-only trait with the contract's signature — what the report
//!   engine implements and a host can depend on directly; and
//! - [`ReportResourceService`], an adapter that implements the whole
//!   [`ResourceService`](spot_http::api::service::ResourceService) by delegating every
//!   document method to an inner service and answering `run_report` from the report engine.
//!   This is what the binary wires into the router, so the report arm needs no change to
//!   `spot-http`.
//!
//! It resolves the caller to a principal, finds the report the caller's roles admit (an
//! unknown report and one the roles do not admit answer the same `404 report.not_found`),
//! clamps the request's `limit` to the report's maximum, runs it and maps every `report.*`
//! error to the contract's code and status. The engine emits the telemetry timing line.

use std::sync::Arc;

use serde_json::Value as Json;
use spot_http::api::codec::{ApiDocument, DocumentPatchApi};
use spot_http::api::service::{
    ApiError, BoxFuture, Message, MethodOutcome, Page, QuerySpec, ReportResult, RequestCtx,
    ResourceService, WriteResult,
};
use spot_permissions::Principal;
use spot_storage_contract::{RecordId, Version};

use crate::error::{ReportError, codes};
use crate::registry::ReportRegistry;
use crate::runner::{ReportArgs, ReportRunner};

/// The report arm of the native API, with the contract's `run_report` signature.
pub trait ReportService: Send + Sync + 'static {
    /// Run a report with filters.
    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>>;
}

/// Resolves the HTTP caller to a permission principal. The real implementation validates the
/// bearer credential through the auth service; tests supply a fake. It decides no right.
pub trait PrincipalSource: Send + Sync {
    /// The principal for this request. An unauthenticated request resolves to an empty
    /// principal, which reads nothing.
    fn principal(&self, ctx: &RequestCtx) -> Principal;
}

/// A source that resolves every request to an empty principal (for wiring before auth lands).
#[derive(Clone, Copy, Debug, Default)]
pub struct AnonymousPrincipals;

impl PrincipalSource for AnonymousPrincipals {
    fn principal(&self, _ctx: &RequestCtx) -> Principal {
        Principal {
            user: String::new(),
            assignments: Vec::new(),
            is_system: false,
        }
    }
}

/// The report engine behind the native API.
pub struct ReportApiService {
    registry: Arc<ReportRegistry>,
    runner: Arc<ReportRunner>,
    principals: Arc<dyn PrincipalSource>,
}

impl ReportApiService {
    /// A service over its registry, runner and principal source.
    #[must_use]
    pub fn new(
        registry: Arc<ReportRegistry>,
        runner: Arc<ReportRunner>,
        principals: Arc<dyn PrincipalSource>,
    ) -> Self {
        ReportApiService {
            registry,
            runner,
            principals,
        }
    }

    async fn run_inner(
        &self,
        ctx: &RequestCtx,
        report: &str,
        filters: Json,
        limit: Option<u64>,
    ) -> Result<(ReportResult, Vec<Message>), ApiError> {
        let principal = self.principals.principal(ctx);
        let Some(registered) = self.registry.lookup_for(report, &principal) else {
            return Err(ReportError::new(
                codes::NOT_FOUND,
                format!("No report named `{report}` is available."),
            )
            .to_api_error());
        };
        let args = ReportArgs::from_json(&filters).map_err(|e| e.to_api_error())?;
        // The request's `limit` never exceeds the report's maximum; the engine clamps to the
        // default when the request sends none.
        let limit = limit.map(|l| l.min(u64::from(registered.compiled.max_rows)));
        let out = self
            .runner
            .run(&registered.compiled, &principal, &args, limit)
            .await
            .map_err(|e| e.to_api_error())?;
        Ok((out.result, out.messages))
    }
}

impl ReportService for ReportApiService {
    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>> {
        Box::pin(async move { self.run_inner(ctx, report, filters, limit).await })
    }
}

/// Implements the native API's whole `ResourceService` for the router: every document method
/// delegates to `inner` (the document engine), and `run_report` answers from the report
/// engine. This is the one place the report arm is wired in, so `spot-http` stays unchanged.
pub struct ReportResourceService {
    inner: Arc<dyn ResourceService>,
    reports: ReportApiService,
}

impl ReportResourceService {
    /// Wraps the document engine's resource service with the report engine.
    #[must_use]
    pub fn new(inner: Arc<dyn ResourceService>, reports: ReportApiService) -> Self {
        ReportResourceService { inner, reports }
    }
}

impl ResourceService for ReportResourceService {
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        self.inner.get(ctx, doctype, id)
    }

    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        self.inner.list(ctx, doctype, spec)
    }

    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        self.inner.query(ctx, doctype, spec)
    }

    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        document: &'a ApiDocument,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        self.inner.create(ctx, doctype, document)
    }

    fn update<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        patch: &'a DocumentPatchApi,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        self.inner.update(ctx, doctype, id, patch, expected)
    }

    fn delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<Vec<Message>, ApiError>> {
        self.inner.delete(ctx, doctype, id, expected)
    }

    fn post<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        self.inner.post(ctx, doctype, id, expected)
    }

    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        self.inner.cancel(ctx, doctype, id, expected)
    }

    fn correct<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        self.inner.correct(ctx, doctype, id, expected)
    }

    fn rename<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        self.inner.rename(ctx, doctype, id, new_name, expected)
    }

    fn call_method<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, Result<(MethodOutcome, Vec<Message>), ApiError>> {
        self.inner
            .call_method(ctx, doctype, id, method, args, expected)
    }

    fn call_function<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<Message>), ApiError>> {
        self.inner
            .call_function(ctx, app, module, function, args, via_get)
    }

    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>> {
        self.reports.run_report(ctx, report, filters, limit)
    }
}
