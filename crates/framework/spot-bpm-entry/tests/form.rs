//! The form step end to end: the handler opens the task, and completing it saves the draft and
//! the step's transition in one storage transaction.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use common::{n, now};
use serde_json::{Value as Json, json};
use spot_app_api::error::{StorageOp, map_storage_error};
use spot_app_api::prelude::{Decimal, RecordId};
use spot_app_api::{AppError, ErrorClass};
use spot_bpm_engine::{IdempotencyKey, NodeCtx, NodeHandler, NodeKind, NodeOutcome, WaitSpec};
use spot_bpm_entry::{
    DraftSave, DraftWriter, EntryError, ExecPlanner, FieldIssue, FormConfigs, FormNodeConfig,
    FormService, FormStepHandler, FormSubmission, MemoryRequests, NoSignalEngine, RequestAuthority,
    RunLabel, StaticDag, TaskInbox, generate_entry_process,
};
use spot_bpm_tasks::{InMemoryTaskStore, MapDirectory, NoApprovers, TaskService, TaskStatus};
use spot_dag::{CompiledDag, NodeId, Published};
use spot_dag_exec::unit::WorkFuture;
use spot_dag_exec::{
    ExecStore, InMemoryExecStore, NodeOutcome as ExecOutcome, NodeStatus, Run, RunId, Transition,
    Unit, claim, complete, start,
};
use spot_storage_contract::{
    Catalog, DocumentData, DocumentStore, Fields, Isolation, Name, RowLock, StorageBackend,
    Transaction, TxOptions, Value, contract_tests,
};
use spot_storage_fake::{FakeBackend, FakeTx};

const REQUEST: &str = "req1";

fn party_doc(id: RecordId, name: &str, credit: &str) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), Value::Text(name.to_owned()));
    fields.insert(
        n("credit_limit"),
        Value::Decimal(Decimal::from_str(credit).unwrap()),
    );
    fields.insert(n("parent_party"), Value::Null);
    for field in [
        "note", "since", "opens_at", "seen_at", "active", "visits", "profile", "logo",
    ] {
        fields.insert(n(field), Value::Null);
    }
    DocumentData {
        entity: n("party"),
        id,
        fields,
        children: BTreeMap::new(),
    }
}

/// The record type's lifecycle, stood in for by the storage fake's `party` entity: it validates
/// the name and the credit limit, and writes inside the unit's transaction.
#[derive(Default)]
struct PartyWriter {
    saved: Mutex<Vec<DraftSave>>,
}

impl DraftWriter<FakeTx> for PartyWriter {
    fn validate(&self, save: &DraftSave) -> Result<(), Vec<FieldIssue>> {
        let mut issues = Vec::new();
        let name = save.values.get("name").and_then(Json::as_str).unwrap_or("");
        if name.trim().is_empty() {
            issues.push(FieldIssue::new(
                "name",
                "document.required",
                "A name is required.",
            ));
        }
        let credit = save
            .values
            .get("credit_limit")
            .and_then(Json::as_str)
            .unwrap_or("0");
        if Decimal::from_str(credit).is_err() {
            issues.push(FieldIssue::new(
                "credit_limit",
                "document.invalid",
                "Not a number.",
            ));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    fn write<'t>(self: Arc<Self>, tx: &'t mut FakeTx, save: Arc<DraftSave>) -> WorkFuture<'t> {
        Box::pin(async move {
            self.saved.lock().unwrap().push((*save).clone());
            let name = save.values.get("name").and_then(Json::as_str).unwrap_or("");
            let credit = save
                .values
                .get("credit_limit")
                .and_then(Json::as_str)
                .unwrap_or("0");
            tx.insert(&party_doc(save.document, name, credit))
                .await
                .map_err(|error| map_storage_error(&error, StorageOp::Write))?;
            if name == "boom" {
                // The row is written, then the write fails: the transaction must roll it back.
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "test.boom",
                    "write failed",
                ));
            }
            Ok(())
        })
    }
}

/// A `Unit` over the in-memory exec store and the storage fake.
struct FakeUnit {
    store: Arc<InMemoryExecStore>,
    backend: FakeBackend,
    catalog: Arc<Catalog>,
}

impl Unit<FakeTx> for FakeUnit {
    fn commit_with<'a>(
        &'a self,
        transition: &'a Transition,
        expected_version: u64,
        work: &'a mut (dyn for<'t> FnMut(&'t mut FakeTx) -> WorkFuture<'t> + Send),
    ) -> spot_dag_exec::ExecFuture<'a, Run> {
        Box::pin(async move {
            let current = self.store.load_run(&transition.run).await?.unwrap();
            if current.version != expected_version {
                return Err(spot_dag_exec::ExecError::VersionConflict {
                    current: current.version,
                    expected: expected_version,
                });
            }
            let options = TxOptions::new(n("form_test"), Isolation::ReadCommitted);
            let mut tx = self.backend.begin(self.catalog.clone(), options).await?;
            work(&mut tx).await?;
            tx.commit().await?;
            self.store.apply(transition, expected_version).await
        })
    }
}

struct Fixed(FormNodeConfig);

impl FormConfigs for Fixed {
    fn config(&self, _request: &str, _node: &str) -> Result<FormNodeConfig, EntryError> {
        Ok(self.0.clone())
    }
}

struct World {
    dag: Arc<CompiledDag>,
    store: Arc<InMemoryExecStore>,
    backend: FakeBackend,
    catalog: Arc<Catalog>,
    tasks: Arc<TaskService>,
    writer: Arc<PartyWriter>,
    requests: Arc<MemoryRequests>,
    service: FormService<FakeTx, FakeUnit>,
    handler: FormStepHandler,
}

fn form_config(mode: &str, assignee: &str) -> FormNodeConfig {
    FormNodeConfig::parse(
        &json!({
            "record_type": "party",
            "fields": ["name", "credit_limit"],
            "assignee": assignee,
            "draft_mode": mode,
            "on_abort": "discard",
        }),
        "enter",
    )
    .unwrap()
}

fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

async fn apply(store: &InMemoryExecStore, run: &Run, t: &Transition) -> Run {
    store.apply(t, run.version).await.unwrap()
}

async fn world(config: FormNodeConfig) -> World {
    let compiled = spot_bpm_model::compile(&generate_entry_process("party").unwrap()).unwrap();
    let dag = Arc::new(compiled.dag);
    let store = Arc::new(InMemoryExecStore::new());
    let pin = Published {
        version: dag.version(),
        hash: dag.hash().clone(),
    };
    let plan = start(&dag, &pin, RunId::new(REQUEST).unwrap(), "biz-1", now()).unwrap();
    let mut run = store.create_run(plan).await.unwrap();
    // The trigger runs, then the engine claims the form node before invoking its handler.
    let t = claim(&run, &node("start"), now()).unwrap();
    run = apply(&store, &run, &t).await;
    let done = ExecOutcome::Done {
        output: None,
        branches: Vec::new(),
    };
    let t = complete(&dag, &run, &node("start"), done, now()).unwrap();
    run = apply(&store, &run, &t).await;
    let t = claim(&run, &node("enter"), now()).unwrap();
    store.apply(&t, run.version).await.unwrap();

    let backend = FakeBackend::new();
    let catalog = Arc::new(contract_tests::catalog().unwrap());
    backend.prepare(&catalog).await.unwrap();

    let directory = Arc::new(MapDirectory::new().with_role("hr", "u2"));
    let tasks = Arc::new(
        TaskService::new(Arc::new(InMemoryTaskStore::new()), Arc::new(NoSignalEngine))
            .with_directory(directory.clone()),
    );
    let inbox = Arc::new(TaskInbox::new(
        tasks.clone(),
        directory,
        Arc::new(NoApprovers),
    ));
    let writer = Arc::new(PartyWriter::default());
    let unit = Arc::new(FakeUnit {
        store: store.clone(),
        backend: backend.clone(),
        catalog: catalog.clone(),
    });
    let planner = Arc::new(ExecPlanner::new(
        store.clone() as Arc<dyn ExecStore>,
        Arc::new(StaticDag(dag.clone())),
    ));
    let requests = MemoryRequests::new().shared();
    let service = FormService::new(
        unit,
        planner,
        writer.clone() as Arc<dyn DraftWriter<FakeTx>>,
        inbox.clone(),
        Arc::new(Fixed(config)),
        requests.clone(),
    );
    let handler = FormStepHandler::new(inbox, RunLabel::shipped());
    World {
        dag,
        store,
        backend,
        catalog,
        tasks,
        writer,
        requests,
        service,
        handler,
    }
}

/// Runs the handler for the form node the way the engine does.
fn run_handler(w: &World, config: &Json, variables: &mut Json) -> NodeOutcome {
    let case = RunId::new(REQUEST).unwrap();
    let node_id = node("enter");
    let outgoing = Vec::new();
    let mut ctx = NodeCtx {
        case: &case,
        node: &node_id,
        kind: NodeKind::Unknown("form".to_owned()),
        attempt: 1,
        key: IdempotencyKey::new(case.clone(), node_id.clone(), 1),
        is_retry: false,
        now: now(),
        deadline: None,
        principal: Some("starter"),
        config,
        outgoing: &outgoing,
        variables,
        cancelled: false,
    };
    w.handler.run(&mut ctx)
}

fn config_json(mode: &str, assignee: &str) -> Json {
    json!({
        "record_type": "party",
        "fields": ["name", "credit_limit"],
        "assignee": assignee,
        "draft_mode": mode,
    })
}

fn submission(ticket: &str, actor: &str, draft: Option<RecordId>, values: Json) -> FormSubmission {
    FormSubmission {
        ticket: ticket.to_owned(),
        request: REQUEST.to_owned(),
        node: "enter".to_owned(),
        attempt: 1,
        actor: actor.to_owned(),
        draft,
        expected: None,
        values,
        now: now(),
    }
}

async fn party_exists(w: &World, id: RecordId) -> bool {
    let options = TxOptions::new(n("form_check"), Isolation::ReadCommitted);
    let mut tx = w.backend.begin(w.catalog.clone(), options).await.unwrap();
    let found = tx
        .load(&Name::new("party").unwrap(), id, RowLock::None)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    found.is_some()
}

async fn node_status(w: &World, id: &str) -> NodeStatus {
    let run = w
        .store
        .load_run(&RunId::new(REQUEST).unwrap())
        .await
        .unwrap()
        .unwrap();
    run.node(&node(id)).unwrap().status
}

fn ticket_of(outcome: NodeOutcome) -> String {
    match outcome {
        NodeOutcome::Wait(WaitSpec::HumanTask { key, .. }) => key,
        other => panic!("expected a human-task wait, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_form_step_happy_path_saves_the_draft_and_the_transition_together() {
    let w = world(form_config("create", "u1")).await;
    let mut variables = json!({});
    let ticket = ticket_of(run_handler(
        &w,
        &config_json("create", "u1"),
        &mut variables,
    ));

    // A task appears in the assignee's inbox; the node waits on it.
    let inbox = w.tasks.my_tasks("u1", now()).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].id.to_string(), ticket);
    assert_eq!(inbox[0].title, "Enter the details for this Request");
    assert_eq!(node_status(&w, "enter").await, NodeStatus::Running);

    let draft = RecordId::new_v7();
    let done = w
        .service
        .complete(&submission(
            &ticket,
            "u1",
            Some(draft),
            json!({"name": "Acme Traders", "credit_limit": "2500.50"}),
        ))
        .await
        .unwrap();
    assert!(!done.already_done);
    assert_eq!(done.draft, Some(("party".to_owned(), draft)));

    // The draft exists, the node is done and the next node is released: one commit.
    assert!(party_exists(&w, draft).await);
    assert_eq!(node_status(&w, "enter").await, NodeStatus::Done);
    assert_eq!(node_status(&w, "submit").await, NodeStatus::Ready);

    // The draft was saved as the assigned person, flagged as the request's, under the
    // request's own command context.
    let saved = w.writer.saved.lock().unwrap().clone();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].actor, "u1");
    assert_eq!(
        saved[0].values.get("owned_by_request"),
        Some(&json!(REQUEST))
    );
    let step = w
        .requests
        .authenticate(&saved[0].context.command_id(), "u1")
        .expect("the draft write runs under a grant for this person and attempt");
    assert_eq!(
        (step.request.as_str(), step.node.as_str(), step.attempt),
        (REQUEST, "enter", 1)
    );
    assert!(
        w.requests
            .authenticate(&saved[0].context.command_id(), "mallory")
            .is_none()
    );

    // The task is completed.
    let task = w.tasks.load(&inbox[0].id).unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    // The dag the planner used is the one the run was started from.
    assert_eq!(w.dag.key(), "entry_party");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_validation_failure_keeps_the_step_open_and_returns_field_errors() {
    let w = world(form_config("create", "u1")).await;
    let ticket = ticket_of(run_handler(
        &w,
        &config_json("create", "u1"),
        &mut json!({}),
    ));
    let draft = RecordId::new_v7();

    let error = w
        .service
        .complete(&submission(
            &ticket,
            "u1",
            Some(draft),
            json!({"name": "  ", "credit_limit": "x"}),
        ))
        .await
        .expect_err("the lifecycle's validation refuses the values");
    let EntryError::FormInvalid { fields } = &error else {
        panic!("expected field errors, got {error:?}");
    };
    let names: Vec<&str> = fields.iter().map(|issue| issue.field.as_str()).collect();
    assert_eq!(names, vec!["name", "credit_limit"]);
    // The app error carries them as field errors for the form.
    let app = error.to_app_error();
    assert_eq!(app.code, "bpm_entry.form_invalid");
    assert_eq!(app.fields.len(), 2);

    // Nothing was written and the step is still open.
    assert!(!party_exists(&w, draft).await);
    assert_eq!(node_status(&w, "enter").await, NodeStatus::Running);
    let task = w.tasks.my_tasks("u1", now()).await.unwrap();
    assert_eq!(task[0].status, TaskStatus::Open);
    assert!(
        w.writer.saved.lock().unwrap().is_empty(),
        "no write was attempted"
    );

    // A corrected submission completes the same step.
    let ok = w
        .service
        .complete(&submission(
            &ticket,
            "u1",
            Some(draft),
            json!({"name": "Acme", "credit_limit": "10"}),
        ))
        .await;
    assert!(ok.is_ok(), "{ok:?}");
    assert_eq!(node_status(&w, "enter").await, NodeStatus::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_draft_write_rolls_back_the_transition_too() {
    let w = world(form_config("create", "u1")).await;
    let ticket = ticket_of(run_handler(
        &w,
        &config_json("create", "u1"),
        &mut json!({}),
    ));
    let draft = RecordId::new_v7();
    let error = w
        .service
        .complete(&submission(
            &ticket,
            "u1",
            Some(draft),
            json!({"name": "boom", "credit_limit": "1"}),
        ))
        .await
        .expect_err("the write fails inside the unit");
    assert_eq!(error.code(), "bpm_entry.commit_failed");
    assert!(!party_exists(&w, draft).await, "the row rolled back");
    assert_eq!(
        node_status(&w, "enter").await,
        NodeStatus::Running,
        "the node did not move"
    );
    assert_eq!(node_status(&w, "submit").await, NodeStatus::Pending);
    // A retry with good values succeeds from exactly where it was.
    let again = w
        .service
        .complete(&submission(
            &ticket,
            "u1",
            Some(draft),
            json!({"name": "Fine", "credit_limit": "1"}),
        ))
        .await;
    assert!(again.is_ok(), "{again:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_assigned_person_completes_and_a_repeat_is_a_no_op() {
    let w = world(form_config("create", "role:hr")).await;
    let ticket = ticket_of(run_handler(
        &w,
        &config_json("create", "role:hr"),
        &mut json!({}),
    ));
    let values = json!({"name": "Acme", "credit_limit": "1"});

    let stranger = w
        .service
        .complete(&submission(
            &ticket,
            "mallory",
            Some(RecordId::new_v7()),
            values.clone(),
        ))
        .await
        .expect_err("not assigned");
    assert_eq!(stranger.code(), "bpm_entry.step_not_open");
    assert_eq!(node_status(&w, "enter").await, NodeStatus::Running);

    // A member of the role is asked, and may complete it.
    let draft = RecordId::new_v7();
    assert!(
        w.service
            .complete(&submission(&ticket, "u2", Some(draft), values.clone()))
            .await
            .is_ok()
    );
    let repeat = w
        .service
        .complete(&submission(&ticket, "u2", Some(RecordId::new_v7()), values))
        .await
        .unwrap();
    assert!(repeat.already_done);
    assert_eq!(
        w.writer.saved.lock().unwrap().len(),
        1,
        "the repeat wrote nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn values_outside_the_step_or_floating_point_are_refused() {
    let w = world(form_config("create", "u1")).await;
    let ticket = ticket_of(run_handler(
        &w,
        &config_json("create", "u1"),
        &mut json!({}),
    ));
    for values in [
        json!({"name": "A", "credit_limit": 2500.5}),
        json!({"name": "A", "credit_limit": "1", "nickname": "x"}),
        json!({"name": "A", "credit_limit": "1", "owned_by_request": "other"}),
    ] {
        let error = w
            .service
            .complete(&submission(&ticket, "u1", Some(RecordId::new_v7()), values))
            .await
            .expect_err("refused before any write");
        assert_eq!(error.code(), "bpm_entry.form_invalid");
    }
    assert!(w.writer.saved.lock().unwrap().is_empty());
    assert_eq!(node_status(&w, "enter").await, NodeStatus::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_collect_step_creates_no_record() {
    let config = FormNodeConfig::parse(
        &json!({"fields": ["reason"], "assignee": "u1", "draft_mode": "collect"}),
        "enter",
    )
    .unwrap();
    let w = world(config).await;
    let ticket = ticket_of(run_handler(
        &w,
        &json!({"fields": ["reason"], "assignee": "u1", "draft_mode": "collect"}),
        &mut json!({}),
    ));
    let done = w
        .service
        .complete(&submission(
            &ticket,
            "u1",
            None,
            json!({"reason": "annual leave"}),
        ))
        .await
        .unwrap();
    assert_eq!(done.draft, None);
    assert_eq!(
        done.output,
        json!({"collected": {"reason": "annual leave"}})
    );
    assert!(w.writer.saved.lock().unwrap().is_empty());
    assert_eq!(node_status(&w, "enter").await, NodeStatus::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_handler_reports_bad_config_and_nobody_to_ask() {
    let w = world(form_config("create", "u1")).await;
    let fail = |outcome: NodeOutcome| match outcome {
        NodeOutcome::Fail { code, .. } => code,
        other => panic!("expected a failure, got {other:?}"),
    };
    // A draft-writing step needs a record type.
    let code = fail(run_handler(
        &w,
        &json!({"draft_mode": "create", "assignee": "u1"}),
        &mut json!({}),
    ));
    assert_eq!(code, "bpm_entry.config_invalid");
    // An update step needs the draft it updates.
    let code = fail(run_handler(
        &w,
        &json!({"record_type": "party", "draft_mode": "update", "assignee": "u1"}),
        &mut json!({}),
    ));
    assert_eq!(code, "bpm_entry.config_invalid");
    // Unknown keys are refused, not ignored.
    let code = fail(run_handler(
        &w,
        &json!({"record_type": "party", "colour": "red"}),
        &mut json!({}),
    ));
    assert_eq!(code, "bpm_entry.config_invalid");
    // Nobody to ask: the request parks instead of guessing a person.
    let code = fail(run_handler(
        &w,
        &json!({"record_type": "party"}),
        &mut json!({}),
    ));
    assert_eq!(code, "bpm_entry.no_assignee");
    // An update step finds its draft in the variable an earlier step set.
    let mut variables = json!({"draft": {"id": "d1"}});
    let outcome = run_handler(
        &w,
        &json!({"record_type": "party", "draft_mode": "update", "assignee": "u1", "draft_from": "draft"}),
        &mut variables,
    );
    let ticket = ticket_of(outcome);
    let task = w
        .tasks
        .load(&spot_bpm_tasks::TaskId::new(&ticket).unwrap())
        .unwrap();
    assert_eq!(task.subject.as_deref(), Some("d1"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_the_task_twice_finds_the_same_task() {
    let w = world(form_config("create", "u1")).await;
    let config = config_json("create", "u1");
    let first = ticket_of(run_handler(&w, &config, &mut json!({})));
    let second = ticket_of(run_handler(&w, &config, &mut json!({})));
    assert_eq!(first, second, "a retried node does not open a second task");
    assert_eq!(w.tasks.my_tasks("u1", now()).await.unwrap().len(), 1);
}
