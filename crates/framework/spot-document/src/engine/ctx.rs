//! What a step gets: the hook context, with the phase rules checked on every call (ADR-51).

use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::Operation;
use spot_app_api::context::{
    AppCache, HookContext, Invocation, JobRequest, MethodContext, Repository,
};
use spot_app_api::error::StorageOp;
use spot_app_api::lifecycle::Phase;
use spot_app_api::prelude::{Query, RecordId, Rows, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass, HookPoint,
    Message, Seams,
};
use spot_events_contract::Topic;
use spot_permissions::Action;
use spot_seams::WrappedStep;
use spot_storage_contract::{Outbox, OutboxEvent, QueryExecutor, RowLock, StorageBackend};
use tracing::Instrument;

use super::authz::{apply_masks, empty_page};
use super::ops::{Req, Ret, not_found};
use super::receipt::{Announced, ReceiptCollector};
use super::work::Work;
use super::{Engine, Snapshot, outbox};

/// The context of one hook point's steps.
pub(crate) struct Ctx<'a, B: StorageBackend> {
    engine: &'a Engine<B>,
    snap: &'a Snapshot,
    /// The open transaction in phases 1 to 3; none in the committed and read phases, where
    /// writes are new top-level operations and reads use short transactions of their own.
    work: Option<&'a mut Work<B::Tx>>,
    inv: &'a Invocation,
    point: HookPoint,
    pub(crate) doc: Document,
    pub(crate) messages: Vec<Message>,
    /// Events the steps enqueued; written to the outbox when the hook point is done.
    pending: Vec<OutboxEvent>,
    wrapped: Option<WrappedStep>,
    /// Whether this context is a whitelisted method's: a method may change its **own** document
    /// through the repository (ADR-16), where a hook phase may not.
    method: bool,
    /// What the steps reported with `report_effect`. In phases 1 to 3 the hook point's caller
    /// hands it to the operation's collector; in phase 4 `run_and_flush` writes it as receipt
    /// announcements.
    reports: ReceiptCollector,
    /// For a committed hook point: the operation it is the committed phase of.
    operation: Option<Operation>,
    /// The announcements `run_and_flush` wrote, for the receipt returned to the caller.
    pub(crate) announced: Vec<Announced>,
}

impl<'a, B: StorageBackend> Ctx<'a, B> {
    /// A context in the business transaction (phases 1 to 3).
    pub(crate) fn in_tx(
        engine: &'a Engine<B>,
        snap: &'a Snapshot,
        work: &'a mut Work<B::Tx>,
        inv: &'a Invocation,
        point: HookPoint,
        doc: Document,
    ) -> Self {
        Self {
            engine,
            snap,
            work: Some(work),
            inv,
            point,
            doc,
            messages: Vec::new(),
            pending: Vec::new(),
            wrapped: None,
            method: false,
            reports: ReceiptCollector::default(),
            operation: None,
            announced: Vec::new(),
        }
    }

    /// A whitelisted method's context: in the business transaction, but free to change its own
    /// document (ADR-16). Its `point` fixes the phase rules the method's writes run under.
    pub(crate) fn method(
        engine: &'a Engine<B>,
        snap: &'a Snapshot,
        work: &'a mut Work<B::Tx>,
        inv: &'a Invocation,
        point: HookPoint,
        doc: Document,
    ) -> Self {
        Self {
            method: true,
            ..Self::in_tx(engine, snap, work, inv, point, doc)
        }
    }

    /// A context outside any transaction (the committed and read phases).
    pub(crate) fn detached(
        engine: &'a Engine<B>,
        snap: &'a Snapshot,
        inv: &'a Invocation,
        point: HookPoint,
        doc: Document,
    ) -> Self {
        Self {
            engine,
            snap,
            work: None,
            inv,
            point,
            doc,
            messages: Vec::new(),
            pending: Vec::new(),
            wrapped: None,
            method: false,
            reports: ReceiptCollector::default(),
            operation: None,
            announced: Vec::new(),
        }
    }

    /// Marks the context as the committed phase of `operation`, so that effects its steps
    /// report are announced for that operation's receipt.
    #[must_use]
    pub(crate) fn for_operation(mut self, operation: Operation) -> Self {
        self.operation = Some(operation);
        self
    }

    /// What the steps reported so far, taken out of the context.
    pub(crate) fn take_reports(&mut self) -> ReceiptCollector {
        std::mem::take(&mut self.reports)
    }

    /// Runs every planned step at the context's hook point, in order, with overrides applied.
    pub(crate) async fn run_point(&mut self) -> AppResult<()> {
        let snap = self.snap;
        let doctype = self.doc.doctype().as_str().to_owned();
        let point = self.point.clone();
        for planned in snap.seams.plan(&doctype, &point) {
            self.wrapped = planned.wrapped.clone();
            // Names only: the hook point, the step and the app that declared it.
            let span = tracing::info_span!(
                "doc.step",
                point = %point.name(),
                step = planned.step.as_str(),
                app = planned.app.as_str(),
            );
            planned
                .controller
                .run_step(&planned.step, &mut *self)
                .instrument(span)
                .await?;
        }
        self.wrapped = None;
        Ok(())
    }

    /// Runs the point's steps and then writes what they enqueued to the outbox: in the
    /// transaction for phases 1 to 3 (so it commits or rolls back with the document), in a
    /// transaction of its own for phase 4. A failed step enqueues nothing.
    pub(crate) async fn run_and_flush(&mut self) -> AppResult<()> {
        self.run_point().await?;
        if self.work.is_none()
            && let Some(op) = self.operation
        {
            // Phase 4: the effects the steps announced become receipt events, written with
            // whatever else they enqueued, in the transaction of their own.
            let reports = self.take_reports();
            let (events, announced) = self
                .engine
                .announce(self.snap, self.inv, op, &self.doc, reports)?;
            self.pending.extend(events);
            self.announced = announced;
        }
        if self.pending.is_empty() {
            return Ok(());
        }
        let events = std::mem::take(&mut self.pending);
        match self.work.as_deref_mut() {
            Some(work) => {
                for event in &events {
                    work.tx
                        .enqueue(event)
                        .await
                        .map_err(|e| work.fail(e, StorageOp::Other))?;
                }
                Ok(())
            }
            None => self.engine.enqueue_detached(self.snap, events).await,
        }
    }

    fn phase(&self) -> Phase {
        self.point.phase()
    }

    fn deny(&self, what: &str) -> AppError {
        AppError::phase_violation(what, &self.point.name())
    }

    fn check_write(&self, doctype: &str, id: RecordId) -> AppResult<()> {
        if !self.phase().rules().write_others {
            return Err(self.deny("Writing documents"));
        }
        if !self.method && doctype == self.doc.doctype().as_str() && id == self.doc.id() {
            return Err(self.deny("Writing the step's own document through the repository"));
        }
        Ok(())
    }

    /// Runs a write in the transaction (phase 3, and methods) or as its own top-level operation
    /// (phase 4). The document it returns is filtered for the acting user like a read.
    async fn write(&mut self, req: Req) -> AppResult<Ret> {
        let (engine, snap, inv) = (self.engine, self.snap, self.inv);
        match self.work.as_deref_mut() {
            Some(work) => {
                let ret = engine.nested(snap, work, inv, req).await?;
                let principal = engine.acting_principal(inv).await;
                engine.mask_result(&principal, ret).await
            }
            None => {
                let done = engine.top(inv, req).await?;
                self.messages.extend(done.messages);
                Ok(done.value)
            }
        }
    }
}

impl<B: StorageBackend> Repository for Ctx<'_, B> {
    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async move { self.snap.doctype(doctype) })
    }

    fn load<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            let found = match self.work.as_deref_mut() {
                Some(work) => {
                    self.engine
                        .load_stored(
                            self.snap,
                            work,
                            doctype,
                            id,
                            RowLock::None,
                            StorageOp::Other,
                        )
                        .await?
                }
                None => self.engine.read_document(self.snap, doctype, id).await?,
            };
            let doc = found.ok_or_else(not_found)?;
            let principal = self.engine.authorize(self.inv, Action::Read, &doc).await?;
            self.engine.readable(&principal, doc).await
        })
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async move {
            // The invocation's read rights compile into a filter on `query` here, before it
            // reaches storage: the caller's query is checked, combined and column-restricted,
            // and a principal that may read no row gets an empty page without a storage read.
            let Some(plan) = self.engine.query_plan(self.inv, query).await? else {
                return Ok(empty_page(query));
            };
            let mut rows = match self.work.as_deref_mut() {
                Some(work) => work
                    .tx
                    .query(&plan.query)
                    .await
                    .map_err(|e| work.fail(e, StorageOp::Other))?,
                None => self.engine.query_detached(self.snap, &plan.query).await?,
            };
            apply_masks(&mut rows, &plan.masks);
            Ok(rows)
        })
    }

    fn create(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        Box::pin(async move {
            self.check_write(doc.doctype().as_str(), doc.id())?;
            self.write(Req::Create(doc)).await?.doc()
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
            self.check_write(doctype, id)?;
            let req = Req::Update {
                doctype: doctype.to_owned(),
                id,
                patch,
                expected,
            };
            self.write(req).await?.doc()
        })
    }

    fn post<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            self.check_write(doctype, id)?;
            let req = Req::Post {
                doctype: doctype.to_owned(),
                id,
                expected,
            };
            self.write(req).await?.doc()
        })
    }

    fn cancel<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            self.check_write(doctype, id)?;
            let req = Req::Cancel {
                doctype: doctype.to_owned(),
                id,
                expected,
            };
            self.write(req).await?.doc()
        })
    }

    fn delete<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.check_write(doctype, id)?;
            let req = Req::Delete {
                doctype: doctype.to_owned(),
                id,
                expected,
            };
            self.write(req).await.map(|_| ())
        })
    }
}

impl<B: StorageBackend> HookContext for Ctx<'_, B> {
    fn point(&self) -> &HookPoint {
        &self.point
    }

    fn invocation(&self) -> &Invocation {
        self.inv
    }

    fn doc(&self) -> &Document {
        &self.doc
    }

    fn doc_mut(&mut self) -> AppResult<&mut Document> {
        if self.phase().rules().modify_own || matches!(self.point, HookPoint::FieldChanged(_)) {
            Ok(&mut self.doc)
        } else {
            Err(self.deny("Changing the document"))
        }
    }

    fn message(&mut self, message: Message) {
        self.messages.push(message);
    }

    fn enqueue_job(&mut self, job: JobRequest) -> AppResult<()> {
        if !self.phase().rules().enqueue {
            return Err(self.deny("Enqueuing a job"));
        }
        if !spot_app_api::context::valid_job_name(&job.job) {
            return Err(AppError::new(
                ErrorClass::Internal,
                "platform.invalid_job_name",
                format!("job names are <app>:<job>, got {:?}", job.job),
            ));
        }
        // `sys` is the platform's own: a `sys:committed_steps` job carries the acting identity
        // and runs steps as it, so only the delivery path may write one, never an app's hook.
        if job.job.starts_with("sys:") {
            return Err(AppError::new(
                ErrorClass::Internal,
                "platform.invalid_job_name",
                format!(
                    "job names under `sys:` are reserved for the platform, got {:?}",
                    job.job
                ),
            ));
        }
        let event = outbox::job(&*self.engine.ids, self.inv, &self.doc, &job)?;
        self.pending.push(event);
        Ok(())
    }

    fn publish(&mut self, topic: &Topic, schema_version: u32, data: Json) -> AppResult<()> {
        if !self.phase().rules().enqueue {
            return Err(self.deny("Publishing an event"));
        }
        if topic.is_platform() {
            return Err(AppError::new(
                ErrorClass::Internal,
                "platform.reserved_topic",
                format!("{topic} is a platform topic"),
            ));
        }
        let event = outbox::app_event(
            &*self.engine.ids,
            self.inv,
            topic,
            &self.doc,
            schema_version,
            data,
        )?;
        self.pending.push(event);
        Ok(())
    }

    fn notify_user(&mut self, user: RecordId, payload: Json) -> AppResult<()> {
        if !self.phase().rules().enqueue {
            return Err(self.deny("Sending a realtime notice"));
        }
        let event = outbox::notice(&*self.engine.ids, self.inv, &self.doc, user, payload)?;
        self.pending.push(event);
        Ok(())
    }

    fn report_effect(&mut self, receipt: spot_app_api::EffectReceipt) {
        self.reports.report(receipt);
    }

    fn external_allowed(&self) -> AppResult<()> {
        if self.phase().rules().external {
            Ok(())
        } else {
            Err(self.deny("Calling an outside system"))
        }
    }

    fn seams(&self) -> &dyn Seams {
        &*self.snap.seams
    }

    fn cache(&self) -> &dyn AppCache {
        &*self.engine.cache
    }

    fn run_wrapped(&mut self) -> BoxFuture<'_, AppResult<()>> {
        Box::pin(async move {
            let Some(wrapped) = self.wrapped.take() else {
                return Err(self.deny("run_wrapped outside a wrap override"));
            };
            wrapped.controller.run_step(&wrapped.step, self).await
        })
    }
}

impl<B: StorageBackend> MethodContext for Ctx<'_, B> {
    fn invocation(&self) -> &Invocation {
        self.inv
    }

    fn doc(&self) -> &Document {
        &self.doc
    }

    fn message(&mut self, message: Message) {
        self.messages.push(message);
    }

    fn enqueue_job(&mut self, job: JobRequest) -> AppResult<()> {
        HookContext::enqueue_job(self, job)
    }

    fn seams(&self) -> &dyn Seams {
        &*self.snap.seams
    }
}
