//! Choosing the snapshot a rebuild starts from.

use super::snapshot;
use crate::{JournalCatalog, SnapshotInfo};

fn at_least(position: u64, generation: u64, verified: bool, taken_at_millis: i64) -> SnapshotInfo {
    SnapshotInfo {
        position,
        generation,
        verified,
        taken_at_millis,
    }
}

fn catalog(snapshots: Vec<SnapshotInfo>) -> JournalCatalog {
    JournalCatalog {
        snapshots,
        segments: Vec::new(),
        forks: Vec::new(),
    }
}

#[test]
fn an_unverified_snapshot_is_never_best() {
    let c = catalog(vec![
        at_least(10, 1, true, 1_728_000_100_000),
        at_least(20, 1, false, 1_728_000_200_000),
    ]);
    assert_eq!(c.best_snapshot(30).map(|s| s.position), Some(10));
    let only_unverified = catalog(vec![at_least(20, 1, false, 1_728_000_200_000)]);
    assert_eq!(only_unverified.best_snapshot(30), None);
}

#[test]
fn a_snapshot_after_the_target_is_not_best() {
    let c = catalog(vec![at_least(20, 1, true, 1_728_000_200_000)]);
    assert_eq!(c.best_snapshot(20).map(|s| s.position), Some(20));
    assert_eq!(c.best_snapshot(19), None);
}

#[test]
fn the_latest_position_wins() {
    let c = catalog(vec![
        at_least(10, 1, true, 1_728_000_100_000),
        at_least(20, 1, true, 1_728_000_200_000),
        at_least(15, 1, true, 1_728_000_150_000),
    ]);
    assert_eq!(c.best_snapshot(30).map(|s| s.position), Some(20));
    assert_eq!(c.best_snapshot(19).map(|s| s.position), Some(15));
}

#[test]
fn at_one_position_the_higher_generation_wins() {
    let c = catalog(vec![
        at_least(20, 1, true, 1_728_000_250_000),
        at_least(20, 2, true, 1_728_000_200_000),
    ]);
    assert_eq!(
        c.best_snapshot(30).map(|s| s.generation),
        Some(2),
        "the generation outranks the time the snapshot was taken"
    );
}

#[test]
fn at_one_position_and_generation_the_later_taken_at_wins() {
    let c = catalog(vec![
        at_least(20, 1, true, 1_728_000_300_000),
        at_least(20, 1, true, 1_728_000_200_000),
    ]);
    assert_eq!(
        c.best_snapshot(30).map(|s| s.taken_at_millis),
        Some(1_728_000_300_000)
    );
}

#[test]
fn an_empty_catalogue_has_no_best_snapshot() {
    assert_eq!(catalog(Vec::new()).best_snapshot(10), None);
    let with_one = catalog(vec![snapshot(5, 1, true)]);
    assert_eq!(with_one.best_snapshot(5).map(|s| s.position), Some(5));
}
