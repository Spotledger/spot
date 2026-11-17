//! Positions are `(generation, seq)` (ADR-061 amendment A.0, WP-JRN-05.T7): a catalogue the relay
//! wrote after a fork (generation 2 starts again at 1) is accepted, planned in `(generation,
//! position)` order, and a gap inside one generation is still an error.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_journal_rebuild_plan::{
    ForkInfo, JournalCatalog, PlanError, RebuildPlan, SegmentRange, Target, plan,
};

fn range(generation: u64, first_seq: u64, last_seq: u64) -> SegmentRange {
    SegmentRange {
        generation,
        first_seq,
        last_seq,
    }
}

/// Generation 1 sealed 1..=4, 5..=6 and 7..=8; a fork at 6 (a sealed boundary: a fork point inside
/// a sealed segment is refused) opened generation 2, which holds 1..=3.
fn catalog(second: &[(u64, u64)]) -> JournalCatalog {
    let mut segments = vec![range(1, 1, 4), range(1, 5, 6), range(1, 7, 8)];
    segments.extend(second.iter().map(|&(a, b)| range(2, a, b)));
    JournalCatalog {
        snapshots: Vec::new(),
        segments,
        forks: vec![ForkInfo {
            generation: 2,
            from_position: 6,
        }],
    }
}

/// Every `(generation, position)` a plan applies, in order.
fn covered(plan: &RebuildPlan) -> Vec<(u64, u64)> {
    plan.steps
        .iter()
        .flat_map(|s| (s.first_seq()..=s.last_seq()).map(|p| (s.generation(), p)))
        .collect()
}

fn pairs(generation: u64, last: u64) -> impl Iterator<Item = (u64, u64)> {
    (1..=last).map(move |p| (generation, p))
}

#[test]
fn a_post_fork_catalogue_is_planned_in_generation_then_position_order() {
    let planned = plan(&catalog(&[(1, 3)]), None, Target::End).unwrap();
    // The abandoned 7 and 8 of generation 1 are never applied; generation 2 restarts at 1.
    let want: Vec<(u64, u64)> = pairs(1, 6).chain(pairs(2, 3)).collect();
    assert_eq!(covered(&planned), want);
    assert_eq!(planned.ends_at, 3);
    assert!(planned.preserves_positions);
    assert!(planned.fork_point.is_none());
}

#[test]
fn a_target_in_the_current_generation_forks_a_third() {
    let planned = plan(&catalog(&[(1, 3)]), None, Target::At(2)).unwrap();
    let want: Vec<(u64, u64)> = pairs(1, 6).chain(pairs(2, 2)).collect();
    assert_eq!(covered(&planned), want);
    let fork = planned.fork_point.unwrap();
    assert_eq!((fork.from_position, fork.abandoned_generation), (2, 2));
    assert_eq!(planned.new_generation, Some(3));
    assert!(!planned.preserves_positions);
}

#[test]
fn a_target_in_an_older_generation_is_refused() {
    let catalog = catalog(&[(1, 3)]);
    // Live history of generation 1, but the manifest chain cannot record a fork from there.
    assert_eq!(
        plan(
            &catalog,
            None,
            Target::In {
                generation: 1,
                position: 5
            }
        )
        .unwrap_err(),
        PlanError::TargetBeforeFork {
            generation: 1,
            position: 5,
            current_generation: 2
        }
    );
    // After the fork point the position belongs to abandoned history.
    assert_eq!(
        plan(
            &catalog,
            None,
            Target::In {
                generation: 1,
                position: 7
            }
        )
        .unwrap_err(),
        PlanError::TargetInsideAbandonedGeneration {
            generation: 1,
            position: 7
        }
    );
}

#[test]
fn a_gap_inside_one_generation_is_still_an_error() {
    let error = plan(&catalog(&[(1, 2), (4, 5)]), None, Target::End).unwrap_err();
    assert_eq!(
        error,
        PlanError::GapAt {
            generation: 2,
            position: 3
        }
    );
}
