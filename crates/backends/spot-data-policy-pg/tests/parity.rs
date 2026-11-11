//! One table of cases, run against the in-memory reference store and the PostgreSQL store. The
//! in-memory store is the contract: both must give the same answers.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use common::{
    Harness, MemoryHarness, PgHarness, SharedSource, checkpoint, create_schema, delete_item,
    fixture, outcome, pg_store, tenant,
};
use spot_data_policy::DataPolicyError;
use spot_data_policy::store::{ApplyItem, Effect, MemoryStore, OutcomeStatus, SkipReason};

type Case = (&'static str, fn(&mut dyn Harness));

const RECORDS: [&str; 4] = ["a", "b", "c", "d"];

fn applying_the_same_item_twice_applies_it_once(h: &mut dyn Harness) {
    let item = delete_item("run", "a");
    h.store().apply_item(item.clone()).unwrap();
    h.store().apply_item(item).unwrap();
    assert_eq!(h.effects(), 1);
    assert_eq!(h.live(), 3);
    assert_eq!(
        h.store().outcome("run", "a").unwrap().unwrap().status,
        OutcomeStatus::Applied
    );
}

fn a_skipped_item_records_an_outcome_and_applies_nothing(h: &mut dyn Harness) {
    let mut skipped = outcome("run", "a", OutcomeStatus::Skipped);
    skipped.reason = Some(SkipReason::LegalHold {
        hold: "h1".to_owned(),
        reason: "case".to_owned(),
    });
    h.store()
        .apply_item(ApplyItem {
            effect: Effect::None,
            outcome: skipped.clone(),
            audit: None,
            proof: None,
        })
        .unwrap();
    assert_eq!(h.effects(), 0);
    assert_eq!(h.live(), 4);
    assert_eq!(h.store().outcome("run", "a").unwrap(), Some(skipped));
    assert!(h.store().proof("a").unwrap().is_none());
}

fn a_failing_effect_leaves_no_outcome_and_no_proof(h: &mut dyn Harness) {
    // "zz" is not a live record: the delete cannot be applied.
    let err = h.store().apply_item(delete_item("run", "zz")).unwrap_err();
    assert!(matches!(err, DataPolicyError::StoreFailed { .. }));
    assert!(h.store().outcome("run", "zz").unwrap().is_none());
    assert!(h.store().proof("zz").unwrap().is_none());
    assert!(h.store().run_outcomes("run").unwrap().is_empty());
    assert_eq!(h.live(), 4);
}

fn a_different_outcome_for_the_same_item_is_refused(h: &mut dyn Harness) {
    h.store().apply_item(delete_item("run", "a")).unwrap();
    let mut other = delete_item("run", "a");
    other.outcome.attempt = 2;
    let err = h.store().apply_item(other).unwrap_err();
    assert!(matches!(err, DataPolicyError::StoreFailed { .. }));
    assert_eq!(h.store().outcome("run", "a").unwrap().unwrap().attempt, 1);
    assert_eq!(h.effects(), 1);
}

fn a_deleted_records_proof_survives_and_verifies(h: &mut dyn Harness) {
    h.store().apply_item(delete_item("run", "a")).unwrap();
    assert!(h.store().record("a").unwrap().is_none());
    let proof = h.store().proof("a").unwrap().unwrap();
    assert_eq!(proof.record_id, "a");
    proof.verify_digest().unwrap();
    proof.verify(&*h.store()).unwrap();
}

fn proofs_and_outcomes_are_ordered_by_record_id(h: &mut dyn Harness) {
    for id in ["d", "b", "c", "a"] {
        h.store().apply_item(delete_item("run", id)).unwrap();
    }
    h.store()
        .apply_item(delete_item("other", "zz"))
        .unwrap_err();
    let ids: Vec<String> = h
        .store()
        .proofs()
        .unwrap()
        .into_iter()
        .map(|p| p.record_id)
        .collect();
    assert_eq!(ids, ["a", "b", "c", "d"]);
    let ids: Vec<String> = h
        .store()
        .run_outcomes("run")
        .unwrap()
        .into_iter()
        .map(|o| o.record_id)
        .collect();
    assert_eq!(ids, ["a", "b", "c", "d"]);
    assert!(h.store().run_outcomes("another run").unwrap().is_empty());
}

fn a_checkpoint_is_replaced_and_read_back(h: &mut dyn Harness) {
    assert!(h.store().checkpoint("run").unwrap().is_none());
    h.store().save_checkpoint(checkpoint("run", 1)).unwrap();
    h.store().save_checkpoint(checkpoint("run", 3)).unwrap();
    assert_eq!(
        h.store().checkpoint("run").unwrap(),
        Some(checkpoint("run", 3))
    );
    assert!(h.store().checkpoint("other").unwrap().is_none());
}

fn resume_after_a_crash_processes_each_item_once(h: &mut dyn Harness) {
    // First attempt: a and b done, checkpoint says 2, then "crash" before c.
    for id in ["a", "b"] {
        h.store().apply_item(delete_item("run", id)).unwrap();
    }
    h.store().save_checkpoint(checkpoint("run", 2)).unwrap();
    // Resume re-walks the whole plan: items with an outcome are skipped by the engine, and a
    // repeated apply of one is a no-op in the store.
    for id in ["a", "b", "c", "d"] {
        if h.store().outcome("run", id).unwrap().is_none() {
            h.store().apply_item(delete_item("run", id)).unwrap();
        }
    }
    h.store().apply_item(delete_item("run", "a")).unwrap();
    assert_eq!(h.effects(), 4);
    assert_eq!(h.live(), 0);
    assert_eq!(h.store().run_outcomes("run").unwrap().len(), 4);
}

const CASES: [Case; 8] = [
    (
        "applying twice applies once",
        applying_the_same_item_twice_applies_it_once,
    ),
    (
        "skipped item",
        a_skipped_item_records_an_outcome_and_applies_nothing,
    ),
    (
        "failing effect",
        a_failing_effect_leaves_no_outcome_and_no_proof,
    ),
    (
        "different outcome refused",
        a_different_outcome_for_the_same_item_is_refused,
    ),
    (
        "proof survives",
        a_deleted_records_proof_survives_and_verifies,
    ),
    ("ordering", proofs_and_outcomes_are_ordered_by_record_id),
    ("checkpoint", a_checkpoint_is_replaced_and_read_back),
    ("resume", resume_after_a_crash_processes_each_item_once),
];

#[test]
fn the_in_memory_store_passes_every_case() {
    for (name, case) in CASES {
        let mut h = MemoryHarness(
            MemoryStore::from_records(RECORDS.iter().map(|i| common::record(i)).collect()).unwrap(),
        );
        println!("memory: {name}");
        case(&mut h);
    }
}

#[test]
fn the_postgres_store_passes_the_same_cases() {
    let Some(fx) = fixture() else { return };
    let bridge = common::bridge(&fx, 2);
    for (n, (name, case)) in CASES.iter().enumerate() {
        // One tenant schema per case keeps the cases independent in one database.
        let tenant = tenant(u8::try_from(n + 1).unwrap());
        create_schema(&fx, &tenant);
        let source = SharedSource::with(&RECORDS);
        let mut h = PgHarness {
            store: pg_store(&fx, &bridge, &tenant, source.clone()),
            source,
        };
        println!("postgres: {name}");
        case(&mut h);
    }
}
