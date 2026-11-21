//! How a script's host calls reach documents: one small port, two implementations.
//!
//! `host.get`, `host.query`, `host.insert`, `host.update` and `host.call` are serviced by
//! [`crate::host`] through a [`ScriptDocumentAccess`]. A document-event script is serviced by
//! the step's own `HookContext` ([`HookAccess`], the behaviour T1 shipped, moved behind the
//! port). An API script or a scheduled script has no hook context: it is serviced by the app
//! API's `DocumentService` acting as one named invocation ([`ServiceAccess`], opened by a
//! [`ScriptAccessFactory`]). Either way the invocation's actor decides what the script may read
//! and write, so a script never has more rights than the user it runs as.

use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{Query, RecordId, Rows, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass, HookContext,
    Invocation,
};

use crate::codes;
use crate::host::MethodCaller;

/// The document operations a script's host calls need, as one actor.
///
/// Every method takes `&mut self`: an implementation is one run's access, never shared between
/// runs, so no state of one run can reach another.
pub trait ScriptDocumentAccess: Send {
    /// Who is acting, and when.
    fn invocation(&self) -> &Invocation;

    /// Whether `host.call` can be dispatched. When false the call is refused as `unsupported`
    /// before [`ScriptDocumentAccess::call`] is reached.
    fn can_call(&self) -> bool {
        true
    }

    /// A DocType's metadata.
    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>>;

    /// Loads a document as the actor (the permission check is the actor's).
    fn get<'a>(&'a mut self, doctype: &'a str, id: RecordId) -> BoxFuture<'a, AppResult<Document>>;

    /// Runs a query as the actor.
    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>>;

    /// Creates a document through its create lifecycle.
    fn insert(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>>;

    /// Updates a document through its update lifecycle.
    fn update<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>>;

    /// Calls a whitelisted method on a document as the actor.
    fn call<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
    ) -> BoxFuture<'a, AppResult<Json>>;
}

/// The access of a document-event script: the step's own context, so the lifecycle's phase
/// rules and authorizer apply exactly as they did before the port existed.
pub struct HookAccess<'c> {
    ctx: &'c mut dyn HookContext,
    caller: Option<&'c dyn MethodCaller>,
}

impl<'c> HookAccess<'c> {
    /// Access through `ctx`, dispatching `host.call` through `caller` when there is one.
    pub fn new(ctx: &'c mut dyn HookContext, caller: Option<&'c dyn MethodCaller>) -> Self {
        HookAccess { ctx, caller }
    }
}

impl ScriptDocumentAccess for HookAccess<'_> {
    fn invocation(&self) -> &Invocation {
        self.ctx.invocation()
    }

    fn can_call(&self) -> bool {
        self.caller.is_some()
    }

    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.ctx.meta(doctype)
    }

    fn get<'a>(&'a mut self, doctype: &'a str, id: RecordId) -> BoxFuture<'a, AppResult<Document>> {
        self.ctx.load(doctype, id)
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        self.ctx.query(query)
    }

    fn insert(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        self.ctx.create(doc)
    }

    fn update<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.ctx.update(doctype, id, patch, expected)
    }

    fn call<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        let invocation = self.ctx.invocation().clone();
        let caller = self.caller;
        Box::pin(async move {
            match caller {
                Some(caller) => caller.call(&invocation, doctype, id, method, args).await,
                None => Err(AppError::new(
                    ErrorClass::Internal,
                    codes::STORE_FAILED,
                    "this host offers no whitelisted-method dispatch",
                )),
            }
        })
    }
}

/// Runs a query as an invocation's actor. The app API's `DocumentService` has no query, so a
/// host supplies this (the same permission-filtered path `HookContext::query` takes).
pub trait ScriptQueries: Send + Sync {
    /// Runs `query` as `inv`'s actor; the rows are permission-filtered.
    fn query<'a>(&'a self, inv: &'a Invocation, query: &'a Query)
    -> BoxFuture<'a, AppResult<Rows>>;
}

/// Opens the access one run uses, acting as the invocation's actor.
pub trait ScriptAccessFactory: Send + Sync {
    /// A fresh access for `invocation`. Nothing is shared with any other run.
    fn open(&self, invocation: Invocation) -> Box<dyn ScriptDocumentAccess>;
}

/// The access of an API or scheduled script: the document service as one invocation.
pub struct ServiceAccess {
    docs: Arc<dyn DocumentService>,
    queries: Arc<dyn ScriptQueries>,
    invocation: Invocation,
}

impl ServiceAccess {
    /// Access to `docs` and `queries` as `invocation`'s actor.
    pub fn new(
        docs: Arc<dyn DocumentService>,
        queries: Arc<dyn ScriptQueries>,
        invocation: Invocation,
    ) -> ServiceAccess {
        ServiceAccess {
            docs,
            queries,
            invocation,
        }
    }
}

impl ScriptDocumentAccess for ServiceAccess {
    fn invocation(&self) -> &Invocation {
        &self.invocation
    }

    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.docs.meta(doctype)
    }

    fn get<'a>(&'a mut self, doctype: &'a str, id: RecordId) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            let outcome = self.docs.load(&self.invocation, doctype, id).await?;
            Ok(outcome.value)
        })
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        self.queries.query(&self.invocation, query)
    }

    fn insert(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        Box::pin(async move {
            let outcome = self.docs.create(&self.invocation, doc).await?;
            Ok(outcome.value)
        })
    }

    fn update<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            let outcome = self
                .docs
                .update(&self.invocation, doctype, id, patch, expected)
                .await?;
            Ok(outcome.value)
        })
    }

    fn call<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            let outcome = self
                .docs
                .call_method(&self.invocation, doctype, id, method, args, None)
                .await?;
            Ok(outcome.value.1)
        })
    }
}

/// The production factory: every run gets a [`ServiceAccess`] over the host's services.
pub struct ServiceAccessFactory {
    docs: Arc<dyn DocumentService>,
    queries: Arc<dyn ScriptQueries>,
}

impl ServiceAccessFactory {
    /// A factory over `docs` and `queries`.
    pub fn new(docs: Arc<dyn DocumentService>, queries: Arc<dyn ScriptQueries>) -> Self {
        ServiceAccessFactory { docs, queries }
    }
}

impl ScriptAccessFactory for ServiceAccessFactory {
    fn open(&self, invocation: Invocation) -> Box<dyn ScriptDocumentAccess> {
        Box::new(ServiceAccess::new(
            Arc::clone(&self.docs),
            Arc::clone(&self.queries),
            invocation,
        ))
    }
}
