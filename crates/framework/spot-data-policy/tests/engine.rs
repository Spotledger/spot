//! Execution: deletion proofs, dry run, identity, checkpoints and resume after a kill.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_data_policy::{
    Checkpoint, DataPolicyError, DeletionProof, ExecutionOptions, FieldClassMap, Gregorian, Holds,
    MemoryStore, PolicySet, Resume, RetentionStore, StatutoryFloor, execute, plan_run,
};

fn deleting_notes() -> spot_data_policy::RetentionRules {
    common::rules(&[("notes", "note", "created", "1y", "delete")])
}

fn notes(count: usize) -> Vec<spot_data_policy::Record> {
    (0..count)
        .map(|index| common::record(&format!("note-{index}"), "note", common::at(2020, 1, 1)))
        .collect()
}

#[test]
fn a_planned_deletion_leaves_a_verifiable_proof_after_the_record_is_gone() {
    let mut store = MemoryStore::from_records(vec![common::record(
        "note-1",
        "note",
        common::at(2020, 1, 1),
    )])
    .unwrap();
    let rules = deleting_notes();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let now = common::at(2026, 1, 1);
    let plan = plan_run(&policy, &store, now).unwrap();
    let options = ExecutionOptions {
        dry_run: false,
        ..ExecutionOptions::default()
    };
    let report = execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap();

    assert!(report.completed);
    assert_eq!(report.applied(), 1);
    assert!(
        store.record("note-1").unwrap().is_none(),
        "the record is gone"
    );
    let proof = store.proof("note-1").unwrap().expect("a proof is kept");
    assert_eq!(proof.rule_id, "notes");
    assert_eq!(proof.instant, now);
    assert!(proof.verify_digest().is_ok());
    proof.verify(&store).expect("the proof verifies");
    assert_eq!(
        report.outcomes[0].proof_digest.as_deref(),
        Some(proof.digest_hex().as_str())
    );

    let audit = &store.audits()[0];
    assert_eq!(audit.kind, "data_policy.delete");
    assert_eq!(audit.subject.as_ref().unwrap().id, "note-1");
    assert_eq!(store.effects().len(), 1);
    assert_eq!(store.effects()[0].effect, "delete");
}

#[test]
fn a_proof_does_not_verify_if_the_record_is_still_present_or_its_digest_changed() {
    let store = MemoryStore::from_records(vec![common::record(
        "note-1",
        "note",
        common::at(2020, 1, 1),
    )])
    .unwrap();
    let record = store.record("note-1").unwrap().unwrap();
    let proof = DeletionProof::compute(&record, "notes", 1);
    assert!(proof.verify(&store).is_err(), "the record is still there");

    let mut altered = proof;
    altered.rule_id = "another-rule".to_owned();
    assert!(altered.verify_digest().is_err());
}

#[test]
fn resume_after_a_kill_finishes_every_item_once() {
    let mut store = MemoryStore::from_records(notes(5)).unwrap();
    let rules = deleting_notes();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let now = common::at(2026, 1, 1);
    let plan = plan_run(&policy, &store, now).unwrap();
    let options = ExecutionOptions {
        dry_run: false,
        batch_size: 2,
        max_batches: Some(1),
        ..ExecutionOptions::default()
    };

    let first = execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap();
    assert!(!first.completed, "the kill stopped after one batch");
    assert_eq!(first.applied(), 2);
    assert_eq!(first.outcomes.len(), 2);
    assert_eq!(store.effects().len(), 2);

    let resumed_options = ExecutionOptions {
        max_batches: None,
        ..options
    };
    let second = execute(
        &plan,
        &policy,
        &mut store,
        &resumed_options,
        Resume::Continue,
        now,
    )
    .unwrap();
    assert!(second.completed);
    assert_eq!(second.attempt, 2);
    assert_eq!(
        second.applied(),
        5,
        "the counters come from the outcome rows"
    );
    assert_eq!(store.effects().len(), 5);
    let mut ids: Vec<&str> = store
        .effects()
        .iter()
        .map(|effect| effect.record_id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 5, "no record was applied twice");
    assert_eq!(store.record_count(), 0);
    assert_eq!(store.proofs().unwrap().len(), 5);
}

#[test]
fn a_stale_checkpoint_repeats_no_effect() {
    let mut store = MemoryStore::from_records(notes(4)).unwrap();
    let rules = deleting_notes();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let now = common::at(2026, 1, 1);
    let plan = plan_run(&policy, &store, now).unwrap();
    let options = ExecutionOptions {
        dry_run: false,
        batch_size: 2,
        max_batches: Some(1),
        ..ExecutionOptions::default()
    };
    let first = execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap();
    assert!(!first.completed);

    // Rewind the checkpoint to the start: the outcome rows must still prevent a second effect.
    let mut rewound = store.checkpoint(&first.run_id).unwrap().unwrap();
    rewound.next_index = 0;
    store.save_checkpoint(rewound).unwrap();

    let resumed = execute(
        &plan,
        &policy,
        &mut store,
        &ExecutionOptions {
            max_batches: None,
            ..options
        },
        Resume::Continue,
        now,
    )
    .unwrap();
    assert!(resumed.completed);
    assert_eq!(
        store.effects().len(),
        4,
        "each record was deleted exactly once"
    );
    assert_eq!(store.record_count(), 0);
}

#[test]
fn a_run_identity_cannot_be_started_twice_and_a_finished_run_cannot_resume() {
    let mut store = MemoryStore::from_records(notes(2)).unwrap();
    let rules = deleting_notes();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let now = common::at(2026, 1, 1);
    let plan = plan_run(&policy, &store, now).unwrap();
    let options = ExecutionOptions {
        dry_run: false,
        ..ExecutionOptions::default()
    };
    execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap();

    let error = execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap_err();
    assert_eq!(error.code(), "data_policy.run_mismatch");
    let error = execute(&plan, &policy, &mut store, &options, Resume::Continue, now).unwrap_err();
    assert_eq!(error.code(), "data_policy.run_mismatch");
}

#[test]
fn a_checkpoint_from_another_plan_is_refused() {
    let mut store = MemoryStore::from_records(notes(1)).unwrap();
    let rules = deleting_notes();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let first = plan_run(&policy, &store, common::at(2026, 1, 1)).unwrap();
    let second = plan_run(&policy, &store, common::at(2026, 2, 1)).unwrap();
    assert_ne!(first.identity(), second.identity());

    // A checkpoint filed under the second run's id but carrying the first plan's digest.
    let second_run = format!(
        "{}:{}",
        spot_data_policy::DEFAULT_KIND,
        second.identity_hex()
    );
    store
        .save_checkpoint(Checkpoint::new(second_run, first.identity()))
        .unwrap();
    let options = ExecutionOptions {
        dry_run: false,
        ..ExecutionOptions::default()
    };
    let error = execute(
        &second,
        &policy,
        &mut store,
        &options,
        Resume::Continue,
        common::at(2026, 2, 1),
    )
    .unwrap_err();
    assert_eq!(error.code(), "data_policy.plan_changed");
}

#[test]
fn a_dry_run_with_max_batches_writes_nothing_and_is_not_resumable() {
    let mut store = MemoryStore::from_records(notes(3)).unwrap();
    let rules = deleting_notes();
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let now = common::at(2026, 1, 1);
    let plan = plan_run(&policy, &store, now).unwrap();
    let options = ExecutionOptions {
        max_batches: Some(1),
        ..ExecutionOptions::default()
    };
    assert!(options.dry_run, "a dry run is the default");
    let report = execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap();
    assert!(report.completed);
    assert_eq!(report.outcomes.len(), 3);
    assert_eq!(store.effects().len(), 0);
    assert_eq!(store.record_count(), 3);
    assert!(store.checkpoint(&report.run_id).unwrap().is_none());
    assert!(matches!(
        execute(&plan, &policy, &mut store, &options, Resume::Continue, now),
        Err(DataPolicyError::InvalidOption { .. })
    ));
}

#[test]
fn archive_keeps_the_record_and_writes_no_proof() {
    let mut store = MemoryStore::from_records(notes(1)).unwrap();
    let rules = common::rules(&[("notes", "note", "created", "1y", "archive")]);
    let holds = Holds::empty();
    let floors = StatutoryFloor::empty();
    let fields = FieldClassMap::default();
    let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    let now = common::at(2026, 1, 1);
    let plan = plan_run(&policy, &store, now).unwrap();
    let options = ExecutionOptions {
        dry_run: false,
        ..ExecutionOptions::default()
    };
    execute(&plan, &policy, &mut store, &options, Resume::Fresh, now).unwrap();
    assert!(
        store.record("note-0").unwrap().is_some(),
        "archive keeps the record"
    );
    assert!(store.is_archived("note-0"));
    assert!(store.proofs().unwrap().is_empty());
}
