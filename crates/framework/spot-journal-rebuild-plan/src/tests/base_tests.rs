//! The live chain a catalogue describes, and what a base knows about itself.

use super::{cat, fork, ranges, snapshot};
use crate::base::LiveChain;
use crate::catalog::FIRST_POSITION;
use crate::{Base, PlanError};

#[test]
fn the_catalogue_must_describe_one_live_history() {
    let cases: Vec<(&str, crate::JournalCatalog, PlanError)> = vec![
        (
            "a range with no positions",
            cat(vec![], ranges(1, &[(11, 10)]), vec![]),
            PlanError::SegmentRangeInvalid {
                generation: 1,
                first_seq: 11,
                last_seq: 10,
            },
        ),
        (
            "two ranges that claim the same positions",
            cat(vec![], ranges(1, &[(1, 10), (5, 15)]), vec![]),
            PlanError::SegmentsInconsistent {
                generation: 1,
                position: 5,
            },
        ),
        (
            "the same range twice",
            cat(vec![], ranges(1, &[(1, 10), (1, 10)]), vec![]),
            PlanError::SegmentsInconsistent {
                generation: 1,
                position: 1,
            },
        ),
        (
            "a fork point inside a sealed segment",
            cat(vec![], ranges(1, &[(1, 10), (11, 20)]), vec![fork(2, 15)]),
            PlanError::SegmentsInconsistent {
                generation: 1,
                position: 15,
            },
        ),
        (
            "a new generation with no fork recorded",
            cat(
                vec![],
                {
                    let mut s = ranges(1, &[(1, 10)]);
                    s.extend(ranges(2, &[(11, 20)]));
                    s
                },
                vec![],
            ),
            PlanError::UnrecordedFork {
                generation: 2,
                position: 11,
            },
        ),
        (
            "no segment at all",
            cat(vec![], vec![], vec![]),
            PlanError::NoSegments,
        ),
        (
            "every range abandoned by its own fork",
            cat(vec![], ranges(1, &[(1, 10)]), vec![fork(2, 0)]),
            PlanError::NoSegments,
        ),
    ];
    for (label, catalog, want) in cases {
        assert_eq!(
            LiveChain::build(&catalog).unwrap_err(),
            want,
            "the live chain refused {label} for the wrong reason"
        );
    }
}

#[test]
fn the_live_chain_keeps_history_up_to_the_fork_and_drops_the_tail() {
    let mut segments = ranges(1, &[(1, 10), (11, 20), (21, 30)]);
    segments.extend(ranges(2, &[(11, 20)]));
    let chain = LiveChain::build(&cat(vec![], segments, vec![fork(2, 10)])).expect("one history");
    assert_eq!(chain.start, FIRST_POSITION);
    assert_eq!(chain.end, 20, "generation 2 ends the live history");
    assert_eq!(chain.current_generation, 2);
    assert_eq!(
        chain
            .ranges
            .iter()
            .map(|r| (r.generation, r.first_seq, r.last_seq))
            .collect::<Vec<_>>(),
        vec![(1, 1, 10), (2, 11, 20)]
    );
    assert_eq!(
        chain
            .abandoned
            .iter()
            .map(|r| (r.generation, r.first_seq, r.last_seq))
            .collect::<Vec<_>>(),
        vec![(1, 11, 20), (1, 21, 30)],
        "generation 1's positions after the fork are abandoned"
    );
}

#[test]
fn fork_generations_chain_and_a_fork_may_run_ahead_of_its_segments() {
    let mut chained = ranges(1, &[(1, 10)]);
    chained.extend(ranges(2, &[(11, 20)]));
    chained.extend(ranges(3, &[(21, 30)]));
    let chain = LiveChain::build(&cat(vec![], chained, vec![fork(2, 10), fork(3, 20)]))
        .expect("three in a row");
    assert_eq!(chain.current_generation, 3);
    assert_eq!((chain.start, chain.end), (1, 30));
    assert!(chain.forked_at(3, 20));
    assert!(!chain.forked_at(3, 19));

    let mut segments = ranges(1, &[(1, 10), (11, 20)]);
    segments.truncate(2);
    let chain = LiveChain::build(&cat(vec![], segments, vec![fork(5, 10)]))
        .expect("a fork whose generation has not written yet");
    assert_eq!(chain.current_generation, 5, "the fork names the generation");
    assert_eq!(chain.end, 10, "nothing after the fork point is live");
    assert_eq!(chain.abandoned.len(), 1);
}

#[test]
fn a_base_knows_where_it_starts() {
    assert_eq!(Base::Empty.position(), 0);
    assert_eq!(Base::Empty.generation(), None);
    let snapshot_base = Base::Snapshot {
        position: 40,
        generation: 2,
    };
    assert_eq!(snapshot_base.position(), 40);
    assert_eq!(snapshot_base.generation(), Some(2));
    let backup_base = Base::Backup { position: 25 };
    assert_eq!(backup_base.position(), 25);
    assert_eq!(
        backup_base.generation(),
        None,
        "a backup carries no generation"
    );
}

#[test]
fn the_chain_finds_the_generation_a_position_belongs_to() {
    let mut segments = ranges(1, &[(1, 10)]);
    segments.extend(ranges(2, &[(11, 20)]));
    let chain = LiveChain::build(&cat(vec![], segments, vec![fork(2, 10)])).expect("one history");
    assert_eq!(chain.generation_at(5), Some(1));
    assert_eq!(chain.generation_at(10), Some(1));
    assert_eq!(chain.generation_at(11), Some(2));
    assert_eq!(chain.generation_at(0), None);
}

#[test]
fn a_snapshot_at_the_end_of_a_chain_needs_no_step() {
    let catalog = cat(vec![snapshot(20, 2, true)], ranges(2, &[(1, 20)]), vec![]);
    let plan = crate::plan(&catalog, None, crate::Target::End).expect("the snapshot is the end");
    assert_eq!(plan.steps, vec![]);
    assert_eq!(plan.ends_at, 20);
    assert!(plan.preserves_positions);
}
