//! A store whose reads fail: the failure is part of the signature, so the engine stops, applies
//! nothing further, leaves the checkpoint as it was, and a later run with a healthy store
//! completes with no duplicate effect (WP-SEC-05.T3).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::cell::Cell;

use spot_data_policy::store::{ApplyItem, Checkpoint, ItemOutcome, RetentionStore};
use spot_data_policy::{
    DataPolicyError, DeletionProof, ExecutionOptions, FieldClassMap, Gregorian, Holds, MemoryStore,
    PolicySet, Record, Resume, StatutoryFloor, execute, plan_run,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Read {
    Records,
    Record,
    Outcome,
    RunOutcomes,
    Checkpoint,
    Proof,
    Proofs,
}

const ALL: [Read; 7] = [
    Read::Records,
    Read::Record,
    Read::Outcome,
    Read::RunOutcomes,
    Read::Checkpoint,
    Read::Proof,
    Read::Proofs,
];

/// A [`MemoryStore`] whose reads can be made to fail: every call of one read from its `n`th
/// (counting from 0) onwards, until healed.
struct Flaky {
    inner: MemoryStore,
    calls: [Cell<usize>; 7],
    armed: Cell<Option<(Read, usize)>>,
}

impl Flaky {
    fn new(inner: MemoryStore) -> Self {
        Self {
            inner,
            calls: Default::default(),
            armed: Cell::new(None),
        }
    }

    fn fail_from(&self, read: Read, nth: usize) {
        let slot = ALL.iter().position(|r| *r == read).unwrap();
        self.calls[slot].set(0);
        self.armed.set(Some((read, nth)));
    }

    fn heal(&self) {
        self.armed.set(None);
    }

    fn gate(&self, read: Read) -> Result<(), DataPolicyError> {
        let slot = ALL.iter().position(|r| *r == read).unwrap();
        let seen = self.calls[slot].get();
        self.calls[slot].set(seen + 1);
        match self.armed.get() {
            Some((armed, nth)) if armed == read && seen >= nth => {
                Err(DataPolicyError::StoreFailed {
                    reason: "the injected read failure",
                })
            }
            _ => Ok(()),
        }
    }
}

impl RetentionStore for Flaky {
    fn records(&self) -> Result<Vec<Record>, DataPolicyError> {
        self.gate(Read::Records)?;
        self.inner.records()
    }
    fn record(&self, id: &str) -> Result<Option<Record>, DataPolicyError> {
        self.gate(Read::Record)?;
        self.inner.record(id)
    }
    fn apply_item(&mut self, item: ApplyItem) -> Result<(), DataPolicyError> {
        self.inner.apply_item(item)
    }
    fn outcome(&self, run: &str, record: &str) -> Result<Option<ItemOutcome>, DataPolicyError> {
        self.gate(Read::Outcome)?;
        self.inner.outcome(run, record)
    }
    fn run_outcomes(&self, run: &str) -> Result<Vec<ItemOutcome>, DataPolicyError> {
        self.gate(Read::RunOutcomes)?;
        self.inner.run_outcomes(run)
    }
    fn save_checkpoint(&mut self, checkpoint: Checkpoint) -> Result<(), DataPolicyError> {
        self.inner.save_checkpoint(checkpoint)
    }
    fn checkpoint(&self, run: &str) -> Result<Option<Checkpoint>, DataPolicyError> {
        self.gate(Read::Checkpoint)?;
        self.inner.checkpoint(run)
    }
    fn proof(&self, record: &str) -> Result<Option<DeletionProof>, DataPolicyError> {
        self.gate(Read::Proof)?;
        self.inner.proof(record)
    }
    fn proofs(&self) -> Result<Vec<DeletionProof>, DataPolicyError> {
        self.gate(Read::Proofs)?;
        self.inner.proofs()
    }
}

fn notes(count: usize) -> Flaky {
    let records = (0..count)
        .map(|i| common::record(&format!("note-{i}"), "note", common::at(2020, 1, 1)))
        .collect();
    Flaky::new(MemoryStore::from_records(records).unwrap())
}

fn is_store_failed(error: &DataPolicyError) -> bool {
    matches!(error, DataPolicyError::StoreFailed { .. })
}

fn live() -> ExecutionOptions {
    ExecutionOptions {
        dry_run: false,
        batch_size: 1,
        ..ExecutionOptions::default()
    }
}

macro_rules! policy {
    ($policy:ident) => {
        let rules = common::rules(&[("notes", "note", "created", "1y", "delete")]);
        let holds = Holds::empty();
        let floors = StatutoryFloor::empty();
        let fields = FieldClassMap::default();
        let $policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
    };
}

const NOW: fn() -> u64 = || common::at(2026, 1, 1);

#[test]
fn a_failed_records_read_is_an_error_not_an_empty_plan() {
    policy!(policy);
    let store = notes(3);
    store.fail_from(Read::Records, 0);
    let error = plan_run(&policy, &store, NOW()).unwrap_err();
    assert!(is_store_failed(&error), "{error:?}");
    store.heal();
    assert_eq!(plan_run(&policy, &store, NOW()).unwrap().items().len(), 3);
}

/// For each read the engine makes during a run: a run that meets the failure stops with
/// `StoreFailed`, applies nothing after it, and a second run with a healthy store finishes
/// every item exactly once.
#[test]
fn every_engine_read_that_fails_stops_the_run_and_a_healthy_rerun_repeats_no_effect() {
    // (the read, the call it starts failing from, items applied before the failure at most)
    let cases = [
        (Read::Checkpoint, 0, 0),
        (Read::Outcome, 0, 0),
        (Read::Outcome, 1, 1),
        (Read::Record, 0, 0),
        (Read::Record, 1, 1),
        (Read::Records, 1, 1),
        (Read::RunOutcomes, 0, 1),
        (Read::RunOutcomes, 1, 2),
    ];
    for (read, nth, applied_at_most) in cases {
        policy!(policy);
        let mut store = notes(3);
        let plan = plan_run(&policy, &store, NOW()).unwrap();
        store.fail_from(read, nth);
        let error = execute(&plan, &policy, &mut store, &live(), Resume::Fresh, NOW())
            .expect_err(&format!("{read:?} from {nth}"));
        assert!(is_store_failed(&error), "{read:?}: {error:?}");
        let applied = store.inner.effects().len();
        assert!(
            applied <= applied_at_most,
            "{read:?} from {nth}: {applied} effects after the failure"
        );
        // The checkpoint never claims more than was applied: the failed item is not counted.
        store.heal();
        let run_id = format!("{}:{}", live().kind, plan.identity_hex());
        if let Some(saved) = store.checkpoint(&run_id).unwrap() {
            assert!(
                saved.next_index <= applied,
                "{read:?} from {nth}: checkpoint at {} with {applied} applied",
                saved.next_index
            );
        }
        // A second run with a healthy store completes, each record deleted once.
        let report = execute(&plan, &policy, &mut store, &live(), Resume::Fresh, NOW())
            .or_else(|_| execute(&plan, &policy, &mut store, &live(), Resume::Continue, NOW()))
            .unwrap();
        assert!(report.completed, "{read:?} from {nth}");
        let mut ids: Vec<&str> = store
            .inner
            .effects()
            .iter()
            .map(|e| e.record_id.as_str())
            .collect();
        assert_eq!(ids.len(), 3, "{read:?} from {nth}: no duplicate effect");
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 3);
        assert_eq!(store.inner.record_count(), 0);
    }
}

#[test]
fn a_failure_after_a_saved_checkpoint_leaves_that_checkpoint_as_it_was() {
    policy!(policy);
    let mut store = notes(4);
    let plan = plan_run(&policy, &store, NOW()).unwrap();
    let options = ExecutionOptions {
        max_batches: Some(1),
        ..live()
    };
    let first = execute(&plan, &policy, &mut store, &options, Resume::Fresh, NOW()).unwrap();
    assert!(!first.completed);
    let saved = store.checkpoint(&first.run_id).unwrap().unwrap();

    store.fail_from(Read::RunOutcomes, 0);
    let error = execute(&plan, &policy, &mut store, &live(), Resume::Continue, NOW())
        .expect_err("the counters cannot be read");
    assert!(is_store_failed(&error));
    store.heal();
    assert_eq!(
        store.checkpoint(&first.run_id).unwrap().unwrap(),
        saved,
        "the checkpoint is exactly the one saved before the failure"
    );
    assert_eq!(
        store.inner.effects().len(),
        2,
        "one item ran, then the run stopped"
    );

    let done = execute(&plan, &policy, &mut store, &live(), Resume::Continue, NOW()).unwrap();
    assert!(done.completed);
    assert_eq!(store.inner.effects().len(), 4);
    assert_eq!(store.inner.record_count(), 0);
}

#[test]
fn a_proof_is_not_verified_against_a_record_read_that_failed() {
    policy!(policy);
    let mut store = notes(1);
    let plan = plan_run(&policy, &store, NOW()).unwrap();
    execute(&plan, &policy, &mut store, &live(), Resume::Fresh, NOW()).unwrap();
    let proof = store.proof("note-0").unwrap().unwrap();
    proof.verify(&store).expect("a healthy store verifies");

    store.fail_from(Read::Record, 0);
    let error = proof.verify(&store).unwrap_err();
    assert!(is_store_failed(&error), "an error, not a pass: {error:?}");
}

#[test]
fn the_proof_reads_report_their_failure() {
    let store = notes(1);
    store.fail_from(Read::Proof, 0);
    assert!(is_store_failed(&store.proof("note-0").unwrap_err()));
    store.fail_from(Read::Proofs, 0);
    assert!(is_store_failed(&store.proofs().unwrap_err()));
    store.heal();
    assert!(store.proofs().unwrap().is_empty());
}

#[test]
fn the_memory_store_never_fails_a_read() {
    let store = MemoryStore::new();
    assert!(store.records().unwrap().is_empty());
    assert!(store.record("x").unwrap().is_none());
    assert!(store.outcome("r", "x").unwrap().is_none());
    assert!(store.run_outcomes("r").unwrap().is_empty());
    assert!(store.checkpoint("r").unwrap().is_none());
    assert!(store.proof("x").unwrap().is_none());
    assert!(store.proofs().unwrap().is_empty());
}
