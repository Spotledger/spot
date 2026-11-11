//! The participant seam carries the reason for a write and the node, and a refused operation is
//! reported to a denial sink (WP-DOC-04.T5), on the fake backend.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::{definitions, n, now, valid_invoice};
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation, MethodContext};
use spot_app_api::controller::{MethodDecl, StepDecl, StepName};
use spot_app_api::prelude::{CommandId, Query, RecordId, SiteId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, Document, DocumentPatch, ErrorClass, HookPoint,
    Operation, Phase, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{
    AllowAllAuthorizer, Authorizer, Denial, DenialSink, Engine, NodeId, TxParticipant, V7Ids,
    WriteCause, WriteIntent, WriteRecord,
};
use spot_meta::Registry;
use spot_permissions::{Action, DocFacts, Principal};
use spot_seams::RegistryBuilder;
use spot_storage_contract::Transaction;
use spot_storage_fake::FakeBackend;

// ---------------------------------------------------------------------------------------------
// A participant that writes down what the seam told it
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Seen {
    phase: &'static str,
    doctype: String,
    op: Operation,
    cause: String,
    node: Option<String>,
}

fn cause_text(cause: WriteCause<'_>) -> String {
    match cause {
        WriteCause::Method { name } => format!("method:{name}"),
        WriteCause::CorrectCancel => "correct_cancel".to_owned(),
        WriteCause::CorrectAmend => "correct_amend".to_owned(),
        WriteCause::Step => "step".to_owned(),
        WriteCause::Direct => "direct".to_owned(),
        _ => "other".to_owned(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Refuse {
    Never,
    /// Refuse `before_write` of this operation with an `InvalidState` error (a class the engine
    /// does not otherwise treat as a refusal of rights).
    Before(Operation),
    /// Refuse `after_write` of this operation (the document is already written and must roll back).
    After(Operation),
}

struct Recorder {
    log: Arc<Mutex<Vec<Seen>>>,
    refuse: Refuse,
    /// A refusal happens once: the sink's own writes must not be refused again.
    spent: std::sync::atomic::AtomicBool,
}

impl<Tx: Transaction> TxParticipant<Tx> for Recorder {
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        w: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(Seen {
                phase: "before",
                doctype: w.doctype.to_owned(),
                op: w.operation,
                cause: cause_text(w.cause),
                node: w.node.map(str::to_owned),
            });
            if self.refuse == Refuse::Before(w.operation) {
                return Err(AppError::new(
                    ErrorClass::InvalidState,
                    "test.participant_refused",
                    "name Acme is not allowed",
                ));
            }
            Ok(())
        })
    }

    fn after_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        w: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(Seen {
                phase: "after",
                doctype: w.doctype.to_owned(),
                op: w.operation,
                cause: cause_text(w.cause),
                node: w.node.map(str::to_owned),
            });
            if self.refuse == Refuse::After(w.operation)
                && !self.spent.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(AppError::new(
                    ErrorClass::InvalidState,
                    "test.participant_late",
                    "refused",
                ));
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The controller: a method that writes, and a step that writes another document
// ---------------------------------------------------------------------------------------------

struct Probe;

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("probe:after_create").unwrap(),
            point: HookPoint::of(Phase::After, Target::Op(Operation::Create)),
            priority: 0,
            budget: Duration::from_secs(5),
        }]
    }

    fn methods(&self) -> Vec<MethodDecl> {
        vec![MethodDecl::new(n("rename_once"), true)]
    }

    /// A customer named `Parent` gets a `Child`, written by the step through the repository.
    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if matches!(ctx.doc().get("name"), Value::Text(t) if t == "Parent") {
                let mut child = Document::new(n("customer"), RecordId::new_v7());
                child.set(n("name"), "Child");
                ctx.create(child).await?;
            }
            Ok(())
        })
    }

    fn call_method<'a>(
        &'a self,
        _method: &'a str,
        _args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            let id = ctx.doc().id();
            let version = ctx.doc().version().unwrap();
            let mut patch = DocumentPatch::default();
            patch.fields.insert(n("name"), "Renamed".into());
            let v = ctx.update("customer", id, patch, version).await?;
            Ok(json!({"version": v.version().unwrap().0}))
        })
    }
}

// ---------------------------------------------------------------------------------------------
// An authorizer that refuses updates, a sink that writes down what it is told
// ---------------------------------------------------------------------------------------------

struct NoUpdates;

/// Refuses `read` of any document, so a method call is answered `method.not_found`.
struct NoReads;

impl Authorizer for NoUpdates {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        AllowAllAuthorizer.principal_for(inv)
    }

    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> BoxFuture<'a, spot_permissions::Decision> {
        AllowAllAuthorizer.decide(principal, action, facts)
    }

    fn authorize<'a>(
        &'a self,
        _principal: &'a Principal,
        action: Action,
        _doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if matches!(action, Action::Op(Operation::Update)) {
                return Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    "permission.denied",
                    "Acme may not be changed",
                ));
            }
            Ok(())
        })
    }

    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [spot_document::engine::WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        AllowAllAuthorizer.authorize_write(principal, action, doc, changes)
    }

    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        def: &'a spot_app_api::DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        AllowAllAuthorizer.readable(principal, doc, def)
    }

    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Option<spot_document::engine::QueryPlan>>> {
        AllowAllAuthorizer.readable_query(principal, query)
    }
}

impl Authorizer for NoReads {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        AllowAllAuthorizer.principal_for(inv)
    }

    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> BoxFuture<'a, spot_permissions::Decision> {
        AllowAllAuthorizer.decide(principal, action, facts)
    }

    fn authorize<'a>(
        &'a self,
        _principal: &'a Principal,
        action: Action,
        _doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if matches!(action, Action::Read) {
                return Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    "permission.denied",
                    "Acme may not be read",
                ));
            }
            Ok(())
        })
    }

    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [spot_document::engine::WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        AllowAllAuthorizer.authorize_write(principal, action, doc, changes)
    }

    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        def: &'a spot_app_api::DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        AllowAllAuthorizer.readable(principal, doc, def)
    }

    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Option<spot_document::engine::QueryPlan>>> {
        AllowAllAuthorizer.readable_query(principal, query)
    }
}

/// Refuses `create` once switched on.
struct NoCreates(Arc<std::sync::atomic::AtomicBool>);

impl Authorizer for NoCreates {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        AllowAllAuthorizer.principal_for(inv)
    }

    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        facts: &'a DocFacts,
    ) -> BoxFuture<'a, spot_permissions::Decision> {
        AllowAllAuthorizer.decide(principal, action, facts)
    }

    fn authorize<'a>(
        &'a self,
        _principal: &'a Principal,
        action: Action,
        _doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if self.0.load(std::sync::atomic::Ordering::SeqCst)
                && matches!(action, Action::Op(Operation::Create))
            {
                return Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    "permission.denied",
                    "no create",
                ));
            }
            Ok(())
        })
    }

    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        doc: &'a Document,
        changes: &'a [spot_document::engine::WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        AllowAllAuthorizer.authorize_write(principal, action, doc, changes)
    }

    fn readable<'a>(
        &'a self,
        principal: &'a Principal,
        doc: Document,
        def: &'a spot_app_api::DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        AllowAllAuthorizer.readable(principal, doc, def)
    }

    fn readable_query<'a>(
        &'a self,
        principal: &'a Principal,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Option<spot_document::engine::QueryPlan>>> {
        AllowAllAuthorizer.readable_query(principal, query)
    }
}

#[derive(Clone, Debug)]
struct Told {
    doctype: String,
    id: Option<RecordId>,
    op: Operation,
    cause: String,
    class: ErrorClass,
    code: String,
    message: String,
    has_details: bool,
    system: bool,
    /// Whether the document was in storage when the sink ran.
    stored: Option<bool>,
    /// Whether the sink could create a document under the refused id (the id is free again).
    reused: Option<bool>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Record,
    Panic,
    Hang,
}

struct Sink {
    told: Mutex<Vec<Told>>,
    engine: OnceLock<Arc<Engine<FakeBackend>>>,
    mode: Mode,
    reuse: bool,
}

impl DenialSink for Sink {
    fn denied(&self, d: &Denial<'_>) -> BoxFuture<'_, ()> {
        let told = Told {
            doctype: d.doctype.to_owned(),
            id: d.id,
            op: d.operation,
            cause: cause_text(d.cause),
            class: d.error.class,
            code: d.error.code.clone(),
            message: d.error.message.clone(),
            has_details: !d.error.details.is_empty() || !d.error.fields.is_empty(),
            system: matches!(d.invocation.actor, Actor::System { .. }),
            stored: None,
            reused: None,
        };
        let id = d.id;
        Box::pin(async move {
            match self.mode {
                Mode::Panic => panic!("a sink bug"),
                Mode::Hang => std::future::pending::<()>().await,
                Mode::Record => {}
            }
            let mut told = told;
            if let (Some(engine), Some(id)) = (self.engine.get(), id) {
                told.stored = Some(engine.peek(&told.doctype, id).await.unwrap().is_some());
            }
            if self.reuse
                && let (Some(engine), Some(id)) = (self.engine.get(), id)
            {
                let mut again = Document::new(n("customer"), id);
                again.set(n("name"), "Again");
                told.reused = Some(engine.create(&inv(), again).await.is_ok());
            }
            self.told.lock().unwrap().push(told);
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------------------------

struct Fx {
    engine: Arc<Engine<FakeBackend>>,
    log: Arc<Mutex<Vec<Seen>>>,
    sink: Arc<Sink>,
}

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut defs = definitions();
    for def in &mut defs {
        if def.name == "customer" {
            def.title_field = Some("name".to_owned());
        }
    }
    Registry::new(defs).unwrap().effective()
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("participant.context").unwrap(),
        actor: Actor::System {
            component: n("context_tests"),
        },
        command_id: CommandId::new("cmd-5").ok(),
        language: None,
        now: now(),
    }
}

fn user_inv() -> Invocation {
    Invocation {
        actor: Actor::User {
            id: RecordId::new_v7(),
            via: None,
        },
        ..inv()
    }
}

struct Opts {
    refuse: Refuse,
    node: Option<&'static str>,
    sink: Option<Mode>,
    authorizer: Option<Arc<dyn Authorizer>>,
    timeout: Option<Duration>,
    reuse: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            refuse: Refuse::Never,
            node: None,
            sink: Some(Mode::Record),
            authorizer: None,
            timeout: None,
            reuse: false,
        }
    }
}

async fn fx(opts: Opts) -> Fx {
    let probe = Arc::new(Probe);
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", probe.clone());
    let mut engine = Engine::new(
        FakeBackend::new(),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_controllers(vec![probe]);
    if let Some(node) = opts.node {
        engine = engine.with_node(NodeId::new(node));
    }
    if let Some(authorizer) = opts.authorizer {
        engine = engine.with_authorizer(authorizer);
    }
    if let Some(timeout) = opts.timeout {
        engine = engine.with_denial_timeout(timeout);
    }
    engine.prepare().await.unwrap();
    let log: Arc<Mutex<Vec<Seen>>> = Arc::default();
    engine.set_participants(vec![Arc::new(Recorder {
        log: log.clone(),
        refuse: opts.refuse,
        spent: std::sync::atomic::AtomicBool::new(false),
    }) as Arc<dyn TxParticipant<_>>]);
    let sink = Arc::new(Sink {
        told: Mutex::new(Vec::new()),
        engine: OnceLock::new(),
        mode: opts.sink.unwrap_or(Mode::Record),
        reuse: opts.reuse,
    });
    if opts.sink.is_some() {
        engine.set_denial_sink(sink.clone());
    }
    let engine = Arc::new(engine);
    sink.engine.set(engine.clone()).ok();
    Fx { engine, log, sink }
}

impl Fx {
    fn seen(&self) -> Vec<(String, Operation, String)> {
        std::mem::take(&mut *self.log.lock().unwrap())
            .into_iter()
            .map(|s| (s.phase.to_owned() + ":" + &s.doctype, s.op, s.cause))
            .collect()
    }

    fn told(&self) -> Vec<Told> {
        std::mem::take(&mut *self.sink.told.lock().unwrap())
    }

    async fn customer(&self, name: &str) -> Document {
        let mut doc = Document::new(n("customer"), RecordId::new_v7());
        doc.set(n("name"), name);
        self.engine.create(&inv(), doc).await.unwrap().value
    }

    /// A customer at version 2, so that version 1 is stale.
    async fn bumped(&self) -> Document {
        let c = self.customer("Acme").await;
        self.engine
            .update(
                &inv(),
                "customer",
                c.id(),
                rename("Acme Ltd"),
                Version::FIRST,
            )
            .await
            .unwrap()
            .value
    }

    async fn posted(&self) -> Document {
        let customer = self.customer("Acme").await;
        let mut doc = valid_invoice();
        doc.set(n("customer"), customer.id());
        let draft = self.engine.create(&inv(), doc).await.unwrap().value;
        self.engine
            .post(
                &inv(),
                "sales_invoice",
                draft.id(),
                draft.version().unwrap(),
            )
            .await
            .unwrap()
            .value
    }
}

const STALE: Version = Version::FIRST;

fn causes(seen: &[(String, Operation, String)]) -> Vec<String> {
    seen.iter().map(|(_, _, c)| c.clone()).collect()
}

fn rename(name: &str) -> DocumentPatch {
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("name"), name.into());
    patch
}

// ---------------------------------------------------------------------------------------------
// The reason for a write
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_create_and_update_are_direct() {
    let f = fx(Opts::default()).await;
    let c = f.customer("Acme").await;
    f.engine
        .update(
            &inv(),
            "customer",
            c.id(),
            rename("Acme Ltd"),
            Version::FIRST,
        )
        .await
        .unwrap();
    let seen = f.seen();
    assert_eq!(seen.len(), 4, "{seen:?}");
    assert!(seen.iter().all(|(_, _, c)| c == "direct"), "{seen:?}");
}

#[tokio::test]
async fn a_write_made_by_a_method_says_which_method() {
    let f = fx(Opts::default()).await;
    let c = f.customer("Acme").await;
    f.seen();
    f.engine
        .call_method(&inv(), "customer", c.id(), "rename_once", json!({}), None)
        .await
        .unwrap();
    let seen = f.seen();
    assert_eq!(
        seen,
        [
            (
                "before:customer".to_owned(),
                Operation::Update,
                "method:rename_once".to_owned()
            ),
            (
                "after:customer".to_owned(),
                Operation::Update,
                "method:rename_once".to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn the_two_halves_of_a_correction_are_told_apart() {
    let f = fx(Opts::default()).await;
    let posted = f.posted().await;
    f.seen();
    f.engine
        .correct(
            &inv(),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap();
    let seen = f.seen();
    assert_eq!(
        seen,
        [
            (
                "before:sales_invoice".to_owned(),
                Operation::Cancel,
                "correct_cancel".to_owned()
            ),
            (
                "after:sales_invoice".to_owned(),
                Operation::Cancel,
                "correct_cancel".to_owned()
            ),
            (
                "before:sales_invoice".to_owned(),
                Operation::Create,
                "correct_amend".to_owned()
            ),
            (
                "after:sales_invoice".to_owned(),
                Operation::Create,
                "correct_amend".to_owned()
            ),
        ]
    );
}

#[tokio::test]
async fn a_write_a_step_makes_through_the_repository_is_a_step_write() {
    let f = fx(Opts::default()).await;
    f.customer("Parent").await;
    let seen = f.seen();
    // The parent is written directly; the step's child is a step write; then the engine is
    // back to direct for nothing further.
    assert_eq!(
        causes(&seen),
        ["direct", "direct", "step", "step"],
        "{seen:?}"
    );
}

#[tokio::test]
async fn the_node_is_carried_when_set_and_absent_otherwise() {
    let with = fx(Opts {
        node: Some("node-7"),
        ..Opts::default()
    })
    .await;
    with.customer("Acme").await;
    let seen = with.log.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(seen.iter().all(|s| s.node.as_deref() == Some("node-7")));

    let without = fx(Opts::default()).await;
    without.customer("Acme").await;
    assert!(without.log.lock().unwrap().iter().all(|s| s.node.is_none()));
}

// ---------------------------------------------------------------------------------------------
// Denials
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_permission_denial_is_reported_once_after_rollback_for_a_user() {
    let f = fx(Opts {
        authorizer: Some(Arc::new(NoUpdates)),
        ..Opts::default()
    })
    .await;
    let c = f.customer("Acme").await;
    let error = f
        .engine
        .update(
            &user_inv(),
            "customer",
            c.id(),
            rename("Other"),
            Version::FIRST,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    let t = &told[0];
    assert_eq!(
        (t.doctype.as_str(), t.id, t.op, t.cause.as_str()),
        ("customer", Some(c.id()), Operation::Update, "direct")
    );
    assert_eq!(
        (t.class, t.code.as_str()),
        (ErrorClass::PermissionDenied, "permission.denied")
    );
    assert!(!t.system, "the sink sees the acting user");
}

#[tokio::test]
async fn a_version_conflict_is_reported() {
    let f = fx(Opts::default()).await;
    let c = f.bumped().await;
    f.seen();
    f.told();
    let error = f
        .engine
        .update(&inv(), "customer", c.id(), rename("X"), STALE)
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::StaleVersion);
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].class, ErrorClass::StaleVersion);
    assert_eq!(told[0].code, error.code);
}

#[tokio::test]
async fn a_validation_failure_is_reported_for_the_system_principal_too() {
    let f = fx(Opts::default()).await;
    // `name` is required.
    let doc = Document::new(n("customer"), RecordId::new_v7());
    let id = doc.id();
    let error = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_eq!(error.class, ErrorClass::Validation);
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].op, Operation::Create);
    assert_eq!(told[0].id, Some(id));
    assert!(told[0].system, "the system principal is reported too");
    assert_eq!(told[0].code, error.code);
}

#[tokio::test]
async fn the_sink_runs_after_rollback_and_gets_no_message_or_details() {
    // The participant refuses `after_write`: the document was written, then rolled back.
    let f = fx(Opts {
        refuse: Refuse::After(Operation::Create),
        ..Opts::default()
    })
    .await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    let error = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_eq!(error.code, "test.participant_late");
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        told[0].stored,
        Some(false),
        "not in storage when the sink ran"
    );
    assert_eq!(told[0].message, "", "no message reaches the sink");
    assert!(!told[0].has_details);
}

#[tokio::test]
async fn a_participant_refusal_shows_its_own_code_whatever_its_class() {
    let f = fx(Opts {
        refuse: Refuse::Before(Operation::Create),
        ..Opts::default()
    })
    .await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    f.engine.create(&inv(), doc).await.unwrap_err();
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].code, "test.participant_refused");
    assert_eq!(told[0].class, ErrorClass::InvalidState);
}

#[tokio::test]
async fn a_refused_method_is_reported_with_its_name() {
    let f = fx(Opts {
        authorizer: Some(Arc::new(NoUpdates)),
        ..Opts::default()
    })
    .await;
    let c = f.customer("Acme").await;
    // The method's own write is refused (the authorizer refuses updates).
    let error = f
        .engine
        .call_method(
            &user_inv(),
            "customer",
            c.id(),
            "rename_once",
            json!({}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0].cause, "method:rename_once");
}

#[tokio::test]
async fn nothing_is_reported_on_success_or_for_a_missing_document() {
    let f = fx(Opts::default()).await;
    let c = f.customer("Acme").await;
    f.engine
        .update(
            &inv(),
            "customer",
            c.id(),
            rename("Acme Ltd"),
            Version::FIRST,
        )
        .await
        .unwrap();
    // A read of a document that does not exist, and an update of one.
    let missing = RecordId::new_v7();
    f.engine
        .load(&inv(), "customer", missing)
        .await
        .unwrap_err();
    let error = f
        .engine
        .update(&inv(), "customer", missing, rename("X"), Version::FIRST)
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::NotFound);
    assert!(f.told().is_empty());
}

#[tokio::test]
async fn a_panicking_sink_changes_nothing() {
    let f = fx(Opts {
        sink: Some(Mode::Panic),
        ..Opts::default()
    })
    .await;
    let c = f.bumped().await;
    f.seen();
    f.told();
    let error = f
        .engine
        .update(&inv(), "customer", c.id(), rename("X"), STALE)
        .await
        .unwrap_err();
    assert_eq!(
        error.class,
        ErrorClass::StaleVersion,
        "the operation's own error"
    );
}

#[tokio::test]
async fn a_hanging_sink_is_cut_off_at_the_timeout() {
    let f = fx(Opts {
        sink: Some(Mode::Hang),
        timeout: Some(Duration::from_millis(100)),
        ..Opts::default()
    })
    .await;
    let c = f.bumped().await;
    f.seen();
    f.told();
    let started = Instant::now();
    let error = f
        .engine
        .update(&inv(), "customer", c.id(), rename("X"), STALE)
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::StaleVersion);
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(90),
        "waited for the sink: {took:?}"
    );
    assert!(took < Duration::from_secs(1), "but not for ever: {took:?}");
}

#[tokio::test]
async fn without_a_sink_refusals_are_unchanged() {
    let f = fx(Opts {
        sink: None,
        ..Opts::default()
    })
    .await;
    let c = f.bumped().await;
    f.seen();
    f.told();
    let error = f
        .engine
        .update(&inv(), "customer", c.id(), rename("X"), STALE)
        .await
        .unwrap_err();
    assert_eq!(error.class, ErrorClass::StaleVersion);
    assert!(f.told().is_empty());
}

#[tokio::test]
async fn a_method_call_hidden_as_not_found_is_still_reported_as_the_refusal_it_was() {
    let f = fx(Opts {
        authorizer: Some(Arc::new(NoReads)),
        ..Opts::default()
    })
    .await;
    let c = f.customer("Acme").await;
    let error = f
        .engine
        .call_method(
            &user_inv(),
            "customer",
            c.id(),
            "rename_once",
            json!({}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "method.not_found", "the caller learns nothing");
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        told[0].code, "permission.denied",
        "the sink gets the real refusal"
    );
    assert_eq!(told[0].cause, "method:rename_once");
    assert!(!told[0].system);
}

#[tokio::test]
async fn the_sink_runs_when_the_refused_transaction_has_released_its_row() {
    // The create is refused after its row was inserted. The sink creates a document under the
    // same id: that can only succeed once the refused transaction has rolled back.
    let f = fx(Opts {
        refuse: Refuse::After(Operation::Create),
        reuse: true,
        timeout: Some(Duration::from_secs(1)),
        ..Opts::default()
    })
    .await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    f.engine.create(&inv(), doc).await.unwrap_err();
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        told[0].reused,
        Some(true),
        "the id was free again: {told:?}"
    );
}

#[tokio::test]
async fn a_refusal_in_the_amend_half_is_reported_as_the_create_of_the_amendment() {
    let switch = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f = fx(Opts {
        authorizer: Some(Arc::new(NoCreates(switch.clone()))),
        ..Opts::default()
    })
    .await;
    let posted = f.posted().await;
    f.told();
    // From now on creating is refused: the cancel half is allowed, the amend half is not.
    switch.store(true, std::sync::atomic::Ordering::SeqCst);
    let error = f
        .engine
        .correct(
            &inv(),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");
    let told = f.told();
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        (told[0].op, told[0].cause.as_str()),
        (Operation::Create, "correct_amend")
    );
}

#[tokio::test]
async fn a_step_write_inside_a_correction_leaves_the_amend_cause_in_place() {
    // After the amendment is created, nothing may reset the cause early: the amend's own
    // write is still `correct_amend` even though the cancel's steps wrote in between.
    let f = fx(Opts::default()).await;
    let posted = f.posted().await;
    f.seen();
    f.engine
        .correct(
            &inv(),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        causes(&f.seen()),
        [
            "correct_cancel",
            "correct_cancel",
            "correct_amend",
            "correct_amend"
        ]
    );
}
