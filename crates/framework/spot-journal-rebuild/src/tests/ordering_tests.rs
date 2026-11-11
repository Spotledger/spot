//! Gaps, regressions, duplicates, plan consistency, bounded memory.

use spot_journal_rebuild_plan::{Base, RebuildPlan, Step, Target};
use spot_journal_segment::SegmentReader;

use super::block_on;
use super::fixture::{Ev, Journal, insert, options, update};
use crate::{Call, RebuildError, RecordingSink, execute};

fn run(
    journal: &Journal,
    plan: &RebuildPlan,
    sink: &mut RecordingSink,
) -> Result<crate::RebuildOutcome, RebuildError> {
    block_on(execute(
        &journal.store,
        &journal.keys(),
        &spot_image_upcast::Registry::new(),
        plan,
        sink,
        &options(1),
    ))
}

fn hand_plan(base: Base, steps: Vec<Step>, ends_at: u64) -> RebuildPlan {
    RebuildPlan {
        base,
        steps,
        ends_at,
        new_generation: None,
        fork_point: None,
        projectors_after: true,
        invariant_suite_after: true,
        preserves_positions: true,
    }
}

fn range(first_seq: u64, last_seq: u64) -> Step {
    Step::ApplySegmentRange {
        generation: 1,
        first_seq,
        last_seq,
    }
}

#[test]
fn a_hole_between_the_base_and_the_first_step_is_a_position_gap_with_nothing_applied() {
    let journal = Journal::new((1..=20).map(|n| insert(n, 1)).collect(), 5);
    let snapshot = journal.snapshot(10);
    let plan = hand_plan(
        Base::Snapshot {
            position: snapshot.position,
            generation: 1,
        },
        vec![range(12, 20)],
        20,
    );
    let mut sink = RecordingSink::new();
    let error = run(&journal, &plan, &mut sink).unwrap_err();
    assert_eq!(
        error,
        RebuildError::PositionGap {
            generation: 1,
            expected: 11,
            found: 12
        }
    );
    assert_eq!(error.code(), "journal_rebuild.position_gap");
    assert!(sink.calls().is_empty());
}

#[test]
fn a_hole_between_two_steps_is_a_position_gap() {
    let journal = Journal::new((1..=20).map(|n| insert(n, 1)).collect(), 5);
    let plan = hand_plan(Base::Empty, vec![range(1, 5), range(7, 9)], 9);
    let mut sink = RecordingSink::new();
    let error = run(&journal, &plan, &mut sink).unwrap_err();
    assert_eq!(
        error,
        RebuildError::PositionGap {
            generation: 1,
            expected: 6,
            found: 7
        }
    );
    assert!(sink.calls().is_empty());
}

#[test]
fn a_range_the_journal_does_not_hold_is_a_missing_segment() {
    let journal = Journal::new((1..=10).map(|n| insert(n, 1)).collect(), 5);
    let plan = hand_plan(Base::Empty, vec![range(1, 14)], 14);
    let mut sink = RecordingSink::new();
    let error = run(&journal, &plan, &mut sink).unwrap_err();
    assert_eq!(
        error,
        RebuildError::SegmentMissing {
            generation: 1,
            first_seq: 11,
            last_seq: 14
        }
    );
    assert!(sink.calls().is_empty());
}

#[test]
fn a_version_that_goes_backwards_stops_the_rebuild_and_aborts_the_sink() {
    let journal = Journal::new(
        vec![insert(1, 1), update(1, 3), update(1, 2), insert(2, 1)],
        4,
    );
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    let error = run(&journal, &plan, &mut sink).unwrap_err();
    assert!(
        matches!(
            &error,
            RebuildError::VersionRegressed {
                seq: 3,
                applied: 3,
                found: 2,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.code(), "journal_rebuild.version_regressed");
    assert_eq!(sink.aborted(), Some("journal_rebuild.version_regressed"));
    assert!(sink.state().is_none(), "nothing is visible after an abort");
}

#[test]
fn a_version_below_the_snapshots_is_a_regression_too() {
    let journal = Journal::new(
        vec![insert(1, 1), update(1, 2), update(1, 3), update(1, 2)],
        4,
    );
    let snapshot = journal.snapshot(3);
    let plan = journal.plan(vec![snapshot], Target::End);
    let mut sink = RecordingSink::new();
    let error = run(&journal, &plan, &mut sink).unwrap_err();
    assert!(matches!(
        error,
        RebuildError::VersionRegressed {
            applied: 3,
            found: 2,
            ..
        }
    ));
}

#[test]
fn the_same_version_delivered_again_is_skipped_and_counted() {
    let journal = Journal::new(
        vec![insert(1, 1), update(1, 2), update(1, 2), update(1, 3)],
        4,
    );
    let plan = journal.plan(vec![], Target::End);
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &plan, &mut sink).unwrap();
    assert_eq!((outcome.state_applied, outcome.duplicates_skipped), (3, 1));
    let images = sink
        .calls()
        .iter()
        .filter(|c| matches!(c, Call::Image(_)))
        .count();
    assert_eq!(images, 3);
}

#[test]
fn an_inconsistent_plan_is_refused_before_anything_is_read() {
    let journal = Journal::new((1..=10).map(|n| insert(n, 1)).collect(), 5);
    let mut sink = RecordingSink::new();
    let cases = [
        hand_plan(Base::Empty, vec![range(1, 5)], 9),
        hand_plan(Base::Empty, vec![range(5, 1)], 1),
        hand_plan(Base::Empty, vec![range(1, 5), range(3, 8)], 8),
        hand_plan(Base::Empty, vec![], 0),
        RebuildPlan {
            new_generation: Some(2),
            ..hand_plan(Base::Empty, vec![range(1, 5)], 5)
        },
    ];
    for plan in cases {
        let error = run(&journal, &plan, &mut sink).unwrap_err();
        assert!(matches!(error, RebuildError::PlanInvalid(_)), "{error:?}");
    }
    assert!(sink.calls().is_empty());
}

#[test]
fn memory_stays_within_one_block() {
    let events: Vec<Ev> = (0..400_u64)
        .map(|i| {
            if i < 7 {
                insert(1 + i, 1)
            } else {
                update(1 + i % 7, 1 + i / 7)
            }
        })
        .collect();
    let journal = Journal::new(events, 200);
    let plan = journal.plan(vec![], Target::End);
    let largest_block = journal
        .segments
        .iter()
        .map(|s| {
            let reader = SegmentReader::open(s.bytes.clone()).unwrap();
            reader
                .block_index()
                .iter()
                .map(|b| b.count as usize)
                .max()
                .unwrap()
        })
        .max()
        .unwrap();
    let blocks: usize = journal
        .segments
        .iter()
        .map(|s| {
            SegmentReader::open(s.bytes.clone())
                .unwrap()
                .block_index()
                .len()
        })
        .sum();
    assert!(blocks > 4, "the fixture needs several blocks, got {blocks}");
    let mut sink = RecordingSink::new();
    let outcome = run(&journal, &plan, &mut sink).unwrap();
    assert_eq!(outcome.entries_read, 400);
    assert!(
        outcome.peak_buffered_entries <= largest_block,
        "{} > {largest_block}",
        outcome.peak_buffered_entries
    );
    assert!(outcome.peak_buffered_entries < 400 / 2);
}
