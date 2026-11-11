//! End-to-end scenarios for `spot-bpm-versioning`.
//!
//! These are the acceptance stories from WP-BPM-09.T1: a Case stays on its pinned version after
//! a new one is published, a migration with a mapping gap is refused, a bulk migration resumes,
//! and cancelling an upstream document raises the declared return step for every delivery while
//! the Case file keeps the chain intact.
#![allow(clippy::unwrap_used, clippy::expect_used, deprecated)]

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use serde_json::json;
use spot_app_api::prelude::{Name, OffsetDateTime, Version};
use spot_bpm_documents::ids::record_id_for;
use spot_bpm_documents::{CaseFileStore, Entry, MemoryCaseFile, Operation, Touch};
use spot_bpm_engine::{
    CaseId, CaseRecord, EffectLedger, HandlerRegistry, Harness, NodeKind, ProcessDefinition,
    RecordingHandler,
};
use spot_bpm_model::{ProcessVersion, from_text};
use spot_bpm_versioning::{
    BatchLedger, BatchRequest, BatchStatus, BulkMigrator, CancellationWatcher, CompensationJournal,
    CompensationModel, CompensationOutcome, CompensationPath, DeclaredCompensator, ItemState,
    MemoryCaseIndex, MemoryCompensationSink, MemoryMigrationTarget, MigrationMapping,
    NEEDS_DECISION_TASK, TriggerRecord, VersioningError, pinned, plan_migration, publish,
};
use spot_dag::NodeId;
use spot_dag_exec::RunStatus;

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn version(text: &str) -> ProcessVersion {
    from_text(text).expect("the process text must parse")
}

fn definition(version: &ProcessVersion) -> ProcessDefinition {
    ProcessDefinition::compile(version).expect("the process must compile")
}

fn harness() -> Harness {
    let ledger = Arc::new(EffectLedger::new());
    let mut registry = HandlerRegistry::logic_default();
    registry.register(RecordingHandler::new(NodeKind::Document, ledger.clone()));
    Harness::with_ledger(registry, ledger)
}

const V1: &str = r#"
name = "versioned_order"
app = "versioning_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "review"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "review"
"#;

const V2: &str = r#"
name = "versioned_order"
app = "versioning_test"
version = 2
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "approve"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "approve"
"#;

fn case_at_review(name: &str) -> (CaseRecord, ProcessVersion, ProcessVersion) {
    let v1 = version(V1);
    let v2 = version(V2);
    let harness = harness();
    let case = CaseId::new(name).unwrap();
    block_on(harness.start(&v1, case.clone(), name)).unwrap();
    block_on(harness.step(&case)).unwrap();
    let record = block_on(harness.engine().load(&case)).unwrap();
    assert!(
        record
            .run
            .ready_nodes()
            .contains(&NodeId::new("review").unwrap())
    );
    (record, v1, v2)
}

#[test]
fn a_case_on_v1_completes_on_v1_after_v2_is_published() {
    let harness = harness();
    let v1 = version(V1);
    let v2 = version(V2);
    let case = CaseId::new("pinned_case").unwrap();
    block_on(harness.start(&v1, case.clone(), "pinned-key")).unwrap();
    let before = block_on(harness.engine().load(&case)).unwrap();
    let v1_hash = definition(&v1).compiled.dag.hash().to_string();

    let report = publish(harness.engine(), &v2, std::slice::from_ref(&before)).unwrap();
    assert_eq!(report.published.version, 2);
    assert_eq!(report.running.len(), 1);
    assert_eq!(report.running[0].pin.version, 1);
    assert_eq!(report.on_other_versions, vec![case.clone()]);
    assert_eq!(
        report.running[0].pin,
        spot_bpm_versioning::VersionRef::of(&before)
    );

    let finished = block_on(harness.drive(&case, 20)).unwrap();
    assert_eq!(finished.run.status, RunStatus::Completed);
    assert_eq!(
        finished.run.dag_version, 1,
        "the Case stays on its pinned version"
    );
    assert_eq!(finished.run.content_hash, v1_hash);
    assert!(
        finished
            .run
            .nodes
            .contains_key(&NodeId::new("review").unwrap())
    );
    assert!(
        !finished
            .run
            .nodes
            .contains_key(&NodeId::new("approve").unwrap())
    );

    let pin = pinned(harness.engine(), &finished).unwrap();
    assert_eq!(pin.version, 1);
    assert_eq!(pin.hash, v1_hash);
}

#[test]
fn migration_with_an_unmapped_current_node_is_refused() {
    let (record, v1, v2) = case_at_review("gap_case");
    let error = plan_migration(
        &record,
        &definition(&v1),
        &definition(&v2),
        &MigrationMapping::new(),
    )
    .expect_err("the current node has no mapping");
    assert!(matches!(error, VersioningError::UnmappedNode { .. }));
    assert_eq!(error.code(), "bpm_versioning.unmapped_node");
}

#[test]
fn migration_maps_only_the_current_node_and_preserves_history() {
    let (record, v1, v2) = case_at_review("mapped_case");
    let mut mapping = MigrationMapping::new().with_node(
        NodeId::new("review").unwrap(),
        NodeId::new("approve").unwrap(),
    );
    mapping = mapping.requiring("order.total");
    let missing = plan_migration(&record, &definition(&v1), &definition(&v2), &mapping)
        .expect_err("the variable has no value and no default");
    assert!(matches!(missing, VersioningError::VariableMissing { .. }));
    assert_eq!(missing.code(), "bpm_versioning.variable_missing");

    mapping = mapping.with_variable_default("order.total", json!(7));
    let plan = plan_migration(&record, &definition(&v1), &definition(&v2), &mapping).unwrap();
    assert_eq!(plan.current_node(), "review");
    assert_eq!(plan.target_node(), "approve");
    assert_eq!(plan.node_changes().len(), 1);
    assert!(plan.history_preserved());
    assert_eq!(plan.variable_changes().len(), 1);
    assert!(plan.render_text().contains("review -> approve"));
    assert_eq!(plan.migrated().run.dag_version, 2);
    assert!(
        plan.migrated()
            .run
            .nodes
            .contains_key(&NodeId::new("approve").unwrap())
    );
    assert_eq!(
        plan.migrated().variables.get("order"),
        Some(&json!({"total": 7}))
    );

    let target = MemoryMigrationTarget::new().with_case(record);
    let applied = plan.apply(&target, now(), Some("user_1")).unwrap();
    assert_eq!(applied.to.version, 2);
    assert_eq!(applied.audit.topic, "sys_bpm_case_migrated");
    assert_eq!(applied.audit.actor.as_deref(), Some("user_1"));
    assert_eq!(target.audits().len(), 1);
    let stored = target.load(plan.case()).unwrap();
    assert_eq!(stored.run.dag_version, 2);
    assert_eq!(stored.run.nodes.len(), 2);
}

#[test]
fn a_bulk_migration_resumes_and_refuses_a_second_identity_start() {
    let (record_a, v1, v2) = case_at_review("batch_a");
    let (record_b, _, _) = case_at_review("batch_b");
    let mapping = MigrationMapping::new().with_node(
        NodeId::new("review").unwrap(),
        NodeId::new("approve").unwrap(),
    );
    let plan_a = plan_migration(&record_a, &definition(&v1), &definition(&v2), &mapping).unwrap();
    let plan_b = plan_migration(&record_b, &definition(&v1), &definition(&v2), &mapping).unwrap();
    let target = MemoryMigrationTarget::new()
        .with_case(record_a.clone())
        .with_case(record_b.clone());
    let request = BatchRequest {
        kind_version: 1,
        process: "versioned_order".to_owned(),
        from_version: 1,
        to_version: 2,
        cases: vec![record_a.run.id.clone(), record_b.run.id.clone()],
        dry_run: false,
        actor: Some("user_1".to_owned()),
    };
    let ledger = BatchLedger::new();
    let run = ledger.start(&request, now()).unwrap();
    let mut migrator = BulkMigrator::new(&ledger, &target).with_plan(plan_a);
    migrator = migrator.with_plan(plan_b);
    migrator.checkpoint_every = 1;

    let first = migrator.run(&run.id, now(), Some(1)).unwrap();
    assert_eq!(first.processed, 1);
    assert_eq!(first.status, BatchStatus::Running);
    assert_eq!(first.checkpoint, 1);

    let rest = migrator.run(&run.id, now(), None).unwrap();
    assert_eq!(rest.status, BatchStatus::Succeeded);
    assert_eq!(rest.counters.done, 2);
    assert_eq!(target.audits().len(), 2);

    let again = ledger
        .start(&request, now())
        .expect_err("identity is complete");
    assert!(matches!(
        again,
        VersioningError::BatchAlreadyCompleted { .. }
    ));
}

#[test]
fn a_bulk_migration_skips_a_case_with_no_plan() {
    let (record_a, v1, v2) = case_at_review("skip_a");
    let (record_b, _, _) = case_at_review("skip_b");
    let mapping = MigrationMapping::new().with_node(
        NodeId::new("review").unwrap(),
        NodeId::new("approve").unwrap(),
    );
    let plan_a = plan_migration(&record_a, &definition(&v1), &definition(&v2), &mapping).unwrap();
    let target = MemoryMigrationTarget::new()
        .with_case(record_a.clone())
        .with_case(record_b.clone());
    let request = BatchRequest {
        kind_version: 1,
        process: "versioned_order".to_owned(),
        from_version: 1,
        to_version: 2,
        cases: vec![record_a.run.id.clone(), record_b.run.id.clone()],
        dry_run: false,
        actor: None,
    };
    let ledger = BatchLedger::new();
    let run = ledger.start(&request, now()).unwrap();
    let migrator = BulkMigrator::new(&ledger, &target).with_plan(plan_a);
    let progress = migrator.run(&run.id, now(), None).unwrap();
    assert_eq!(progress.status, BatchStatus::SucceededWithSkips);
    assert_eq!(progress.counters.done, 1);
    assert_eq!(progress.counters.skipped, 1);
    let stored = ledger.load(&run.id).unwrap();
    let skipped = stored
        .items
        .iter()
        .find(|item| item.case == record_b.run.id)
        .and_then(|item| item.outcome.as_ref())
        .unwrap();
    assert_eq!(skipped.state, ItemState::Skipped);
    assert_eq!(skipped.code.as_deref(), Some("bpm_versioning.plan_missing"));
}

fn entry(case: &CaseId, node: &str, doctype: &str, document: &str) -> Entry {
    Entry {
        case: case.clone(),
        node: NodeId::new(node).unwrap(),
        operation: Operation::Submit,
        doctype: Name::new(doctype).unwrap(),
        document: record_id_for(document),
        version: Version(1),
        compensation: Some(Operation::Cancel),
        receipt: json!({"record": {"version": 1}}),
    }
}

fn cancellation_trigger(event: &str, doctype: &str, document: &str) -> TriggerRecord {
    TriggerRecord {
        event: record_id_for(event),
        doctype: Name::new(doctype).unwrap(),
        document: record_id_for(document),
        subject_version: Some(Version(2)),
    }
}

fn compensation_watcher(
    entries: &[Entry],
    model: CompensationModel,
) -> (
    CancellationWatcher,
    Arc<MemoryCaseFile>,
    Arc<CompensationJournal>,
) {
    let file = Arc::new(MemoryCaseFile::new());
    for entry in entries {
        file.record(entry).unwrap();
    }
    let index = Arc::new(MemoryCaseIndex::new());
    index.record_all(entries);
    let journal = Arc::new(CompensationJournal::new());
    let sink = Arc::new(MemoryCompensationSink::new(journal.clone()));
    let compensator = Arc::new(DeclaredCompensator::new(Arc::new(model), sink));
    let watcher = CancellationWatcher::new(index, file.clone(), compensator, journal.clone());
    (watcher, file, journal)
}

#[test]
fn cancelling_the_order_raises_a_return_step_for_each_delivery() {
    let case = CaseId::new("order_case").unwrap();
    let entries = vec![
        entry(&case, "order", "sales_order", "order_1"),
        entry(&case, "delivery_one", "delivery_note", "delivery_1"),
        entry(&case, "delivery_two", "delivery_note", "delivery_2"),
    ];
    let model = CompensationModel::new()
        .with(
            NodeId::new("delivery_one").unwrap(),
            CompensationPath::ReturnStep {
                step: NodeId::new("return_one").unwrap(),
            },
        )
        .with(
            NodeId::new("delivery_two").unwrap(),
            CompensationPath::ReturnStep {
                step: NodeId::new("return_two").unwrap(),
            },
        );
    let (watcher, file, journal) = compensation_watcher(&entries, model);
    let trigger = cancellation_trigger("cancel_1", "sales_order", "order_1");

    let report = watcher.consume(&trigger).unwrap();
    assert_eq!(report.cases, 1);
    assert_eq!(report.outcomes.len(), 2);
    assert!(
        report
            .outcomes
            .iter()
            .all(|outcome| matches!(outcome, CompensationOutcome::Raised { .. }))
    );
    assert_eq!(journal.raised().len(), 2);
    assert!(!watcher.may_continue(&case));
    assert_eq!(
        file.entries(&case).unwrap().len(),
        3,
        "the Case file chain stays intact"
    );

    // The outbox redelivers the same event: the watcher raises nothing twice.
    let again = watcher.consume(&trigger).unwrap();
    assert!(again.outcomes.is_empty());
    assert_eq!(again.duplicates, 2);
    assert_eq!(journal.raised().len(), 2);
    assert_eq!(file.entries(&case).unwrap().len(), 3);

    watcher
        .journal()
        .complete_step(&case, &NodeId::new("return_one").unwrap());
    assert!(!watcher.may_continue(&case));
    watcher
        .journal()
        .complete_step(&case, &NodeId::new("return_two").unwrap());
    assert!(watcher.may_continue(&case));
}

#[test]
fn a_cancellation_without_a_declaration_parks_the_case_for_a_decision() {
    let case = CaseId::new("parked_case").unwrap();
    let entries = vec![
        entry(&case, "order", "sales_order", "order_2"),
        entry(&case, "delivery_one", "delivery_note", "delivery_3"),
    ];
    let (watcher, _file, journal) = compensation_watcher(&entries, CompensationModel::new());
    let trigger = cancellation_trigger("cancel_2", "sales_order", "order_2");
    let report = watcher.consume(&trigger).unwrap();
    assert_eq!(report.outcomes.len(), 1);
    match &report.outcomes[0] {
        CompensationOutcome::Parked {
            task, dependencies, ..
        } => {
            assert_eq!(task, NEEDS_DECISION_TASK);
            assert!(dependencies.contains(&Touch::new(
                Name::new("delivery_note").unwrap(),
                record_id_for("delivery_3")
            )));
        }
        other => panic!("expected a parked decision, got {other:?}"),
    }
    assert!(!watcher.may_continue(&case));
    assert_eq!(journal.parked().len(), 1);
    journal.release(&case);
    assert!(watcher.may_continue(&case));
}

#[test]
fn a_migration_plan_is_idempotent_when_applied_twice() {
    let (record, v1, v2) = case_at_review("idempotent_case");
    let mapping = MigrationMapping::new().with_node(
        NodeId::new("review").unwrap(),
        NodeId::new("approve").unwrap(),
    );
    let plan = plan_migration(&record, &definition(&v1), &definition(&v2), &mapping).unwrap();
    let target = MemoryMigrationTarget::new().with_case(record);
    let first = plan.apply(&target, now(), None).unwrap();
    let second = plan.apply(&target, now(), None).unwrap();
    assert_eq!(first.plan_digest, second.plan_digest);
    assert_eq!(target.audits().len(), 1);
}
