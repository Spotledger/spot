#![allow(clippy::expect_used, clippy::unwrap_used)]
//! End-to-end rescue cases with fakes: restore, re-enter, block, fail and resume after a kill.

mod common;

use std::sync::Arc;

use common::{FakeInvariants, FakeReenter, principal, site};
use serde_json::json;
use spot_migration_apply::{
    AdmissionGate, InMemoryAdmissionGate, RecordingSnapshotPort, SnapshotId, SnapshotPort,
};
use spot_rescue::{
    CommittedOperation, Error, FixedOperations, KillAt, KillPoint, MemoryRestoreApprovals,
    OldDocType, OldSchema, OperationId, OperationKind, OperationSource, ReentryPlanner,
    RescueEngine, RescueOutcome, RescueRequest, RestoreApprovals, RestoreTicket, RunIdentity,
};

const T: i64 = 10_000;

fn identity() -> RunIdentity {
    RunIdentity::new("platform:migration_rescue", format!("localhost:{T}"), 1)
        .expect("a valid identity")
}

fn planner() -> ReentryPlanner {
    ReentryPlanner::new(
        OldSchema::new().with_doctype(
            "sales_invoice",
            OldDocType::new(["id", "customer", "amount"]).with_required(["id", "amount"]),
        ),
        vec![],
    )
}

fn invoice(index: usize, amount: i64) -> CommittedOperation {
    CommittedOperation::new(
        OperationId::new(format!("invoice-{index}")).expect("a valid id"),
        index as u64,
        OperationKind::Create,
        "sales_invoice",
        json!({"id": index, "customer": "acme", "amount": amount}),
        principal("alice"),
        T + 1 + index as i64,
    )
}

fn operations(count: usize, amount: i64) -> Vec<CommittedOperation> {
    (0..count).map(|index| invoice(index, amount)).collect()
}

fn ticket() -> RestoreTicket {
    RestoreTicket::two_person("restore-1", T, "alice", "bob", T).expect("two distinct approvers")
}

async fn snapshot(port: &Arc<RecordingSnapshotPort>) -> SnapshotId {
    port.take("pre-migration", T)
        .await
        .expect("the fake snapshot is taken")
}

fn engine(
    journal: Arc<spot_rescue::InMemoryRescueJournal>,
    gate: Arc<dyn AdmissionGate>,
    snapshots: Arc<dyn SnapshotPort>,
    approvals: Arc<dyn RestoreApprovals>,
    reenter: Arc<FakeReenter>,
    invariants: Arc<FakeInvariants>,
) -> RescueEngine {
    RescueEngine::new(
        site(),
        journal,
        gate,
        snapshots,
        approvals,
        reenter,
        invariants,
    )
    .with_planner(planner())
    .with_batch_size(10)
}

fn request(snapshot: SnapshotId, operations: Vec<CommittedOperation>) -> RescueRequest {
    let source: Arc<dyn OperationSource> = Arc::new(FixedOperations::new(operations));
    RescueRequest::new("rescue-1", identity(), Some(snapshot), T, T + 1_000, source)
}

#[tokio::test]
async fn three_hundred_documents_reenter_with_equal_totals() {
    let journal = Arc::new(spot_rescue::InMemoryRescueJournal::new());
    let gate: Arc<dyn AdmissionGate> = Arc::new(InMemoryAdmissionGate::new());
    let snapshots = Arc::new(RecordingSnapshotPort::new());
    let snapshot = snapshot(&snapshots).await;
    let reenter = Arc::new(FakeReenter::new());
    let invariants = Arc::new(FakeInvariants::new(reenter.clone(), 300, 1_500));
    let approvals: Arc<dyn RestoreApprovals> =
        Arc::new(MemoryRestoreApprovals::with_ticket(ticket()));
    let rescue = engine(
        journal.clone(),
        gate.clone(),
        snapshots.clone(),
        approvals,
        reenter.clone(),
        invariants,
    );
    let operations = operations(300, 5);
    let expected: i64 = operations
        .iter()
        .map(|operation| operation.payload["amount"].as_i64().unwrap_or_default())
        .sum();

    let outcome = rescue
        .run(request(snapshot.clone(), operations))
        .await
        .expect("the rescue completes");
    let RescueOutcome::Completed(report) = outcome else {
        panic!("expected a completed rescue: {outcome:?}");
    };
    assert_eq!(report.counts.planned, 300);
    assert_eq!(report.counts.done, 300);
    assert_eq!(report.counts.failed, 0);
    assert_eq!(report.counts.unattempted, 0);
    assert!(report.needs_person.is_empty());
    assert!(report.gate_open);
    assert!(gate.status().is_open());
    assert_eq!(reenter.count(), 300);
    assert_eq!(reenter.total(), expected);
    assert_eq!(reenter.total(), 1_500);
    assert!(
        journal
            .written()
            .iter()
            .any(|entry| matches!(entry.record, spot_rescue::JournalRecord::GateOpened))
    );
    let restored = snapshots.restores();
    assert_eq!(restored, 1);
    assert_eq!(report.snapshot, snapshot);
}

#[tokio::test]
async fn a_blocked_item_halts_the_run_and_is_reported() {
    let journal = Arc::new(spot_rescue::InMemoryRescueJournal::new());
    let gate: Arc<dyn AdmissionGate> = Arc::new(InMemoryAdmissionGate::new());
    let snapshots = Arc::new(RecordingSnapshotPort::new());
    let snapshot = snapshot(&snapshots).await;
    let reenter = Arc::new(FakeReenter::new());
    let invariants = Arc::new(FakeInvariants::new(reenter.clone(), 1, 5));
    let approvals: Arc<dyn RestoreApprovals> =
        Arc::new(MemoryRestoreApprovals::with_ticket(ticket()));
    let rescue = engine(
        journal,
        gate.clone(),
        snapshots,
        approvals,
        reenter.clone(),
        invariants,
    );
    let blocked = CommittedOperation::new(
        OperationId::new("widget-1").expect("a valid id"),
        1,
        OperationKind::Create,
        "widget",
        json!({"id": 1, "size": 3}),
        principal("bob"),
        T + 2,
    );
    let outcome = rescue
        .run(request(snapshot, vec![invoice(0, 5), blocked]))
        .await
        .expect("the run returns a halt report");
    let RescueOutcome::Halted(report) = outcome else {
        panic!("expected a halted rescue: {outcome:?}");
    };
    assert_eq!(report.counts.blocked, 1);
    assert_eq!(report.counts.done, 1);
    assert_eq!(report.needs_person.len(), 1);
    assert_eq!(report.needs_person[0].key.as_str(), "widget-1");
    assert_eq!(
        report.needs_person[0].original_payload,
        json!({"id": 1, "size": 3})
    );
    assert!(!report.gate_open);
    assert!(!gate.status().is_open());
    assert_eq!(reenter.count(), 1);
}

#[tokio::test]
async fn a_killed_rescue_resumes_without_repeating_an_item() {
    let journal = Arc::new(spot_rescue::InMemoryRescueJournal::new());
    let gate: Arc<dyn AdmissionGate> = Arc::new(InMemoryAdmissionGate::new());
    let snapshots = Arc::new(RecordingSnapshotPort::new());
    let snapshot = snapshot(&snapshots).await;
    let reenter = Arc::new(FakeReenter::new());
    let invariants = Arc::new(FakeInvariants::new(reenter.clone(), 300, 1_500));
    let approvals: Arc<dyn RestoreApprovals> =
        Arc::new(MemoryRestoreApprovals::with_ticket(ticket()));

    let first = engine(
        journal.clone(),
        gate.clone(),
        snapshots.clone(),
        approvals.clone(),
        reenter.clone(),
        invariants.clone(),
    )
    .with_faults(Arc::new(KillAt::new(KillPoint::AfterItem(9))));
    let error = first
        .run(request(snapshot.clone(), operations(300, 5)))
        .await
        .expect_err("the kill stops the first attempt");
    assert!(error.is_killed());
    assert!(!gate.status().is_open());
    assert_eq!(reenter.count(), 10);

    let resumed = engine(
        journal.clone(),
        gate.clone(),
        snapshots.clone(),
        approvals,
        reenter.clone(),
        invariants,
    );
    let outcome = resumed
        .run(request(snapshot, operations(300, 5)))
        .await
        .expect("the second attempt completes");
    let RescueOutcome::Completed(report) = outcome else {
        panic!("expected a completed rescue: {outcome:?}");
    };
    assert_eq!(report.counts.done, 300);
    assert_eq!(report.counts.unattempted, 0);
    assert_eq!(reenter.count(), 300);
    assert_eq!(reenter.total(), 1_500);
    assert!(gate.status().is_open());
    assert_eq!(snapshots.restores(), 1, "the restore is not repeated");
    assert_eq!(reenter.calls().len(), 300, "no item is presented twice");
    let restored_passes = journal
        .written()
        .iter()
        .filter(|entry| {
            matches!(
                entry.record,
                spot_rescue::JournalRecord::InvariantsChecked { .. }
            )
        })
        .count();
    assert_eq!(restored_passes, 2, "one pass after restore, one final");
}

#[tokio::test]
async fn a_rejected_item_stops_the_run_and_keeps_the_original() {
    let journal = Arc::new(spot_rescue::InMemoryRescueJournal::new());
    let gate: Arc<dyn AdmissionGate> = Arc::new(InMemoryAdmissionGate::new());
    let snapshots = Arc::new(RecordingSnapshotPort::new());
    let snapshot = snapshot(&snapshots).await;
    let reenter = Arc::new(FakeReenter::failing_on("invoice-2"));
    let invariants = Arc::new(FakeInvariants::new(reenter.clone(), 3, 15));
    let approvals: Arc<dyn RestoreApprovals> =
        Arc::new(MemoryRestoreApprovals::with_ticket(ticket()));
    let rescue = engine(
        journal,
        gate.clone(),
        snapshots,
        approvals,
        reenter.clone(),
        invariants,
    );
    let outcome = rescue
        .run(request(snapshot, operations(3, 5)))
        .await
        .expect("the run returns a halt report");
    let RescueOutcome::Halted(report) = outcome else {
        panic!("expected a halted rescue: {outcome:?}");
    };
    assert_eq!(report.counts.failed, 1);
    assert_eq!(report.counts.done, 2, "the two before the failure are kept");
    assert_eq!(report.needs_person.len(), 1);
    assert_eq!(report.needs_person[0].key.as_str(), "invoice-2");
    assert_eq!(
        report.needs_person[0].original_payload,
        json!({"id": 2, "customer": "acme", "amount": 5})
    );
    assert!(!gate.status().is_open());
    assert_eq!(reenter.count(), 2);
}

#[tokio::test]
async fn a_missing_restore_ticket_refuses_before_closing_the_site() {
    let journal = Arc::new(spot_rescue::InMemoryRescueJournal::new());
    let gate: Arc<dyn AdmissionGate> = Arc::new(InMemoryAdmissionGate::new());
    let snapshots = Arc::new(RecordingSnapshotPort::new());
    let snapshot = snapshot(&snapshots).await;
    let reenter = Arc::new(FakeReenter::new());
    let invariants = Arc::new(FakeInvariants::new(reenter.clone(), 1, 5));
    let rescue = engine(
        journal,
        gate.clone(),
        snapshots,
        Arc::new(MemoryRestoreApprovals::none()),
        reenter.clone(),
        invariants,
    );
    let error = rescue
        .run(request(snapshot, operations(1, 5)))
        .await
        .expect_err("no ticket means no rescue");
    assert!(matches!(error, Error::NoTicket));
    assert!(gate.status().is_open());
    assert_eq!(reenter.count(), 0);
}

#[tokio::test]
async fn a_failing_final_invariant_pass_keeps_users_out() {
    let journal = Arc::new(spot_rescue::InMemoryRescueJournal::new());
    let gate: Arc<dyn AdmissionGate> = Arc::new(InMemoryAdmissionGate::new());
    let snapshots = Arc::new(RecordingSnapshotPort::new());
    let snapshot = snapshot(&snapshots).await;
    let reenter = Arc::new(FakeReenter::new());
    let invariants = Arc::new(FakeInvariants::new(reenter.clone(), 2, 10).failing_final());
    let approvals: Arc<dyn RestoreApprovals> =
        Arc::new(MemoryRestoreApprovals::with_ticket(ticket()));
    let rescue = engine(
        journal,
        gate.clone(),
        snapshots,
        approvals,
        reenter.clone(),
        invariants,
    );
    let outcome = rescue
        .run(request(snapshot, operations(2, 5)))
        .await
        .expect("the run returns a halt report");
    let RescueOutcome::Halted(report) = outcome else {
        panic!("expected a halted rescue: {outcome:?}");
    };
    assert!(!report.gate_open);
    assert!(!gate.status().is_open());
    assert_eq!(report.counts.done, 2);
    let final_pass = report
        .invariants
        .iter()
        .find(|pass| pass.phase == Some(spot_rescue::InvariantPhase::AfterReentry))
        .expect("a final invariant pass");
    assert!(!final_pass.passed());
}
