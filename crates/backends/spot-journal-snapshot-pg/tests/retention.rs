//! The retention plan: pure, so every rule is checked against plain snapshot lists.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_journal_rebuild_plan::SnapshotInfo;
use spot_journal_snapshot_pg::retention::{Retention, plan};

const DAY: i64 = 86_400_000;

fn snap(generation: u64, position: u64, day: i64, verified: bool) -> SnapshotInfo {
    SnapshotInfo {
        position,
        generation,
        verified,
        taken_at_millis: day * DAY + 3_600_000,
    }
}

#[test]
fn it_keeps_seven_dailies_and_four_weeklies() {
    // 40 daily snapshots, one per day, newest on day 39.
    let infos: Vec<SnapshotInfo> = (0..40)
        .map(|d| snap(1, 100 + u64::try_from(d).unwrap(), d, false))
        .collect();
    let result = plan(&infos, Retention::default());
    let kept: std::collections::BTreeSet<_> = result.keep.iter().copied().collect();
    for day in 33..40 {
        assert!(
            kept.contains(&(1, 100 + u64::try_from(day).unwrap())),
            "daily {day}"
        );
    }
    // Weekly buckets are 7-day windows: the newest of each of the 4 newest weeks.
    assert!(
        result.keep.len() >= 7 && result.keep.len() <= 11,
        "{}",
        result.keep.len()
    );
    assert_eq!(result.keep.len() + result.prune.len(), 40);
    assert!(result.prune.contains(&(1, 100)), "the oldest goes");
}

#[test]
fn it_never_drops_the_only_verified_snapshot_however_old() {
    let mut infos: Vec<SnapshotInfo> = (10..40)
        .map(|d| snap(1, u64::try_from(d).unwrap(), d, false))
        .collect();
    infos.push(snap(1, 5, 0, true));
    let result = plan(&infos, Retention::default());
    assert!(result.keep.contains(&(1, 5)), "the verified one stays");
}

#[test]
fn it_keeps_the_newest_snapshot_of_every_generation() {
    let infos = vec![
        snap(1, 10, 0, false),
        snap(1, 20, 1, false),
        snap(2, 5, 30, false),
        snap(3, 7, 60, false),
    ];
    let result = plan(
        &infos,
        Retention {
            daily: 1,
            weekly: 1,
        },
    );
    assert!(result.keep.contains(&(1, 20)), "newest of generation 1");
    assert!(result.keep.contains(&(2, 5)));
    assert!(result.keep.contains(&(3, 7)));
    assert!(result.prune.contains(&(1, 10)));
}

#[test]
fn an_empty_list_has_an_empty_plan() {
    assert!(plan(&[], Retention::default()).keep.is_empty());
}
