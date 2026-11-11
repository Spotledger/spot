//! `EngineResources` — implements `ResourceService` by adapting the `Engine` to the HTTP layer.

use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::prelude::{Clock, RecordId, Version};
use spot_http::api::codec::{ApiDocument, DocumentPatchApi};
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, BoxFuture, Message as ApiMessage, MethodOutcome, Page, QuerySpec, RequestCtx,
    ResourceService, WriteResult,
};
use spot_meta::EffectiveMeta;

use crate::conversion::{
    api_error, messages_of, to_api_document, to_document, to_method_outcome, to_patch,
    to_write_result,
};
use crate::query::{QueryService, list};
use crate::resources::Delegates;
use crate::util::build_invocation;
use spot_app_api::context::Actor;

/// Adapter implementing `ResourceService` for the `Engine`.
///
/// Maps HTTP layer types (`RequestCtx`, `ApiDocument`, `DocumentPatchApi`, `QuerySpec`)
/// to the app API types (`Invocation`, `Document`, `DocumentPatch`, storage query IR)
/// and delegates to the engine.
pub struct EngineResources<S: DocumentService> {
    engine: Arc<S>,
    meta: Arc<EffectiveMeta>,
    clock: Arc<dyn Clock>,
    actor: Arc<ActorResolver>,
    delegates: Delegates,
}

/// Caller-provided credential resolution; this adapter never authenticates or invents users.
pub type ActorResolver = dyn Fn(&RequestCtx) -> Result<Actor, ApiError> + Send + Sync;

impl<S: DocumentService> EngineResources<S> {
    /// Create a new `EngineResources` wrapping the given engine.
    pub fn new(
        engine: Arc<S>,
        meta: Arc<EffectiveMeta>,
        clock: Arc<dyn Clock>,
        actor: Arc<ActorResolver>,
    ) -> Self {
        Self {
            engine,
            meta,
            clock,
            actor,
            delegates: Delegates::none(),
        }
    }

    /// Installs the delegates that serve `call_function` and `run_report` (WP-PRD-01.T29). An
    /// operation with no delegate answers exactly as it did before.
    #[must_use]
    pub fn with_delegates(mut self, delegates: Delegates) -> Self {
        self.delegates = delegates;
        self
    }

    /// Get the effective metadata for a DocType.
    fn get_meta(&self, doctype: &str) -> Result<Arc<spot_app_api::meta::DocTypeDef>, ApiError> {
        self.meta.doctype(doctype).ok_or_else(|| {
            ApiError::new(
                Code::MetaUnknownDoctype,
                format!("The DocType `{doctype}` does not exist."),
            )
        })
    }

    /// Build an `Invocation` from the HTTP request context.
    fn build_invocation(&self, ctx: &RequestCtx) -> Result<Invocation, ApiError> {
        build_invocation(ctx, self.clock.as_ref(), (self.actor)(ctx)?)
    }
}

impl<S: DocumentService + QueryService + 'static> ResourceService for EngineResources<S> {
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            let outcome = self
                .engine
                .load(&inv, doctype, id)
                .await
                .map_err(api_error)?;
            Ok(to_write_result(outcome, to_api_document))
        })
    }

    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            list(self.engine.as_ref(), &self.meta, &inv, doctype, spec).await
        })
    }

    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        // list and query are the same operation at the engine level
        self.list(ctx, doctype, spec)
    }

    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        document: &'a ApiDocument,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let def = self.get_meta(doctype)?;
            let inv = self.build_invocation(ctx)?;

            let doc = to_document(document, &def);

            let outcome = self.engine.create(&inv, doc).await.map_err(api_error)?;
            Ok(to_write_result(outcome, to_api_document))
        })
    }

    fn update<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        patch: &'a DocumentPatchApi,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            let app_patch = to_patch(patch);

            let outcome = self
                .engine
                .update(&inv, doctype, id, app_patch, expected)
                .await
                .map_err(api_error)?;
            Ok(to_write_result(outcome, to_api_document))
        })
    }

    fn delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<Vec<ApiMessage>, ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            let outcome = self
                .engine
                .delete(&inv, doctype, id, expected)
                .await
                .map_err(api_error)?;
            Ok(messages_of(outcome.messages))
        })
    }

    fn post<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            let outcome = self
                .engine
                .post(&inv, doctype, id, expected)
                .await
                .map_err(api_error)?;
            Ok(to_write_result(outcome, to_api_document))
        })
    }

    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            let outcome = self
                .engine
                .cancel(&inv, doctype, id, expected)
                .await
                .map_err(api_error)?;
            Ok(to_write_result(outcome, to_api_document))
        })
    }

    fn correct<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            let outcome = self
                .engine
                .correct(&inv, doctype, id, expected)
                .await
                .map_err(api_error)?;
            Ok(to_write_result(outcome, to_api_document))
        })
    }

    fn rename<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            let outcome = self
                .engine
                .rename(&inv, doctype, id, new_name, expected)
                .await
                .map_err(api_error)?;
            Ok(to_write_result(outcome, to_api_document))
        })
    }

    fn call_method<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, Result<(MethodOutcome, Vec<ApiMessage>), ApiError>> {
        Box::pin(async move {
            let inv = self.build_invocation(ctx)?;
            let outcome = self
                .engine
                .call_method(&inv, doctype, id, method, args, expected)
                .await
                .map_err(api_error)?;
            Ok(to_method_outcome(outcome))
        })
    }

    fn call_function<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<ApiMessage>), ApiError>> {
        if let Some(delegate) = &self.delegates.function {
            // Fail closed: a delegate is reached only by a request whose credential resolves.
            if let Err(error) = (self.actor)(ctx) {
                return Box::pin(async move { Err(error) });
            }
            return delegate.call(ctx, app, module, function, args, via_get);
        }
        Box::pin(async move {
            Err(ApiError::new(
                Code::MethodNotFound,
                "Function calls are not yet implemented in the engine.".to_owned(),
            ))
        })
    }

    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(spot_http::api::service::ReportResult, Vec<ApiMessage>), ApiError>>
    {
        if let Some(delegate) = &self.delegates.report {
            // Fail closed: a delegate is reached only by a request whose credential resolves.
            if let Err(error) = (self.actor)(ctx) {
                return Box::pin(async move { Err(error) });
            }
            return delegate.run(ctx, report, filters, limit);
        }
        Box::pin(async move {
            Err(ApiError::new(
                Code::ReportNotFound,
                "Reports are not yet implemented in the engine.".to_owned(),
            ))
        })
    }
}
