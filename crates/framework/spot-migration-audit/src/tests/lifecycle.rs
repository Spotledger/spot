//! The full lifecycle recorded in order, the fold back into a run, and `explain`.

use futures::executor::block_on;
use spot_audit_chain::{AuditChain, CheckpointPolicy, LocalSigner, Verifier};

use super::{SITE, recorder, summary};
use crate::events::{ApplyEvent, ApprovalEvent, FinishEvent, RehearsalEvent, RunSpec, kinds};
use crate::explain::explain;
use crate::recorder::RunRecorder;
use crate::run::{
    ApprovalDecision, ArtifactKind, ArtifactRef, Outcome, PlanRef, RehearsalVerdict, RunFilter,
    RunId,
};
use crate::store::{ChainRunStore, RunStore};

fn spec(id: &str, actor: &str, at: i64) -> RunSpec {
    RunSpec {
        id: RunId::new(id).unwrap(),
        plan_digest: "plan-digest-abc".to_owned(),
        plan: PlanRef {
            artifact: ArtifactRef::of(
                ArtifactKind::Plan,
                format!("plans/{id}.json"),
                b"plan bytes",
            ),
            summary: summary(),
        },
        actor: actor.to_owned(),
        at,
    }
}

fn artifact(kind: ArtifactKind, location: &str) -> ArtifactRef {
    ArtifactRef::of(kind, location, location.as_bytes())
}

#[test]
fn full_lifecycle_is_recorded_in_order() {
    let (recorder, ring) = recorder(1);
    let handle = block_on(recorder.begin(spec("run-1", "user:alice", 1_000))).unwrap();

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
    block_on(recorder.record_rehearsal(
        &handle,
        RehearsalEvent {
            report: artifact(ArtifactKind::RehearsalReport, "reports/run-1.json"),
            verdict: RehearsalVerdict::Clean,
            findings: 0,
            at: 1_200,
        },
    ))
    .unwrap();
    block_on(recorder.record_apply(
        &handle,
        ApplyEvent {
            journal: artifact(ArtifactKind::ApplyJournal, "journals/run-1.json"),
            steps: 4,
            at: 1_300,
        },
    ))
    .unwrap();
    block_on(recorder.finish(
        &handle,
        FinishEvent {
            outcome: Outcome::Verified,
            at: 1_400,
        },
    ))
    .unwrap();

    let entries = block_on(recorder.chain().read_range(1, u64::MAX)).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(
        entries
            .iter()
            .map(|e| e.record.kind.as_str())
            .collect::<Vec<_>>(),
        vec![
            kinds::RUN_PLANNED,
            kinds::RUN_APPROVED,
            kinds::RUN_REHEARSED,
            kinds::RUN_APPLIED,
            kinds::RUN_FINISHED,
        ]
    );
    assert!(entries.iter().all(|e| e.record.masked));

    let store = ChainRunStore::new(recorder.store(), SITE);
    let evidence = block_on(store.get(handle.id())).unwrap().unwrap();
    let run = &evidence.run;
    assert_eq!(run.plan_digest, "plan-digest-abc");
    assert_eq!(run.actor, "user:alice");
    assert_eq!(run.started, 1_000);
    assert_eq!(run.ended, Some(1_400));
    assert_eq!(run.duration_micros(), Some(400));
    assert_eq!(run.outcome, Outcome::Verified);
    assert_eq!(run.approvals.len(), 1);
    assert_eq!(run.approvers(), vec!["user:bob"]);
    assert!(!run.rejected());
    let rehearsal = run.rehearsal.as_ref().unwrap();
    assert_eq!(rehearsal.verdict, RehearsalVerdict::Clean);
    assert!(rehearsal.verdict.admits_apply());
    assert_eq!(rehearsal.findings, 0);
    assert!(run.apply_journal_ref.is_some());
    assert_eq!(run.plan.summary.changes, 4);
    assert_eq!(evidence.first_seq(), Some(1));
    assert_eq!(evidence.last_seq(), Some(5));

    let explanation = explain(run);
    assert!(explanation.headline.contains("run-1"));
    assert!(explanation.headline.contains("user:alice"));
    assert!(explanation.what_changed[0].contains("4 thing(s)"));
    assert!(explanation.who_approved[0].contains("user:bob"));
    assert!(explanation.rehearsal.is_some());
    assert!(explanation.what_happened.contains("Verify passed"));
    assert!(explanation.render().contains("Result:"));

    let verifier = Verifier::new(&ring);
    assert!(block_on(store.verify(&verifier)).unwrap().is_ok());
}

#[test]
fn list_filters_by_outcome_actor_and_date() {
    let (recorder, _ring) = recorder(2);
    // run-a: alice, verified at 1000..1200.
    let a = block_on(recorder.begin(spec("run-a", "user:alice", 1_000))).unwrap();
    block_on(recorder.finish(
        &a,
        FinishEvent {
            outcome: Outcome::Verified,
            at: 1_200,
        },
    ))
    .unwrap();
    // run-b: bob, failed at 2000..2100.
    let b = block_on(recorder.begin(spec("run-b", "user:bob", 2_000))).unwrap();
    block_on(recorder.finish(
        &b,
        FinishEvent {
            outcome: Outcome::Failed,
            at: 2_100,
        },
    ))
    .unwrap();
    // run-c: alice, still planned at 3000.
    let _c = block_on(recorder.begin(spec("run-c", "user:alice", 3_000))).unwrap();

    let store = ChainRunStore::new(recorder.store(), SITE);
    let all = block_on(store.list(RunFilter::all())).unwrap();
    assert_eq!(
        all.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["run-a", "run-b", "run-c"]
    );

    let verified = block_on(store.list(RunFilter {
        outcome: Some(Outcome::Verified),
        ..RunFilter::all()
    }))
    .unwrap();
    assert_eq!(verified.len(), 1);
    assert_eq!(verified[0].id.as_str(), "run-a");

    let by_actor = block_on(store.list(RunFilter {
        actor: Some("user:alice".to_owned()),
        ..RunFilter::all()
    }))
    .unwrap();
    assert_eq!(
        by_actor.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        vec!["run-a", "run-c"]
    );

    let window = block_on(store.list(RunFilter {
        started_after: Some(1_500),
        started_before: Some(2_500),
        ..RunFilter::all()
    }))
    .unwrap();
    assert_eq!(window.len(), 1);
    assert_eq!(window[0].id.as_str(), "run-b");

    assert!(
        block_on(store.get(&RunId::new("run-c").unwrap()))
            .unwrap()
            .is_some()
    );
    assert!(
        block_on(store.get(&RunId::new("missing").unwrap()))
            .unwrap()
            .is_none()
    );
}

#[test]
fn duplicate_run_and_non_terminal_finish_are_refused() {
    let (recorder, _ring) = recorder(3);
    let handle = block_on(recorder.begin(spec("run-x", "user:alice", 1))).unwrap();
    let again = block_on(recorder.begin(spec("run-x", "user:alice", 2))).unwrap_err();
    assert_eq!(again.code(), crate::codes::DUPLICATE_RUN);

    let early = block_on(recorder.finish(
        &handle,
        FinishEvent {
            outcome: Outcome::Applied,
            at: 3,
        },
    ))
    .unwrap_err();
    assert_eq!(early.code(), crate::codes::BAD_OUTCOME);
}

#[test]
fn a_handle_from_another_site_is_refused() {
    let (recorder, _ring) = recorder(4);
    let signer = std::sync::Arc::new(LocalSigner::from_seed([9u8; 32]));
    let other_chain = AuditChain::new(
        super::TestStore::default(),
        "site-2",
        signer,
        CheckpointPolicy { every: 0 },
    );
    let other = crate::recorder::ChainRunRecorder::new(other_chain);
    let foreign = block_on(other.begin(spec("run-z", "user:alice", 1))).unwrap();

    let refused = block_on(recorder.record_approval(
        &foreign,
        ApprovalEvent {
            approver: "user:bob".to_owned(),
            role: "Migration Approver".to_owned(),
            decision: ApprovalDecision::Approved,
            at: 2,
        },
    ))
    .unwrap_err();
    assert_eq!(refused.code(), crate::codes::WRONG_SITE);
}

#[test]
fn run_id_and_artifact_ref_are_checked() {
    assert!(RunId::new("").is_err());
    assert!(RunId::new("has space").is_err());
    assert!(RunId::new("run:1").is_ok());
    let reference = ArtifactRef::of(ArtifactKind::Plan, "plans/x.json", b"x");
    assert!(reference.digest_well_formed());
    assert_eq!(reference.bytes, 1);
}

#[test]
fn a_bad_actor_is_refused_but_the_audit_forms_are_accepted() {
    let (recorder, _ring) = recorder(12);
    let mut bad = spec("run-bad", "user:alice", 1);
    bad.actor = "alice".to_owned();
    assert_eq!(
        block_on(recorder.begin(bad)).unwrap_err().code(),
        crate::codes::BAD_ACTOR
    );

    block_on(recorder.begin(spec("run-system", "system", 1))).unwrap();
    block_on(recorder.begin(spec("run-agent", "agent:support", 1))).unwrap();
}
