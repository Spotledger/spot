//! Collecting what steps report.
//!
//! The document engine hands steps a context whose `report_effect` may drop the report. The kit
//! needs every report, so the app wraps its controllers with [`Recorder::wrap`] before it gives
//! them to the registry builder and the engine. The wrapper forwards everything unchanged and
//! keeps a copy of each [`EffectReceipt`] a step reports.
//!
//! Limit: a report made while the engine retries a transaction is recorded once per attempt;
//! the in-memory backend does not retry.

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value as Json;
use spot_app_api::context::{AppCache, Invocation, JobRequest, MethodContext};
use spot_app_api::document::{Document, DocumentPatch};
use spot_app_api::meta::DocTypeDef;
use spot_app_api::prelude::{Query, RecordId, Rows, Version};
use spot_app_api::{
    AppResult, BoxFuture, Controller, EffectReceipt, HookContext, HookPoint, Message, MethodDecl,
    Repository, Seams, StepDecl, StepEffects, StepName,
};
use spot_events_contract::Topic;

/// A shared list of the effects reported since it was last emptied.
#[derive(Clone, Debug, Default)]
pub struct Recorder {
    seen: Arc<Mutex<Vec<EffectReceipt>>>,
}

impl Recorder {
    /// An empty recorder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps a controller so its steps' reports land here. Declarations, steps and methods are
    /// the controller's own.
    pub fn wrap(&self, controller: Arc<dyn Controller>) -> Arc<dyn Controller> {
        Arc::new(Recording {
            inner: controller,
            recorder: self.clone(),
        })
    }

    /// Takes and empties everything reported so far, in the order reported.
    pub fn take(&self) -> Vec<EffectReceipt> {
        std::mem::take(&mut *self.seen.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Forgets everything reported so far.
    pub fn clear(&self) {
        drop(self.take());
    }

    fn push(&self, receipt: EffectReceipt) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(receipt);
    }
}

struct Recording {
    inner: Arc<dyn Controller>,
    recorder: Recorder,
}

impl Controller for Recording {
    fn doctype(&self) -> &str {
        self.inner.doctype()
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.inner.steps()
    }

    fn step_effects(&self) -> Vec<StepEffects> {
        self.inner.step_effects()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        self.inner.methods()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let mut recording = RecordingCtx {
                inner: ctx,
                recorder: &self.recorder,
            };
            self.inner.run_step(step, &mut recording).await
        })
    }

    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        self.inner.call_method(method, args, ctx)
    }
}

struct RecordingCtx<'r> {
    inner: &'r mut dyn HookContext,
    recorder: &'r Recorder,
}

impl Repository for RecordingCtx<'_> {
    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.inner.meta(doctype)
    }

    fn load<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.inner.load(doctype, id)
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        self.inner.query(query)
    }

    fn create(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        self.inner.create(doc)
    }

    fn update<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.inner.update(doctype, id, patch, expected)
    }

    fn post<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.inner.post(doctype, id, expected)
    }

    fn cancel<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.inner.cancel(doctype, id, expected)
    }

    fn delete<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<()>> {
        self.inner.delete(doctype, id, expected)
    }
}

impl HookContext for RecordingCtx<'_> {
    fn point(&self) -> &HookPoint {
        self.inner.point()
    }

    fn invocation(&self) -> &Invocation {
        self.inner.invocation()
    }

    fn doc(&self) -> &Document {
        self.inner.doc()
    }

    fn doc_mut(&mut self) -> AppResult<&mut Document> {
        self.inner.doc_mut()
    }

    fn message(&mut self, message: Message) {
        self.inner.message(message);
    }

    fn enqueue_job(&mut self, job: JobRequest) -> AppResult<()> {
        self.inner.enqueue_job(job)
    }

    fn publish(&mut self, topic: &Topic, schema_version: u32, data: Json) -> AppResult<()> {
        self.inner.publish(topic, schema_version, data)
    }

    fn notify_user(&mut self, user: RecordId, payload: Json) -> AppResult<()> {
        self.inner.notify_user(user, payload)
    }

    fn external_allowed(&self) -> AppResult<()> {
        self.inner.external_allowed()
    }

    fn seams(&self) -> &dyn Seams {
        self.inner.seams()
    }

    fn report_effect(&mut self, receipt: EffectReceipt) {
        self.recorder.push(receipt.clone());
        self.inner.report_effect(receipt);
    }

    fn cache(&self) -> &dyn AppCache {
        self.inner.cache()
    }

    fn run_wrapped(&mut self) -> BoxFuture<'_, AppResult<()>> {
        self.inner.run_wrapped()
    }
}
