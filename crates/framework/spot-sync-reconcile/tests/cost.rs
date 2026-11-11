//! The digest comparison cost grows with the number of differences, not with the number of
//! documents.
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_storage_contract::{FeedPosition, Name, Version};
use spot_sync_reconcile::{RecordFacts, RecordKey, Scope, ScopeSnapshot, reconcile};

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn records(count: usize) -> Vec<RecordFacts> {
    (0..count)
        .map(|i| {
            RecordFacts::of(RecordKey::new(
                name("item"),
                format!("rec_{i:06}"),
                Version(1),
            ))
        })
        .collect()
}

fn snapshot(scope: &Scope, records: Vec<RecordFacts>) -> ScopeSnapshot {
    ScopeSnapshot::new(scope.clone(), FeedPosition::new(1, 1), records)
}

/// Reconciles an owner of `size` records against a mirror missing `differences` of them.
fn cost(size: usize, differences: usize) -> (usize, usize, usize) {
    let scope = Scope::new("company", "C1");
    let owner = records(size);
    let mut mirror = owner.clone();
    for _ in 0..differences {
        mirror.pop();
    }
    let report = reconcile(&snapshot(&scope, owner), &snapshot(&scope, mirror), &scope);
    (report.findings.len(), report.rounds, report.digest_bytes)
}

#[test]
fn a_single_difference_costs_a_few_rounds_not_a_full_scan() {
    let (findings, rounds, bytes) = cost(2_000, 1);
    // One missing record and the count digest that changes with it.
    assert_eq!(findings, 2);
    assert!(
        rounds < 100,
        "one difference cost {rounds} rounds over 2000 records"
    );
    assert_eq!(
        bytes,
        rounds * 64,
        "each round compares two 32-byte digests"
    );
}

#[test]
fn the_round_count_is_independent_of_the_document_count() {
    let (_, small_rounds, _) = cost(200, 1);
    let (_, large_rounds, _) = cost(20_000, 1);
    // Both are logarithmic in the record count; the hundred-fold growth of the scope must not
    // show up as a hundred-fold growth in rounds.
    assert!(
        large_rounds < 3 * small_rounds.max(1),
        "{small_rounds} vs {large_rounds}"
    );
    assert!(large_rounds < 100, "{large_rounds}");
}

#[test]
fn the_round_count_grows_with_the_number_of_differences() {
    let (_, one, _) = cost(2_000, 1);
    let (_, sixteen, _) = cost(2_000, 16);
    assert!(
        sixteen > one,
        "sixteen differences ({sixteen}) must cost more rounds than one ({one})"
    );
    // Still far below the record count.
    assert!(sixteen < 1_000, "{sixteen}");
}

#[test]
fn a_wholly_missing_scope_is_one_round_of_findings() {
    let scope = Scope::new("company", "C1");
    let owner = records(500);
    let report = reconcile(
        &snapshot(&scope, owner),
        &snapshot(&scope, Vec::new()),
        &scope,
    );
    // Every record is missing, plus the count digest: one round, no per-record descent.
    assert_eq!(report.of_kind("missing").len(), 500);
    assert_eq!(report.rounds, 1);
}
