//! The rules of the plan, one table per rule.

use super::{Want, at, backup, cat, fork, ranges, run, snapshot, steps_of};
use crate::{
    Base, JournalCatalog, PlanError, RebuildPlan, SegmentRange, SnapshotInfo, Target, plan,
};

/// One generation, positions 1..=30, three segments of ten.
fn one_to_thirty() -> Vec<SegmentRange> {
    ranges(1, &[(1, 10), (11, 20), (21, 30)])
}

/// The whole journal to its end with no base: the yardstick for the other tables.
fn whole(catalog: JournalCatalog) -> RebuildPlan {
    plan(&catalog, None, Target::End).expect("the journal is complete")
}

#[test]
fn only_verified_snapshots_are_trusted() {
    run(
        "only verified snapshots are trusted",
        vec![
            (
                "an unverified snapshot is ignored, so the rebuild starts from the first position",
                cat(vec![snapshot(10, 1, false)], one_to_thirty(), vec![]),
                None,
                Target::End,
                Want::Plan {
                    base: 0,
                    steps: vec![(1, 1, 10), (1, 11, 20), (1, 21, 30)],
                    ends_at: 30,
                },
            ),
            (
                "the verified snapshot wins over a newer unverified one",
                cat(
                    vec![snapshot(10, 1, true), snapshot(20, 1, false)],
                    one_to_thirty(),
                    vec![],
                ),
                None,
                Target::End,
                Want::Plan {
                    base: 10,
                    steps: vec![(1, 11, 20), (1, 21, 30)],
                    ends_at: 30,
                },
            ),
            (
                "the unverified snapshot does not hide the verified one",
                cat(
                    vec![snapshot(20, 1, false), snapshot(10, 1, true)],
                    one_to_thirty(),
                    vec![],
                ),
                None,
                Target::End,
                Want::Plan {
                    base: 10,
                    steps: vec![(1, 11, 20), (1, 21, 30)],
                    ends_at: 30,
                },
            ),
            (
                "no verified snapshot at all: the segments carry the rebuild from the start",
                cat(vec![snapshot(10, 1, false)], one_to_thirty(), vec![]),
                None,
                at(10),
                Want::Plan {
                    base: 0,
                    steps: vec![(1, 1, 10)],
                    ends_at: 10,
                },
            ),
        ],
    );
}

#[test]
fn the_latest_snapshot_at_or_before_the_target_wins() {
    let snapshots = || vec![snapshot(10, 1, true), snapshot(20, 1, true)];
    run(
        "the latest snapshot at or before the target wins",
        vec![
            (
                "a target past both snapshots uses the later one",
                cat(snapshots(), one_to_thirty(), vec![]),
                None,
                at(25),
                Want::Plan {
                    base: 20,
                    steps: vec![(1, 21, 25)],
                    ends_at: 25,
                },
            ),
            (
                "a target on a snapshot uses it: nothing to apply",
                cat(snapshots(), one_to_thirty(), vec![]),
                None,
                at(20),
                Want::Plan {
                    base: 20,
                    steps: vec![],
                    ends_at: 20,
                },
            ),
            (
                "a target between snapshots uses the earlier one",
                cat(snapshots(), one_to_thirty(), vec![]),
                None,
                at(19),
                Want::Plan {
                    base: 10,
                    steps: vec![(1, 11, 19)],
                    ends_at: 19,
                },
            ),
            (
                "a snapshot after the target is not a base",
                cat(snapshots(), one_to_thirty(), vec![]),
                None,
                at(5),
                Want::Plan {
                    base: 0,
                    steps: vec![(1, 1, 5)],
                    ends_at: 5,
                },
            ),
            (
                "the end of the journal uses the latest snapshot",
                cat(snapshots(), one_to_thirty(), vec![]),
                None,
                Target::End,
                Want::Plan {
                    base: 20,
                    steps: vec![(1, 21, 30)],
                    ends_at: 30,
                },
            ),
        ],
    );
}

#[test]
fn a_tie_at_one_position_prefers_the_higher_generation() {
    let catalog = cat(
        vec![snapshot(10, 1, true), snapshot(10, 2, true)],
        ranges(1, &[(1, 10)]),
        vec![fork(2, 10)],
    );
    let mut with_second = catalog.clone();
    with_second.segments.extend(ranges(2, &[(11, 20)]));
    let plan = whole(with_second);
    assert_eq!(
        plan.base,
        Base::Snapshot {
            position: 10,
            generation: 2
        }
    );
    assert_eq!(steps_of(&plan), vec![(2, 11, 20)]);
    assert_eq!(plan.ends_at, 20);
}

#[test]
fn to_the_end_keeps_the_generation_and_preserves_positions() {
    let catalog = cat(vec![snapshot(20, 1, true)], one_to_thirty(), vec![]);
    let plan = whole(catalog);
    assert_eq!(plan.ends_at, 30);
    assert_eq!(
        plan.base,
        Base::Snapshot {
            position: 20,
            generation: 1
        }
    );
    assert_eq!(
        plan.new_generation, None,
        "nothing was lost, so nothing forks"
    );
    assert_eq!(plan.fork_point, None);
    assert!(plan.preserves_positions);
    assert!(plan.projectors_after);
    assert!(plan.invariant_suite_after);
}

#[test]
fn a_target_before_the_end_forks() {
    run(
        "a target before the end forks",
        vec![
            (
                "a point in time inside the current generation forks into the next one",
                cat(vec![], one_to_thirty(), vec![]),
                None,
                at(15),
                Want::Plan {
                    base: 0,
                    steps: vec![(1, 1, 10), (1, 11, 15)],
                    ends_at: 15,
                },
            ),
            (
                "reaching the end of a segment is not a fork",
                cat(vec![], one_to_thirty(), vec![]),
                None,
                at(20),
                Want::Plan {
                    base: 0,
                    steps: vec![(1, 1, 10), (1, 11, 20)],
                    ends_at: 20,
                },
            ),
        ],
    );
    let plan = plan(
        &cat(vec![snapshot(10, 1, true)], one_to_thirty(), vec![]),
        None,
        at(10),
    )
    .expect("a snapshot exactly at the target needs no steps");
    assert_eq!(plan.steps, vec![], "the snapshot already holds the target");
    assert_eq!(
        plan.fork_point
            .map(|f| (f.from_position, f.abandoned_generation)),
        Some((10, 1)),
        "the plan records where history is cut and which generation it abandons"
    );
    assert_eq!(plan.new_generation, Some(2));
    assert!(!plan.preserves_positions, "the abandoned generation is cut");
}

#[test]
fn a_target_inside_an_abandoned_generation_is_refused() {
    // Generation 1 wrote to 30; a rebuild to 10 forked generation 2, which wrote 11..=20. The
    // positions 21..=30 of generation 1 are abandoned: the live history does not continue them.
    let catalog = || {
        let mut segments = one_to_thirty();
        segments.extend(ranges(2, &[(11, 20)]));
        cat(vec![], segments, vec![fork(2, 10)])
    };
    run(
        "a target inside an abandoned generation is refused",
        vec![
            (
                "the live history ends at 20, so 25 is abandoned history",
                catalog(),
                None,
                at(25),
                Want::Error(PlanError::TargetInsideAbandonedGeneration {
                    generation: 1,
                    position: 25,
                }),
            ),
            (
                "the last abandoned position is refused too",
                catalog(),
                None,
                at(30),
                Want::Error(PlanError::TargetInsideAbandonedGeneration {
                    generation: 1,
                    position: 30,
                }),
            ),
            (
                "a target the live history still covers is planned, and forks into generation 3",
                catalog(),
                None,
                at(15),
                Want::Plan {
                    base: 0,
                    steps: vec![(1, 1, 10), (2, 11, 15)],
                    ends_at: 15,
                },
            ),
            (
                "the end of the live history is generation 2's end",
                catalog(),
                None,
                Target::End,
                Want::Plan {
                    base: 0,
                    steps: vec![(1, 1, 10), (2, 11, 20)],
                    ends_at: 20,
                },
            ),
        ],
    );
    let plan = plan(&catalog(), None, at(15)).expect("15 is live");
    assert_eq!(
        plan.new_generation,
        Some(3),
        "generation 2 is the current one"
    );
    assert_eq!(
        plan.fork_point.map(|f| f.abandoned_generation),
        Some(2),
        "position 15 sits in generation 2, whose positions after it are abandoned"
    );
}

#[test]
fn a_gap_names_the_first_missing_position() {
    let holed = || ranges(1, &[(1, 10), (21, 30)]);
    run(
        "a gap names the first missing position",
        vec![
            (
                "a hole in the segments at the start of a rebuild",
                cat(vec![], holed(), vec![]),
                None,
                Target::End,
                Want::Error(PlanError::GapAt { position: 11 }),
            ),
            (
                "a hole after a snapshot",
                cat(vec![snapshot(5, 1, true)], holed(), vec![]),
                None,
                Target::End,
                Want::Error(PlanError::GapAt { position: 11 }),
            ),
            (
                "a hole right after the snapshot's position",
                cat(vec![snapshot(11, 1, true)], holed(), vec![]),
                None,
                Target::End,
                Want::Error(PlanError::GapAt { position: 12 }),
            ),
            (
                "a snapshot inside the hole carries the rebuild over it",
                cat(vec![snapshot(20, 1, true)], holed(), vec![]),
                None,
                Target::End,
                Want::Plan {
                    base: 20,
                    steps: vec![(1, 21, 30)],
                    ends_at: 30,
                },
            ),
        ],
    );
}

#[test]
fn no_base_when_nothing_covers_the_start() {
    run(
        "no base when nothing covers the start",
        vec![
            (
                "the segments start at 11 and there is no snapshot or backup",
                cat(vec![], ranges(1, &[(11, 20)]), vec![]),
                None,
                Target::End,
                Want::Error(PlanError::NoBase),
            ),
            (
                "the only snapshot is after the target and the segments start at 11",
                cat(vec![snapshot(30, 1, true)], ranges(1, &[(11, 20)]), vec![]),
                None,
                Target::End,
                Want::Error(PlanError::NoBase),
            ),
            (
                "a snapshot at the start makes the same journal plannable",
                cat(vec![snapshot(15, 1, true)], ranges(1, &[(11, 20)]), vec![]),
                None,
                Target::End,
                Want::Plan {
                    base: 15,
                    steps: vec![(1, 16, 20)],
                    ends_at: 20,
                },
            ),
            (
                "a catalogue with no segment at all has no journal to rebuild",
                cat(vec![snapshot(10, 1, true)], vec![], vec![]),
                None,
                Target::End,
                Want::Error(PlanError::NoSegments),
            ),
        ],
    );
}

#[test]
fn a_newer_backup_is_preferred_only_when_the_segments_cover_it() {
    let journal = || one_to_thirty();
    run(
        "a newer backup is preferred only when the segments cover it",
        vec![
            (
                "a covered backup newer than the snapshot is rolled forward",
                cat(vec![snapshot(10, 1, true)], journal(), vec![]),
                Some(backup(20)),
                Target::End,
                Want::Plan {
                    base: 20,
                    steps: vec![(1, 21, 30)],
                    ends_at: 30,
                },
            ),
            (
                "a backup older than the snapshot is ignored",
                cat(vec![snapshot(20, 1, true)], journal(), vec![]),
                Some(backup(10)),
                Target::End,
                Want::Plan {
                    base: 20,
                    steps: vec![(1, 21, 30)],
                    ends_at: 30,
                },
            ),
            (
                "a backup after the target cannot be rolled forward to it",
                cat(vec![snapshot(10, 1, true)], journal(), vec![]),
                Some(backup(25)),
                at(20),
                Want::Plan {
                    base: 10,
                    steps: vec![(1, 11, 20)],
                    ends_at: 20,
                },
            ),
            (
                "a backup with no snapshot at all is the base",
                cat(vec![], journal(), vec![]),
                Some(backup(10)),
                Target::End,
                Want::Plan {
                    base: 10,
                    steps: vec![(1, 11, 20), (1, 21, 30)],
                    ends_at: 30,
                },
            ),
            (
                "a backup the segments do not cover is refused, not planned anyway",
                cat(vec![], ranges(1, &[(1, 10), (21, 30)]), vec![]),
                Some(backup(10)),
                Target::End,
                Want::Error(PlanError::GapAt { position: 11 }),
            ),
            (
                "a backup with a hole right after it is refused with the hole, not with no base",
                cat(vec![], ranges(1, &[(21, 30)]), vec![]),
                Some(backup(10)),
                Target::End,
                Want::Error(PlanError::GapAt { position: 11 }),
            ),
        ],
    );
}

#[test]
fn an_equal_backup_does_not_displace_the_snapshot() {
    let plan = plan(
        &cat(vec![snapshot(20, 1, true)], one_to_thirty(), vec![]),
        Some(&backup(20)),
        Target::End,
    )
    .expect("both reach the end");
    assert_eq!(
        plan.base,
        Base::Snapshot {
            position: 20,
            generation: 1
        },
        "a backup must be newer, not merely as new"
    );
}

#[test]
fn a_snapshot_inside_a_range_applies_the_rest_of_it() {
    run(
        "a snapshot inside a range applies the rest of it",
        vec![
            (
                "the range is split at the snapshot's position",
                cat(vec![snapshot(15, 1, true)], one_to_thirty(), vec![]),
                None,
                Target::End,
                Want::Plan {
                    base: 15,
                    steps: vec![(1, 16, 20), (1, 21, 30)],
                    ends_at: 30,
                },
            ),
            (
                "a target inside the same range stops it at the target",
                cat(vec![snapshot(15, 1, true)], one_to_thirty(), vec![]),
                None,
                at(17),
                Want::Plan {
                    base: 15,
                    steps: vec![(1, 16, 17)],
                    ends_at: 17,
                },
            ),
        ],
    );
}

#[test]
fn a_target_after_the_end_is_refused() {
    run(
        "a target after the end is refused",
        vec![
            (
                "one position past the end",
                cat(vec![], ranges(1, &[(1, 10), (11, 20)]), vec![]),
                None,
                at(21),
                Want::Error(PlanError::TargetBeyondEnd {
                    end: 20,
                    position: 21,
                }),
            ),
            (
                "far past the end",
                cat(vec![], ranges(1, &[(1, 10), (11, 20)]), vec![]),
                None,
                at(99),
                Want::Error(PlanError::TargetBeyondEnd {
                    end: 20,
                    position: 99,
                }),
            ),
        ],
    );
}

#[test]
fn a_zero_target_plans_an_empty_rebuild_that_forks() {
    let plan = plan(&cat(vec![], one_to_thirty(), vec![]), None, at(0)).expect("an empty rebuild");
    assert_eq!(plan.base, Base::Empty);
    assert_eq!(plan.steps, vec![]);
    assert_eq!(plan.ends_at, 0);
    assert_eq!(
        plan.fork_point
            .map(|f| (f.from_position, f.abandoned_generation)),
        Some((0, 1)),
        "everything the journal holds is abandoned"
    );
    assert_eq!(plan.new_generation, Some(2));
}

#[test]
fn a_generation_may_only_be_entered_at_a_recorded_fork() {
    let mut segments = ranges(1, &[(1, 10)]);
    segments.extend(ranges(2, &[(11, 20)]));
    let unrecorded = cat(vec![], segments.clone(), vec![fork(2, 11)]);
    assert_eq!(
        plan(&unrecorded, None, Target::End).unwrap_err(),
        PlanError::UnrecordedFork {
            generation: 2,
            position: 11
        },
        "the fork is recorded at 11, but generation 2 begins at 11 and must fork at 10"
    );
    let recorded = cat(vec![], segments, vec![fork(2, 10)]);
    let plan = plan(&recorded, None, Target::End).expect("a recorded fork is the whole point");
    assert_eq!(steps_of(&plan), vec![(1, 1, 10), (2, 11, 20)]);
    assert_eq!(plan.new_generation, None, "crossing a fork is not forking");
    assert!(plan.preserves_positions);
}

#[test]
fn a_snapshot_may_not_change_the_generation_mid_history() {
    let catalog = cat(
        vec![snapshot(10, 2, false), snapshot(10, 2, true)],
        ranges(1, &[(1, 20)]),
        vec![],
    );
    assert_eq!(
        plan(&catalog, None, Target::End).unwrap_err(),
        PlanError::UnrecordedFork {
            generation: 1,
            position: 11
        },
        "a snapshot in generation 2 cannot be continued by generation 1 without a fork"
    );
}

#[test]
fn the_steps_are_contiguous_from_the_base_to_the_target() {
    let mut segments = ranges(1, &[(1, 10), (11, 20)]);
    segments.extend(ranges(2, &[(21, 25)]));
    let catalog = cat(vec![snapshot(5, 1, true)], segments, vec![fork(2, 20)]);
    let plan = plan(&catalog, None, Target::End).expect("contiguous across one fork");
    assert_eq!(
        steps_of(&plan),
        vec![(1, 6, 10), (1, 11, 20), (2, 21, 25)],
        "each step starts one after the previous step"
    );
    let mut position = plan.base_position();
    for step in &plan.steps {
        assert_eq!(step.first_seq(), position + 1);
        assert!(step.first_seq() <= step.last_seq());
        position = step.last_seq();
    }
    assert_eq!(position, plan.ends_at);
}

#[test]
fn the_plan_is_deterministic() {
    let catalog = cat(
        vec![snapshot(10, 1, true), snapshot(20, 1, true)],
        one_to_thirty(),
        vec![],
    );
    let first: Result<RebuildPlan, PlanError> = plan(&catalog, Some(&backup(15)), Target::End);
    for _ in 0..8 {
        assert_eq!(plan(&catalog, Some(&backup(15)), Target::End), first);
    }
    assert_eq!(
        plan(&catalog, Some(&backup(15)), Target::End).unwrap().base,
        Base::Snapshot {
            position: 20,
            generation: 1
        },
        "the backup at 15 is older than the snapshot at 20"
    );
}

#[test]
fn a_verified_snapshot_is_used_even_when_a_snapshot_record_is_untrusted() {
    let trusted: SnapshotInfo = snapshot(20, 1, true);
    let catalog = cat(
        vec![trusted, snapshot(20, 1, false)],
        one_to_thirty(),
        vec![],
    );
    assert_eq!(
        plan(&catalog, None, Target::End).unwrap().base,
        Base::Snapshot {
            position: 20,
            generation: 1
        }
    );
}

#[test]
fn a_snapshot_of_an_abandoned_generation_is_never_the_base() {
    // Generation 1 ran to 100, was cut back to 50 by a point-in-time rebuild, and generation 2
    // continued from 51. The snapshot at 80 holds abandoned history.
    let mut segments = ranges(1, &[(1, 50), (51, 100)]);
    segments.extend(ranges(2, &[(51, 120)]));
    let catalog = cat(
        vec![snapshot(40, 1, true), snapshot(80, 1, true)],
        segments,
        vec![fork(2, 50)],
    );
    let plan = plan(&catalog, None, Target::End).expect("the snapshot at 40 is still good");
    assert_eq!(
        plan.base,
        Base::Snapshot {
            position: 40,
            generation: 1
        },
        "the snapshot at 80 belongs to the abandoned part of generation 1"
    );
    assert_eq!(steps_of(&plan), vec![(1, 41, 50), (2, 51, 120)]);
}
