//! Transaction participants (WP-DOC-04.T3): code that runs in the document's own transaction
//! around every storage write, on the fake backend.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{definitions, n, now, valid_invoice};
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation, MethodContext};
use spot_app_api::controller::{MethodDecl, StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, DocState, Document, DocumentPatch, ErrorClass,
    HookPoint, Operation, Phase, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{Engine, TxParticipant, V7Ids, WriteIntent, WriteRecord};
use spot_meta::Registry;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, FeedPosition, OutboxEvent, RowLock, Transaction};
use spot_storage_fake::FakeBackend;

/// One call a participant saw.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    who: &'static str,
    phase: &'static str,
    doctype: String,
    op: Operation,
    /// The version of the document as loaded, if any.
    old: Option<Version>,
    /// The version of the document as given, if any (the new version for an `after_write`).
    new: Option<Version>,
    /// The version an `after_write` was told about.
    version: Option<Version>,
    /// For an `after_write`: whether the same transaction could read the document back.
    visible: Option<bool>,
}

type Log = Arc<Mutex<Vec<Seen>>>;
/// A second log for steps, so the order against participants is visible.
type Steps = Arc<Mutex<Vec<String>>>;

#[derive(Clone, Copy, PartialEq)]
enum Fail {
    Never,
    Before(Operation),
    After(Operation),
    PanicBefore(Operation),
}

struct Recorder {
    who: &'static str,
    log: Log,
    fail: Fail,
}

fn refusal(code: &str) -> AppError {
    AppError::new(ErrorClass::PermissionDenied, code, "refused")
}

impl<Tx: Transaction> TxParticipant<Tx> for Recorder {
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(Seen {
                who: self.who,
                phase: "before",
                doctype: write.doctype.to_owned(),
                op: write.operation,
                old: write.old.and_then(Document::version),
                new: write.new.and_then(Document::version),
                version: None,
                visible: None,
            });
            if self.fail == Fail::Before(write.operation) {
                return Err(refusal("test.before_refused"));
            }
            assert!(
                self.fail != Fail::PanicBefore(write.operation),
                "a participant bug"
            );
            Ok(())
        })
    }

    fn after_write<'a>(
        &'a self,
        tx: &'a mut Tx,
        write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let id = write.new.or(write.old).unwrap().id();
            let entity = n(write.doctype);
            let stored = tx.load(&entity, id, RowLock::None).await.unwrap();
            self.log.lock().unwrap().push(Seen {
                who: self.who,
                phase: "after",
                doctype: write.doctype.to_owned(),
                op: write.operation,
                old: write.old.and_then(Document::version),
                new: write.new.and_then(Document::version),
                version: write.version,
                visible: Some(stored.is_some()),
            });
            if self.fail == Fail::After(write.operation) {
                return Err(refusal("test.after_refused"));
            }
            Ok(())
        })
    }
}

/// A controller of `customer` that logs its `after_*` steps and offers methods that write.
struct Probe {
    steps: Steps,
}

fn point(phase: Phase, op: Operation) -> HookPoint {
    HookPoint::of(phase, Target::Op(op))
}

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        [Operation::Create, Operation::Update, Operation::Delete]
            .into_iter()
            .map(|op| StepDecl {
                name: StepName::new(&format!("probe:after_{op:?}").to_lowercase()).unwrap(),
                point: point(Phase::After, op),
                priority: 0,
                budget: Duration::from_secs(5),
            })
            .collect()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        vec![MethodDecl::new(n("rename_twice"), true)]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        self.steps.lock().unwrap().push(step.as_str().to_owned());
        Box::pin(async { Ok(()) })
    }

    fn call_method<'a>(
        &'a self,
        _method: &'a str,
        _args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            let id = ctx.doc().id();
            let mut version = ctx.doc().version().unwrap();
            for name in ["First", "Second"] {
                let mut patch = DocumentPatch::default();
                patch.fields.insert(n("name"), name.into());
                version = ctx
                    .update("customer", id, patch, version)
                    .await?
                    .version()
                    .unwrap();
            }
            Ok(json!({"version": version.0}))
        })
    }
}

struct Fx {
    engine: Engine<FakeBackend>,
    log: Log,
    steps: Steps,
}

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut defs = definitions();
    for def in &mut defs {
        if def.name == "customer" {
            def.title_field = Some("name".to_owned());
        }
        if def.name == "sales_invoice" {
            for field in &mut def.fields {
                if field.name == "remarks" {
                    field.allow_on_posted = true;
                }
            }
        }
    }
    Registry::new(defs).unwrap().effective()
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("participants.test").unwrap(),
        actor: Actor::System {
            component: n("participant_tests"),
        },
        command_id: CommandId::new("cmd-9").ok(),
        language: None,
        now: now(),
    }
}

fn recorder(who: &'static str, log: &Log, fail: Fail) -> Arc<Recorder> {
    Arc::new(Recorder {
        who,
        log: log.clone(),
        fail,
    })
}

async fn fx_with(fails: &[(&'static str, Fail)]) -> Fx {
    let steps: Steps = Arc::default();
    let probe = Arc::new(Probe {
        steps: steps.clone(),
    });
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", probe.clone());
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_controllers(vec![probe]);
    engine.prepare().await.unwrap();
    let log: Log = Arc::default();
    let participants: Vec<Arc<dyn TxParticipant<_>>> = fails
        .iter()
        .map(|(who, fail)| recorder(who, &log, *fail) as Arc<dyn TxParticipant<_>>)
        .collect();
    engine.set_participants(participants);
    Fx { engine, log, steps }
}

async fn fx() -> Fx {
    fx_with(&[("a", Fail::Never)]).await
}

impl Fx {
    fn seen(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }

    async fn outbox(&self) -> Vec<OutboxEvent> {
        self.engine
            .backend()
            .read_outbox(FeedPosition::START, 10_000)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.event)
            .collect()
    }

    async fn customer(&self, name: &str) -> Document {
        let mut doc = Document::new(n("customer"), RecordId::new_v7());
        doc.set(n("name"), name);
        self.engine.create(&inv(), doc).await.unwrap().value
    }

    async fn invoice(&self) -> Document {
        let customer = self.customer("Acme").await;
        let mut doc = valid_invoice();
        doc.set(n("customer"), customer.id());
        self.engine.create(&inv(), doc).await.unwrap().value
    }

    async fn posted(&self) -> Document {
        let draft = self.invoice().await;
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

/// `(phase, operation)` of each call, in order.
fn shape(seen: &[Seen]) -> Vec<(&'static str, Operation)> {
    seen.iter().map(|s| (s.phase, s.op)).collect()
}

fn names(seen: &[Seen]) -> Vec<String> {
    seen.iter()
        .map(|s| format!("{}:{}", s.who, s.phase))
        .collect()
}

#[tokio::test]
async fn two_participants_run_in_registration_order_around_the_write_and_before_the_after_steps() {
    let f = fx_with(&[("a", Fail::Never), ("b", Fail::Never)]).await;
    f.customer("Acme").await;
    let seen = f.seen();
    assert_eq!(names(&seen), ["a:before", "b:before", "a:after", "b:after"]);
    // `after_write` ran before the engine's `after_create` step.
    assert_eq!(*f.steps.lock().unwrap(), ["probe:after_create"]);
    assert!(
        seen.iter()
            .all(|s| s.doctype == "customer" && s.op == Operation::Create),
        "{seen:?}"
    );
    // A create: nothing loaded, the draft given, the first version written, readable at once.
    assert_eq!(seen[0].old, None);
    assert_eq!(seen[2].old, None);
    assert_eq!(seen[2].version, Some(Version::FIRST));
    assert_eq!(seen[2].new, Some(Version::FIRST));
    assert_eq!(seen[2].visible, Some(true), "same transaction sees the row");
}

#[tokio::test]
async fn a_swapped_participant_list_applies_to_the_next_write_and_none_means_none() {
    let f = fx().await;
    f.customer("One").await;
    assert_eq!(f.seen().len(), 2);
    f.engine.set_participants(Vec::new());
    f.customer("Two").await;
    assert!(f.seen().is_empty());
}

#[tokio::test]
async fn a_before_write_refusal_changes_nothing() {
    let f = fx_with(&[("a", Fail::Never), ("b", Fail::Before(Operation::Create))]).await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    let id = doc.id();
    let error = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_eq!(
        error.code, "test.before_refused",
        "the participant's error, unchanged"
    );
    assert!(f.engine.peek("customer", id).await.unwrap().is_none());
    assert!(f.outbox().await.is_empty());
    assert!(f.steps.lock().unwrap().is_empty(), "no after_* step ran");
    // The first ran its before_write; nobody ran an after_write.
    assert_eq!(names(&f.seen()), ["a:before", "b:before"]);
}

#[tokio::test]
async fn an_after_write_failure_rolls_back_the_document_the_events_and_the_steps_never_ran() {
    let f = fx_with(&[("a", Fail::After(Operation::Create))]).await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    let id = doc.id();
    let error = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_eq!(error.code, "test.after_refused");
    assert!(f.engine.peek("customer", id).await.unwrap().is_none());
    assert!(
        f.outbox().await.is_empty(),
        "the lifecycle event rolled back too"
    );
    assert!(f.steps.lock().unwrap().is_empty(), "no after_* step ran");
    assert_eq!(
        shape(&f.seen()),
        [("before", Operation::Create), ("after", Operation::Create)]
    );
}

#[tokio::test]
async fn update_gives_the_loaded_document_and_the_new_version() {
    let f = fx().await;
    let created = f.customer("Acme").await;
    f.seen();
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("name"), "Acme Ltd".into());
    let updated = f
        .engine
        .update(&inv(), "customer", created.id(), patch, Version::FIRST)
        .await
        .unwrap()
        .value;
    let seen = f.seen();
    assert_eq!(
        shape(&seen),
        [("before", Operation::Update), ("after", Operation::Update)]
    );
    // As loaded: the stored version, not the patched copy's.
    assert_eq!(seen[0].old, Some(Version::FIRST));
    assert_eq!(seen[1].old, Some(Version::FIRST));
    assert_eq!(seen[1].version, updated.version());
    assert_ne!(seen[1].version, Some(Version::FIRST));
    assert_eq!(
        *f.steps.lock().unwrap(),
        ["probe:after_create", "probe:after_update"]
    );
}

#[tokio::test]
async fn post_cancel_and_update_posted_each_fire_once() {
    let f = fx().await;
    let draft = f.invoice().await;
    f.seen();

    let posted = f
        .engine
        .post(
            &inv(),
            "sales_invoice",
            draft.id(),
            draft.version().unwrap(),
        )
        .await
        .unwrap()
        .value;
    assert_eq!(
        shape(&f.seen()),
        [("before", Operation::Post), ("after", Operation::Post)]
    );

    let mut patch = DocumentPatch::default();
    patch
        .fields
        .insert(n("remarks"), Value::Text("late note".into()));
    let edited = f
        .engine
        .update(
            &inv(),
            "sales_invoice",
            posted.id(),
            patch,
            posted.version().unwrap(),
        )
        .await
        .unwrap()
        .value;
    assert_eq!(
        shape(&f.seen()),
        [
            ("before", Operation::UpdatePosted),
            ("after", Operation::UpdatePosted)
        ]
    );

    f.engine
        .cancel(
            &inv(),
            "sales_invoice",
            posted.id(),
            edited.version().unwrap(),
        )
        .await
        .unwrap();
    let seen = f.seen();
    assert_eq!(
        shape(&seen),
        [("before", Operation::Cancel), ("after", Operation::Cancel)]
    );
    assert_eq!(
        seen[1].new, seen[1].version,
        "the written document carries the new version"
    );
}

#[tokio::test]
async fn delete_fires_with_no_new_document() {
    let f = fx().await;
    let created = f.customer("Acme").await;
    f.seen();
    f.engine
        .delete(&inv(), "customer", created.id(), Version::FIRST)
        .await
        .unwrap();
    let seen = f.seen();
    assert_eq!(
        shape(&seen),
        [("before", Operation::Delete), ("after", Operation::Delete)]
    );
    assert_eq!(seen[0].old, Some(Version::FIRST));
    assert_eq!(seen[1].new, None);
    assert_eq!(seen[1].version, None);
    assert_eq!(
        seen[1].visible,
        Some(false),
        "deleted in this same transaction"
    );
    assert_eq!(
        *f.steps.lock().unwrap(),
        ["probe:after_create", "probe:after_delete"]
    );
}

#[tokio::test]
async fn a_failing_delete_participant_keeps_the_document() {
    let f = fx_with(&[("a", Fail::After(Operation::Delete))]).await;
    let created = f.customer("Acme").await;
    let error = f
        .engine
        .delete(&inv(), "customer", created.id(), Version::FIRST)
        .await
        .unwrap_err();
    assert_eq!(error.code, "test.after_refused");
    let still = f
        .engine
        .peek("customer", created.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.version(), Some(Version::FIRST));
}

#[tokio::test]
async fn rename_fires_as_a_rename() {
    let f = fx().await;
    let created = f.customer("Acme").await;
    f.seen();
    f.engine
        .rename(&inv(), "customer", created.id(), "Acme Ltd", Version::FIRST)
        .await
        .unwrap();
    let seen = f.seen();
    assert_eq!(
        shape(&seen),
        [("before", Operation::Rename), ("after", Operation::Rename)]
    );
    assert_eq!(seen[0].old, Some(Version::FIRST));
}

#[tokio::test]
async fn correct_fires_for_the_cancel_then_the_amended_draft_in_order() {
    let f = fx().await;
    let posted = f.posted().await;
    f.seen();
    let amended = f
        .engine
        .correct(
            &inv(),
            "sales_invoice",
            posted.id(),
            posted.version().unwrap(),
        )
        .await
        .unwrap()
        .value;
    let seen = f.seen();
    assert_eq!(
        shape(&seen),
        [
            ("before", Operation::Cancel),
            ("after", Operation::Cancel),
            ("before", Operation::Create),
            ("after", Operation::Create),
        ]
    );
    assert_eq!(seen[0].old, posted.version());
    assert_eq!(seen[3].old, None, "the amendment is a new record");
    assert_eq!(seen[3].version, amended.version());
    assert_eq!(amended.state(), Some(DocState::Draft));
}

#[tokio::test]
async fn a_failure_in_the_amended_draft_rolls_back_the_cancel() {
    let f = fx_with(&[("a", Fail::After(Operation::Create))]).await;
    // The seeding creates fail too, so seed with the failing participant off.
    f.engine.set_participants(Vec::new());
    let posted = f.posted().await;
    let events_before = f.outbox().await.len();
    f.engine
        .set_participants(vec![recorder("a", &f.log, Fail::After(Operation::Create))]);
    f.seen();
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
    assert_eq!(error.code, "test.after_refused");
    assert_eq!(
        shape(&f.seen()),
        [
            ("before", Operation::Cancel),
            ("after", Operation::Cancel),
            ("before", Operation::Create),
            ("after", Operation::Create),
        ]
    );
    let original = f
        .engine
        .peek("sales_invoice", posted.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        original.state(),
        Some(DocState::Posted),
        "the cancel rolled back"
    );
    assert_eq!(original.version(), posted.version());
    assert_eq!(
        f.outbox().await.len(),
        events_before,
        "no event of the correction remains"
    );
}

#[tokio::test]
async fn writes_a_method_makes_go_through_the_update_lifecycle() {
    let f = fx().await;
    let created = f.customer("Acme").await;
    f.seen();
    let out = f
        .engine
        .call_method(
            &inv(),
            "customer",
            created.id(),
            "rename_twice",
            json!({}),
            None,
        )
        .await
        .unwrap()
        .value;
    assert_eq!(out.1, json!({"version": 3}));
    let seen = f.seen();
    assert_eq!(
        shape(&seen),
        [
            ("before", Operation::Update),
            ("after", Operation::Update),
            ("before", Operation::Update),
            ("after", Operation::Update),
        ]
    );
    // One record per stored version: 2 then 3.
    let versions: Vec<_> = seen
        .iter()
        .filter(|s| s.phase == "after")
        .map(|s| s.version)
        .collect();
    assert_eq!(versions, [Some(Version(2)), Some(Version(3))]);
}

#[tokio::test]
async fn a_failing_participant_rolls_back_a_methods_writes() {
    let f = fx_with(&[("a", Fail::After(Operation::Update))]).await;
    f.engine.set_participants(Vec::new());
    let created = f.customer("Acme").await;
    f.engine
        .set_participants(vec![recorder("a", &f.log, Fail::After(Operation::Update))]);
    let error = f
        .engine
        .call_method(
            &inv(),
            "customer",
            created.id(),
            "rename_twice",
            json!({}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "test.after_refused");
    let stored = f
        .engine
        .peek("customer", created.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.version(), Some(Version::FIRST));
    assert_eq!(stored.get("name"), &Value::Text("Acme".into()));
}

#[tokio::test]
async fn a_participant_sees_exactly_one_record_per_stored_version() {
    let f = fx().await;
    let created = f.customer("Acme").await;
    let mut version = created.version().unwrap();
    for name in ["B", "C"] {
        let mut patch = DocumentPatch::default();
        patch.fields.insert(n("name"), name.into());
        version = f
            .engine
            .update(&inv(), "customer", created.id(), patch, version)
            .await
            .unwrap()
            .value
            .version()
            .unwrap();
    }
    let renamed = f
        .engine
        .rename(&inv(), "customer", created.id(), "D", version)
        .await
        .unwrap()
        .value;
    let versions: Vec<_> = f
        .seen()
        .iter()
        .filter(|s| s.phase == "after")
        .map(|s| s.version.unwrap().0)
        .collect();
    assert_eq!(versions, [1, 2, 3, 4]);
    assert_eq!(renamed.version(), Some(Version(4)));
}

#[tokio::test]
async fn read_paths_call_no_participant() {
    let f = fx().await;
    let created = f.customer("Acme").await;
    f.seen();
    f.engine
        .load(&inv(), "customer", created.id())
        .await
        .unwrap();
    f.engine.peek("customer", created.id()).await.unwrap();
    f.engine
        .field_changed(&inv(), created.clone(), "name")
        .await
        .unwrap();
    f.engine.meta("customer").await.unwrap();
    assert!(f.seen().is_empty());
}

#[tokio::test]
async fn a_stale_or_refused_operation_never_reaches_a_participant() {
    let f = fx().await;
    let created = f.customer("Acme").await;
    f.seen();
    let error = f
        .engine
        .update(
            &inv(),
            "customer",
            created.id(),
            DocumentPatch::default(),
            Version(9),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "document.stale_version");
    assert!(f.seen().is_empty());
}

#[tokio::test]
async fn a_panicking_participant_is_caught_and_rolls_back() {
    let f = fx_with(&[("a", Fail::PanicBefore(Operation::Create))]).await;
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    let id = doc.id();
    let error = f.engine.create(&inv(), doc).await.unwrap_err();
    assert_eq!(error.code, "document.internal_error");
    assert_eq!(error.class, ErrorClass::Internal);
    assert!(
        !error.message.contains("participant bug"),
        "the panic text does not leak"
    );
    assert!(f.engine.peek("customer", id).await.unwrap().is_none());
    assert!(f.outbox().await.is_empty());
    // The engine is still usable.
    f.engine.set_participants(Vec::new());
    f.customer("Next").await;
}
