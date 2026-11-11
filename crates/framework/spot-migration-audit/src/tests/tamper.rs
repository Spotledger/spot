//! Tampering with one stored entry is detected by chain verification.

use futures::executor::block_on;
use spot_audit_chain::{FailureKind, PayloadValue, Verifier};

use super::{SITE, recorder};
use crate::events::{ApprovalEvent, FinishEvent, RunSpec};
use crate::recorder::RunRecorder;
use crate::run::{ApprovalDecision, ArtifactKind, ArtifactRef, Outcome, PlanRef, RunId};
use crate::store::{ChainRunStore, RunStore};

fn full_run(
    seed: u8,
) -> (
    crate::recorder::ChainRunRecorder<super::TestStore>,
    spot_audit_chain::KeyRing,
    RunId,
) {
    let (recorder, ring) = recorder(seed);
    let spec = RunSpec {
        id: RunId::new("run-tamper").unwrap(),
        plan_digest: "plan-digest".to_owned(),
        plan: PlanRef {
            artifact: ArtifactRef::of(ArtifactKind::Plan, "plans/run-tamper.json", b"plan"),
            summary: super::summary(),
        },
        actor: "user:alice".to_owned(),
        at: 1_000,
    };
    let handle = block_on(recorder.begin(spec)).unwrap();
    block_on(recorder.record_approval(
        &handle,
        ApprovalEvent {
            approver: "user:bob".to_owned(),
            role: "Migration Approver".to_owned(),
            decision: ApprovalDecision::Approved,
            at: 1_100,
        },
    ))
    .unwrap();
    block_on(recorder.finish(
        &handle,
        FinishEvent {
            outcome: Outcome::Verified,
            at: 1_200,
        },
    ))
    .unwrap();
    (recorder, ring, handle.id().clone())
}

#[test]
fn a_changed_payload_value_is_detected() {
    let (recorder, ring, id) = full_run(5);
    let verifier = Verifier::new(&ring);
    let store = ChainRunStore::new(recorder.store(), SITE);
    assert!(block_on(store.verify(&verifier)).unwrap().is_ok());

    // The approval entry (seq 2): change the approver's name, a text value that still decodes.
    recorder.store().tamper(2, |entry| {
        entry.record.payload.insert(
            "approver".to_owned(),
            PayloadValue::Text("user:mallory".to_owned()),
        );
    });

    let report = block_on(store.verify(&verifier)).unwrap();
    assert!(!report.is_ok());
    assert!(report.has(FailureKind::HashMismatch));
    assert_eq!(report.failures[0].seq, 2);

    // The fold still reads the (altered) record: only the verifier proves it was changed.
    let evidence = block_on(store.get(&id)).unwrap().unwrap();
    assert_eq!(evidence.run.approvals[0].approver, "user:mallory");
}

#[test]
fn a_changed_actor_is_detected() {
    let (recorder, ring, _id) = full_run(6);
    let verifier = Verifier::new(&ring);
    let store = ChainRunStore::new(recorder.store(), SITE);

    recorder.store().tamper(1, |entry| {
        entry.record.actor = "user:mallory".to_owned();
    });

    let report = block_on(store.verify(&verifier)).unwrap();
    assert!(report.has(FailureKind::HashMismatch));
    assert_eq!(report.failures[0].seq, 1);
}

#[test]
fn a_removed_entry_is_detected() {
    let (recorder, ring, _id) = full_run(7);
    let verifier = Verifier::new(&ring);
    let store = ChainRunStore::new(recorder.store(), SITE);

    recorder.store().remove(2);

    let report = block_on(store.verify(&verifier)).unwrap();
    assert!(!report.is_ok());
    assert!(report.has(FailureKind::SeqGap) || report.has(FailureKind::LinkBroken));
}
