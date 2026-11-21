//! A `TestHost` over the document engine and the in-memory backend: the fixture of the app API
//! contract suite. `define` rebuilds the effective metadata, `register` rebuilds the seam
//! registry; both are applied lazily, before the next operation or inspection, so DocTypes that
//! refer to each other can be defined in any order.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

/// The app API contract cases this engine supports: the 19 of WP-DOC-04.T1 plus the five that
/// need post, cancel, correct, rename or whitelisted methods (WP-DOC-04.T2). The suite has since
/// grown to cover the v0.2 field model and field security, which are other work packages' scope.
pub const CASES: [&str; 24] = [
    "order_of_every_operation",
    "order_of_correct",
    "before_derives_own_document",
    "validate_may_not_change_or_write",
    "validate_may_not_write_other_documents",
    "before_may_not_write_other_documents",
    "after_writes_others_and_everything_rolls_back",
    "after_may_not_change_own_document",
    "external_calls_only_after_commit",
    "committed_failure_does_not_roll_back",
    "jobs_only_from_phase_3_and_4",
    "stale_version_is_refused",
    "merge_patch_semantics",
    "typed_field_errors_with_paths",
    "state_rules",
    "delete_refused_while_linked",
    "lifecycle_events",
    "messages_are_collected",
    "priority_orders_steps_across_controllers",
    "overrides",
    "default_controller_for_runtime_doctype",
    "whitelisted_methods",
    "form_and_load_events",
    "seams_are_typed",
];

use std::future::Future;
use std::pin::pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};

use serde_json::Value as Json;
use spot_app_api::context::{
    DocumentService, HookContext, Invocation, JobRequest, MethodContext, Outcome,
};
use spot_app_api::contract_tests::TestHost;
use spot_app_api::meta::{DocTypeDef, FieldDef, FieldKind};
use spot_app_api::prelude::{RecordId, Version};
use spot_app_api::seams::{SeamProvider, StepOverride};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, Document, DocumentPatch, ErrorClass, MethodDecl,
    StepDecl, StepName,
};
use spot_core::SystemClock;
use spot_document::engine::{
    Engine, IdGenerator, TOPIC_DOC_STATE, TOPIC_JOB, TOPIC_NOTICE, V7Ids, job_from_event,
};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{FeedPosition, OutboxEvent, StorageBackend};
use spot_storage_fake::FakeBackend;

/// Runs a future that the in-memory backend never suspends.
pub fn now_or_never<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("the in-memory backend suspended"),
    }
}

/// How a host drives the engine's short futures from its synchronous inspection methods.
pub trait Block: Send + Sync + 'static {
    /// Runs `f` to completion on the current thread.
    fn run<F: Future>(f: F) -> F::Output;
}

/// For the in-memory backend, which never suspends: poll once.
pub struct NowOrNever;

impl Block for NowOrNever {
    fn run<F: Future>(f: F) -> F::Output {
        now_or_never(f)
    }
}

/// For a real backend: notify the runtime the worker blocks, then drive the future on it. Needs
/// a multi-threaded runtime (`#[tokio::test(flavor = "multi_thread", ...)]`).
pub struct Blocking;

impl Block for Blocking {
    fn run<F: Future>(f: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
    }
}

/// A controller whose step names are scoped to its DocType: `<doctype>_<app>:<step>`.
///
/// ADR-057 fixes step names as `<app>:<step>` and the seam registry refuses a name declared
/// twice. The contract suite registers one controller per DocType that may reuse a step name
/// (its recorders log the same points for two DocTypes), which the stub host tolerates because
/// it plans per DocType. This wrapper gives each controller's steps a DocType-unique name so the
/// real registry can hold them, and its name map lets the host translate override targets and
/// step dispatch.
struct Scoped {
    inner: Arc<dyn Controller>,
    app: String,
    /// `(original name, scoped name)`, one per inner step, in step order.
    names: Vec<(StepName, StepName)>,
}

impl Scoped {
    fn new(inner: Arc<dyn Controller>) -> Self {
        let doctype = inner.doctype().to_owned();
        let mut names = Vec::new();
        let mut app = doctype.clone();
        for (i, decl) in inner.steps().iter().enumerate() {
            let step = decl.name.as_str();
            let step = step.split_once(':').map_or(step, |(_, n)| n);
            let scoped_app = format!("{doctype}_{}", decl.name.app());
            if i == 0 {
                app = scoped_app.clone();
            }
            names.push((
                decl.name.clone(),
                StepName::new(&format!("{scoped_app}:{step}")).unwrap(),
            ));
        }
        Self { inner, app, names }
    }
}

impl Controller for Scoped {
    fn doctype(&self) -> &str {
        self.inner.doctype()
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.inner
            .steps()
            .into_iter()
            .enumerate()
            .map(|(i, decl)| StepDecl {
                name: self.names[i].1.clone(),
                point: decl.point,
                priority: decl.priority,
                budget: decl.budget,
            })
            .collect()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        self.inner.methods()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        match self.names.iter().find(|(_, scoped)| scoped == step) {
            Some((original, _)) => self.inner.run_step(original, ctx),
            None => Box::pin(async move {
                Err(AppError::new(
                    ErrorClass::Internal,
                    "host.unknown_step",
                    step.as_str(),
                ))
            }),
        }
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

#[derive(Default)]
struct Setup {
    defs: Vec<Definition>,
    /// `(app, controller)`, the controller already scoped to its DocType.
    controllers: Vec<(String, Arc<dyn Controller>)>,
    /// `original step name -> scoped step name`, for translating override targets.
    step_names: std::collections::BTreeMap<String, String>,
    overrides: Vec<(String, StepOverride)>,
    providers: Vec<(String, String, u32, Arc<dyn SeamProvider>)>,
    dirty: bool,
}

pub struct Host<B: StorageBackend, R: Block = NowOrNever> {
    pub engine: Engine<B>,
    setup: Mutex<Setup>,
    _bridge: std::marker::PhantomData<R>,
}

impl Host<FakeBackend, NowOrNever> {
    pub fn new() -> Self {
        let meta = Registry::new(Vec::new()).unwrap().effective();
        let seams = Arc::new(RegistryBuilder::new().build().unwrap());
        let engine = Engine::new(
            FakeBackend::new(),
            meta,
            seams,
            Arc::new(SystemClock::new()),
            Arc::new(V7Ids),
        )
        .unwrap();
        Self {
            engine,
            setup: Mutex::new(Setup::default()),
            _bridge: std::marker::PhantomData,
        }
    }
}

impl<B: StorageBackend, R: Block> Host<B, R> {
    /// A host over `backend`, blocking on the engine's short futures through `R`.
    pub fn with_backend(backend: B) -> Self {
        let meta = Registry::new(Vec::new()).unwrap().effective();
        let seams = Arc::new(RegistryBuilder::new().build().unwrap());
        let engine = Engine::new(
            backend,
            meta,
            seams,
            Arc::new(SystemClock::new()),
            Arc::new(V7Ids),
        )
        .unwrap();
        Self {
            engine,
            setup: Mutex::new(Setup::default()),
            _bridge: std::marker::PhantomData,
        }
    }

    fn setup(&self) -> MutexGuard<'_, Setup> {
        self.setup.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every outbox event the backend holds, raw (jobs and notices included).
    fn all_events(&self) -> Vec<OutboxEvent> {
        R::run(
            self.engine
                .backend()
                .read_outbox(FeedPosition::START, 100_000),
        )
        .unwrap()
        .into_iter()
        .map(|r| r.event)
        .collect()
    }

    /// Applies pending definitions, controllers, overrides and seams to the engine.
    fn refresh(&self) {
        let mut setup = self.setup();
        if !setup.dirty {
            return;
        }
        setup.dirty = false;
        let meta = Registry::new(setup.defs.clone())
            .unwrap_or_else(|e| panic!("the contract catalog does not compile: {e:?}"))
            .effective();
        R::run(self.engine.set_meta(meta)).unwrap();
        let mut builder = RegistryBuilder::new();
        for (app, c) in &setup.controllers {
            builder.add_controller(app, c.clone());
        }
        for (app, o) in &setup.overrides {
            builder.add_override(app, o.clone());
        }
        for (app, name, version, p) in &setup.providers {
            builder.add_seam_provider(app, name, *version, p.clone());
        }
        let seams = builder
            .build()
            .unwrap_or_else(|c| panic!("the registry has conflicts: {c:?}"));
        self.engine.set_seams(Arc::new(seams));
        self.engine
            .set_controllers(setup.controllers.iter().map(|(_, c)| c.clone()).collect());
    }
}

impl<B: StorageBackend, R: Block> TestHost for Host<B, R> {
    fn define(&self, def: DocTypeDef) {
        let mut setup = self.setup();
        let new = to_definition(&def);
        setup.defs.retain(|d| d.name != new.name);
        setup.defs.push(new);
        setup.dirty = true;
    }

    fn register(&self, controller: Arc<dyn Controller>) {
        let mut setup = self.setup();
        let scoped = Scoped::new(controller);
        let app = scoped.app.clone();
        for (original, scoped_name) in &scoped.names {
            setup
                .step_names
                .entry(original.as_str().to_owned())
                .or_insert_with(|| scoped_name.as_str().to_owned());
        }
        setup.controllers.push((app, Arc::new(scoped)));
        setup.dirty = true;
    }

    fn add_override(&self, o: StepOverride) {
        let mut setup = self.setup();
        let rewrite = |name: &StepName| -> StepName {
            setup
                .step_names
                .get(name.as_str())
                .and_then(|s| StepName::new(s).ok())
                .unwrap_or_else(|| name.clone())
        };
        let o = StepOverride {
            target: rewrite(&o.target),
            kind: o.kind,
            by: o.by.as_ref().map(&rewrite),
            reason: o.reason,
        };
        setup.overrides.push((o.target.app().to_owned(), o));
        setup.dirty = true;
    }

    fn provide_seam(&self, name: &str, version: u32, provider: Arc<dyn SeamProvider>) {
        let mut setup = self.setup();
        let app = name.split(':').next().unwrap_or("probe").to_owned();
        setup
            .providers
            .push((app, name.to_owned(), version, provider));
        setup.dirty = true;
    }

    fn outbox(&self) -> Vec<OutboxEvent> {
        self.refresh();
        // The stub host keeps jobs and notices apart from the outbox (they are platform
        // topics), and the contract suite assumes that; filter them out here too. The journal's
        // `doc_state` images (WP-JRN-02.T2) are not lifecycle events either: the suite counts
        // the lifecycle events of an operation.
        self.all_events()
            .into_iter()
            .filter(|e| !matches!(e.topic.as_str(), TOPIC_JOB | TOPIC_NOTICE | TOPIC_DOC_STATE))
            .collect()
    }

    fn jobs(&self) -> Vec<JobRequest> {
        self.refresh();
        self.all_events()
            .iter()
            .filter_map(|e| job_from_event(e).unwrap())
            .collect()
    }

    fn committed_failures(&self) -> Vec<(String, AppError)> {
        self.engine
            .committed_failures()
            .into_iter()
            .map(|f| (f.point, f.error))
            .collect()
    }

    fn stored(&self, doctype: &str, id: RecordId) -> Option<Document> {
        self.refresh();
        R::run(self.engine.peek(doctype, id)).unwrap()
    }

    fn fresh_id(&self) -> RecordId {
        V7Ids.next_id()
    }
}

impl<B: StorageBackend, R: Block> DocumentService for Host<B, R> {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.refresh();
        self.engine.meta(doctype)
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.refresh();
        self.engine.load(inv, doctype, id)
    }

    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.refresh();
        self.engine.create(inv, doc)
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.refresh();
        self.engine.update(inv, doctype, id, patch, expected)
    }

    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.refresh();
        self.engine.post(inv, doctype, id, expected)
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.refresh();
        self.engine.cancel(inv, doctype, id, expected)
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.refresh();
        self.engine.correct(inv, doctype, id, expected)
    }

    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        self.refresh();
        self.engine.delete(inv, doctype, id, expected)
    }

    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.refresh();
        self.engine.rename(inv, doctype, id, new_name, expected)
    }

    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, Json)>>> {
        self.refresh();
        self.engine
            .call_method(inv, doctype, id, method, args, expected)
    }

    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.refresh();
        self.engine.field_changed(inv, draft, field)
    }
}

/// The definition-format twin of a contract DocType. The engine-managed fields
/// (`amended_from`, `doc_state`) are reserved in `spot-meta` and added to every posted DocType,
/// so the contract's own declaration of them is dropped.
fn to_definition(def: &DocTypeDef) -> Definition {
    let mut d = Definition::new(def.name.as_str());
    d.label = Some(def.label.clone());
    d.is_posted = def.is_posted;
    d.is_child = def.is_child;
    d.title_field = def.title_field.as_ref().map(|n| n.as_str().to_owned());
    d.fields = def
        .fields
        .iter()
        .filter(|f| !spot_meta::ENGINE_FIELDS.contains(&f.name.as_str()))
        .map(to_field)
        .collect();
    d
}

fn to_field(f: &FieldDef) -> FieldSpec {
    let name = f.name.as_str();
    let mut spec = match &f.kind {
        FieldKind::Bool => FieldSpec::new(name, FieldType::Bool),
        FieldKind::Int { .. } => FieldSpec::new(name, FieldType::Int),
        FieldKind::Decimal {
            scale, precision, ..
        } => {
            let mut s = FieldSpec::new(name, FieldType::Decimal);
            s.scale = Some(*scale);
            s.precision = Some(*precision);
            s
        }
        FieldKind::Text { max_chars, .. } => {
            let mut s = FieldSpec::new(name, FieldType::Text);
            s.max_chars = *max_chars;
            s
        }
        FieldKind::LongText { .. } => FieldSpec::new(name, FieldType::LongText),
        FieldKind::Select { values } => {
            let mut s = FieldSpec::new(name, FieldType::Select);
            s.options = Some(values.join("\n"));
            s
        }
        FieldKind::Date => FieldSpec::new(name, FieldType::Date),
        FieldKind::Time => FieldSpec::new(name, FieldType::Time),
        FieldKind::DateTime => FieldSpec::new(name, FieldType::DateTime),
        FieldKind::Link { doctype } => {
            let mut s = FieldSpec::new(name, FieldType::Link);
            s.options = Some(doctype.as_str().to_owned());
            s
        }
        FieldKind::Table { doctype } => {
            let mut s = FieldSpec::new(name, FieldType::Table);
            s.options = Some(doctype.as_str().to_owned());
            s
        }
        FieldKind::Json { .. } => FieldSpec::new(name, FieldType::Json),
        FieldKind::Bytes => FieldSpec::new(name, FieldType::Bytes),
        other => panic!("this test host does not convert {other:?} yet"),
    };
    spec.required = f.required;
    spec.read_only = f.read_only;
    spec.allow_on_posted = f.allow_on_posted;
    spec
}
