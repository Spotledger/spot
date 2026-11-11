//! End-to-end: the document engine runs a registered controller's step on the fake backend,
//! and the controller's whitelisted method is reachable through the registry.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{Probe, inv, method, n, step, text};
use serde_json::{Value as Json, json};
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{Query, RecordId, Rows, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass, Invocation,
    JobRequest, Message, MethodContext, Repository, Seams,
};
use spot_app_api::{HookPoint, Operation, Phase, Target};
use spot_controllers::{ControllerRegistry, ControllerSource};
use spot_core::SystemClock;
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_storage_fake::FakeBackend;

fn before_create() -> HookPoint {
    HookPoint::of(Phase::Before, Target::Op(Operation::Create))
}

fn unsupported() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "platform.unsupported",
        "this test context does not implement repository writes".to_owned(),
    )
}

/// A minimal [`MethodContext`] that carries a document loaded from the engine's backend and
/// refuses every repository write, so a method that only reads its own document runs.
struct MethodCtx {
    invocation: Invocation,
    doc: Document,
    seams: Arc<spot_seams::Registry>,
}

impl Repository for MethodCtx {
    fn meta<'a>(&'a mut self, _doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn load<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn query<'a>(&'a mut self, _query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn create(&mut self, _doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn update<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _patch: DocumentPatch,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn post<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn cancel<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn delete<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Err(unsupported()) })
    }
}

impl MethodContext for MethodCtx {
    fn invocation(&self) -> &Invocation {
        &self.invocation
    }

    fn doc(&self) -> &Document {
        &self.doc
    }

    fn message(&mut self, _message: Message) {}

    fn enqueue_job(&mut self, _job: JobRequest) -> AppResult<()> {
        Err(unsupported())
    }

    fn seams(&self) -> &dyn Seams {
        self.seams.as_ref()
    }
}

fn note_meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut def = Definition::new("note");
    def.fields.push(FieldSpec::new("title", FieldType::Text));
    Registry::new(vec![def])
        .expect("the definition compiles")
        .effective()
}

#[tokio::test]
async fn a_registered_controllers_step_runs_on_the_engine() {
    let probe = Arc::new(
        Probe::new("note")
            .with_step(step("probe:fill_title", before_create(), 0))
            .setting("title", text("set_by_controller"))
            .with_method(method("ping", false)),
    );

    let mut registry = ControllerRegistry::new();
    registry.register("probe", probe.clone(), ControllerSource::Native);
    let seams = registry.install().expect("no conflicts");

    let engine = Engine::new(
        FakeBackend::new(),
        note_meta(),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds");
    engine.prepare().await.expect("the store prepares");

    let mut draft = Document::new(n("note"), RecordId::new_v7());
    draft.set(n("title"), text("draft"));
    let created = engine
        .create(&inv(), draft)
        .await
        .expect("the document saves through the default path plus the controller step")
        .value;

    assert_eq!(probe.runs(), 1, "the before-create step ran");
    let stored = engine
        .peek("note", created.id())
        .await
        .expect("peek works")
        .expect("the document is stored");
    assert_eq!(
        stored.get("title"),
        &Value::Text("set_by_controller".to_owned())
    );
}

#[tokio::test]
async fn a_registered_method_is_reachable_with_its_document() {
    let probe = Arc::new(
        Probe::new("note")
            .with_step(step("probe:fill_title", before_create(), 0))
            .with_method(method("ping", false)),
    );
    let mut registry = ControllerRegistry::new();
    registry.register("probe", probe.clone(), ControllerSource::Native);
    let seams = registry.install().expect("no conflicts");

    let engine = Engine::new(
        FakeBackend::new(),
        note_meta(),
        seams.clone(),
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds");
    engine.prepare().await.expect("the store prepares");

    // The method index resolves `ping` to the app that declares it.
    let index = registry.methods("note").expect("no conflict");
    let ping = index.get("ping").expect("declared");
    assert_eq!(ping.app, "probe");

    let mut draft = Document::new(n("note"), RecordId::new_v7());
    draft.set(n("title"), text("draft"));
    let created = engine.create(&inv(), draft).await.expect("saved").value;

    let loaded = engine
        .peek("note", created.id())
        .await
        .expect("peek works")
        .expect("stored");
    let mut ctx = MethodCtx {
        invocation: inv(),
        doc: loaded,
        seams,
    };
    let controller = registry.controllers_for("note")[0].controller.clone();
    let out: Json = controller
        .call_method("ping", json!({"n": 1}), &mut ctx)
        .await
        .expect("the method runs");
    assert_eq!(out["method"], json!("ping"));
    assert_eq!(probe.calls(), 1);
}
