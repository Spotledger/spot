//! Every `JournalRecord` variant survives a serde round trip, including a fingerprint whose
//! measures are the extremes of `i128`; and `FakePreImages` handles values beyond 64 bits.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::BTreeMap;

use spot_agent_tiers::{Approval, Tier};
use spot_fix_pipeline::testing::{FakePreImages, FakeWorld};
use spot_fix_pipeline::{
    AgentRef, ChangeRecord, DeploymentEpoch, FailureStage, FixError, FixPlan, FixSource, FixStep,
    JournalRecord, Mechanism, PlanPath, PreImage, RehearsalEvidence, ResidueEvidence, Risk,
    RollbackCause, RollbackEvidence, SnapshotRef, StructuralKind, UndoWindow, VerifyReport,
    ports::PreImageStore,
};
use spot_rehearsal::{Dimension, Fingerprint, FingerprintRow, FingerprintSection, InvariantReport};

fn fingerprint(high: i128, low: i128) -> Fingerprint {
    let mut measures = BTreeMap::new();
    measures.insert("debit".to_owned(), high);
    measures.insert("credit".to_owned(), low);
    let row = FingerprintRow::new(vec![Dimension::new("account", "cash")], measures);
    Fingerprint::new(vec![FingerprintSection::new(
        "trial_balance",
        "Trial balance",
        vec!["account".to_owned()],
        vec!["debit".to_owned(), "credit".to_owned()],
        vec![row],
    )])
}

fn plan() -> FixPlan {
    FixPlan::seal(
        FixSource::Structural {
            structural: StructuralKind::Migration,
            plan_hash: "p".to_owned(),
        },
        Tier::T3,
        vec![FixStep::new(0, "s0", "doc:a", "op", "detail")],
        Risk::Low,
        true,
        Vec::new(),
    )
    .unwrap()
}

fn invariants() -> InvariantReport {
    InvariantReport { sets: Vec::new() }
}

fn rehearsal() -> RehearsalEvidence {
    RehearsalEvidence {
        fingerprint: fingerprint(i128::MAX, i128::MIN),
        pre_digest: "a".to_owned(),
        post_digest: "b".to_owned(),
        invariants: invariants(),
        passed: true,
        detail: "ok".to_owned(),
    }
}

fn report() -> VerifyReport {
    VerifyReport {
        invariants: invariants(),
        remedy_verified: true,
        symptom_cleared: true,
        violations: Vec::new(),
        fingerprint: fingerprint(i128::MAX, i128::MIN),
    }
}

fn rollback() -> RollbackEvidence {
    let pre = fingerprint(i128::MAX, i128::MIN);
    let after = fingerprint(i128::MIN, i128::MAX);
    RollbackEvidence {
        cause: RollbackCause::StepFailed { index: 1 },
        mechanism: Mechanism::Snapshot,
        invariants: invariants(),
        residue: Some(ResidueEvidence::measure(&pre, &after)),
    }
}

fn every_variant() -> Vec<JournalRecord> {
    let change = ChangeRecord::new(0, "doc:a", "x", "y");
    let records = vec![
        JournalRecord::Proposed {
            site: "site".to_owned(),
            agent: AgentRef::new("agent", Tier::T3, false),
            epoch: DeploymentEpoch::new(7),
        },
        JournalRecord::Planned {
            plan: Box::new(plan()),
            path: PlanPath::Rehearse,
        },
        JournalRecord::ApprovalRequested {
            reason: "r".to_owned(),
        },
        JournalRecord::Approved {
            approval: Approval {
                approver: "alice".to_owned(),
                reference: "CR-1".to_owned(),
                at: 5,
            },
        },
        JournalRecord::SelfApproved {
            by: "agent".to_owned(),
        },
        JournalRecord::ApprovalRefused {
            reason: "r".to_owned(),
        },
        JournalRecord::RehearsalStarted {
            plan_hash: "h".to_owned(),
        },
        JournalRecord::Rehearsed {
            evidence: Box::new(rehearsal()),
        },
        JournalRecord::RehearsalFailed {
            reason: "r".to_owned(),
            evidence: Some(Box::new(rehearsal())),
        },
        JournalRecord::ApplyStarted {
            mechanism: Mechanism::PreImage,
            plan_hash: "h".to_owned(),
        },
        JournalRecord::SnapshotTaken {
            snapshot: SnapshotRef::new("snap", "d"),
        },
        JournalRecord::PreImagesCaptured {
            images: vec![PreImage::new(
                "doc:a",
                "1",
                serde_json::Value::String(i128::MAX.to_string()),
            )],
        },
        JournalRecord::PreFingerprintCaptured {
            fingerprint: Box::new(fingerprint(i128::MAX, i128::MIN)),
        },
        JournalRecord::StepStarted {
            index: 0,
            id: "s0".to_owned(),
            idempotency_key: "k".to_owned(),
        },
        JournalRecord::StepApplied {
            change: change.clone(),
        },
        JournalRecord::StepFailed {
            index: 0,
            reason: "r".to_owned(),
        },
        JournalRecord::Applied {
            changes: vec![change],
        },
        JournalRecord::VerifyStarted,
        JournalRecord::Verified {
            report: Box::new(report()),
        },
        JournalRecord::VerifyFailed {
            report: Some(Box::new(report())),
            reasons: vec!["r".to_owned()],
        },
        JournalRecord::RollbackStarted {
            cause: RollbackCause::Recovery,
            mechanism: Mechanism::PreImage,
        },
        JournalRecord::RolledBack {
            evidence: Box::new(rollback()),
        },
        JournalRecord::RollbackFailed {
            reason: "r".to_owned(),
            evidence: Some(Box::new(rollback())),
        },
        JournalRecord::ResidueChecked {
            residue: ResidueEvidence::measure(
                &fingerprint(i128::MAX, i128::MIN),
                &fingerprint(i128::MIN, i128::MAX),
            ),
        },
        JournalRecord::Failed {
            stage: FailureStage::Apply,
            reason: "r".to_owned(),
        },
        JournalRecord::UndoWindowOpened {
            window: UndoWindow::open(10, 60),
        },
        JournalRecord::UndoWindowClosed {
            at: 70,
            reason: "expired".to_owned(),
        },
        JournalRecord::UndoStarted {
            mechanism: Mechanism::Snapshot,
            reason: "r".to_owned(),
        },
        JournalRecord::Undone {
            evidence: Box::new(rollback()),
        },
        JournalRecord::UndoRefused {
            reason: "r".to_owned(),
            code: "c".to_owned(),
        },
    ];
    // A new variant fails to compile here until it is listed above and given a round trip.
    for record in &records {
        match record {
            JournalRecord::Proposed { .. }
            | JournalRecord::Planned { .. }
            | JournalRecord::ApprovalRequested { .. }
            | JournalRecord::Approved { .. }
            | JournalRecord::SelfApproved { .. }
            | JournalRecord::ApprovalRefused { .. }
            | JournalRecord::RehearsalStarted { .. }
            | JournalRecord::Rehearsed { .. }
            | JournalRecord::RehearsalFailed { .. }
            | JournalRecord::ApplyStarted { .. }
            | JournalRecord::SnapshotTaken { .. }
            | JournalRecord::PreImagesCaptured { .. }
            | JournalRecord::PreFingerprintCaptured { .. }
            | JournalRecord::StepStarted { .. }
            | JournalRecord::StepApplied { .. }
            | JournalRecord::StepFailed { .. }
            | JournalRecord::Applied { .. }
            | JournalRecord::VerifyStarted
            | JournalRecord::Verified { .. }
            | JournalRecord::VerifyFailed { .. }
            | JournalRecord::RollbackStarted { .. }
            | JournalRecord::RolledBack { .. }
            | JournalRecord::RollbackFailed { .. }
            | JournalRecord::ResidueChecked { .. }
            | JournalRecord::Failed { .. }
            | JournalRecord::UndoWindowOpened { .. }
            | JournalRecord::UndoWindowClosed { .. }
            | JournalRecord::UndoStarted { .. }
            | JournalRecord::Undone { .. }
            | JournalRecord::UndoRefused { .. } => {}
        }
    }
    records
}

#[test]
fn every_journal_record_variant_round_trips_through_serde() {
    let records = every_variant();
    assert_eq!(records.len(), 30);
    for record in records {
        let text = serde_json::to_string(&record).unwrap();
        let back: JournalRecord = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{} did not read back: {e}", record.audit_detail()));
        assert_eq!(back, record, "{}", record.audit_detail());
    }
}

#[test]
fn fake_pre_images_capture_and_restore_values_beyond_64_bits() {
    let big = i128::from(u64::MAX) + 1;
    let small = i128::from(i64::MIN) - 1;
    let world = FakeWorld::with(&[
        ("doc:big", big),
        ("doc:small", small),
        ("doc:max", i128::MAX),
    ]);
    let mut store = FakePreImages::new(&world);
    let targets: Vec<String> = ["doc:big", "doc:small", "doc:max"]
        .iter()
        .map(|t| (*t).to_owned())
        .collect();
    let images = store.capture(&targets).unwrap();
    // The image survives a JSON round trip, as it does in the journal.
    let images: Vec<PreImage> =
        serde_json::from_str(&serde_json::to_string(&images).unwrap()).unwrap();
    for target in &targets {
        world.set(target, 0).unwrap();
    }
    for image in &images {
        store.restore(image).unwrap();
    }
    assert_eq!(world.get("doc:big").unwrap(), big);
    assert_eq!(world.get("doc:small").unwrap(), small);
    assert_eq!(world.get("doc:max").unwrap(), i128::MAX);
    assert_eq!(store.digest("doc:big").unwrap(), big.to_string());
}

#[test]
fn fake_pre_images_still_read_a_json_integer_and_refuse_a_malformed_image() {
    let world = FakeWorld::with(&[("doc:a", 1)]);
    let mut store = FakePreImages::new(&world);
    store
        .restore(&PreImage::new("doc:a", "9", serde_json::json!(9)))
        .unwrap();
    assert_eq!(world.get("doc:a").unwrap(), 9);
    for bad in [
        serde_json::json!("not a number"),
        serde_json::json!(1.5),
        serde_json::json!(null),
        serde_json::json!([1]),
    ] {
        let err = store
            .restore(&PreImage::new("doc:a", "x", bad))
            .unwrap_err();
        assert!(matches!(err, FixError::PreImageFailed { .. }), "{err}");
        assert!(err.to_string().contains("doc:a"));
    }
}
