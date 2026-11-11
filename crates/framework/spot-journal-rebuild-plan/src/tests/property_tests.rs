//! Properties over random catalogues: whatever the input, a returned plan is walkable in order,
//! never lies about where it starts or ends, never changes a generation without a recorded fork
//! and never changes between two runs. No catalogue makes the planner panic.

use proptest::prelude::*;

use crate::{
    Backup, ForkInfo, JournalCatalog, PlanError, RebuildPlan, SegmentRange, SnapshotInfo, Target,
    plan,
};

/// A journal built forward from the first position: segments of one or more generations, with a
/// recorded fork where a generation changes (or, on purpose, sometimes not), and holes in
/// coverage.
fn journal() -> impl Strategy<Value = (JournalCatalog, Option<Backup>, Target)> {
    (
        1u64..=2,
        prop::collection::vec(
            (1u64..=4u64, any::<bool>(), any::<bool>(), any::<bool>()),
            1..6,
        ),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(start, blocks, verified, mid_segment)| {
            let mut position = start;
            let mut generation = 1u64;
            let mut segments: Vec<SegmentRange> = Vec::new();
            let mut forks: Vec<ForkInfo> = Vec::new();
            for (index, (len, next_generation, record_fork, hole)) in blocks.into_iter().enumerate()
            {
                if index > 0 && hole {
                    position += 1;
                }
                if index > 0 && next_generation {
                    if record_fork {
                        forks.push(ForkInfo {
                            generation: generation + 1,
                            from_position: position - 1,
                        });
                    }
                    generation += 1;
                }
                segments.push(SegmentRange {
                    generation,
                    first_seq: position,
                    last_seq: position + len - 1,
                });
                position += len;
            }
            let snapshots: Vec<SnapshotInfo> = segments
                .iter()
                .step_by(2)
                .map(|s| SnapshotInfo {
                    position: if mid_segment { s.first_seq } else { s.last_seq },
                    generation: s.generation,
                    verified,
                    taken_at_millis: i64::try_from(s.last_seq).unwrap_or(0),
                })
                .collect();
            let end = position.saturating_sub(1);
            let catalog = JournalCatalog {
                snapshots,
                segments,
                forks,
            };
            (catalog, end)
        })
        .prop_flat_map(|(catalog, end)| {
            (
                Just(catalog),
                prop::option::of(0u64..=end.saturating_add(1)),
                prop_oneof![
                    Just(Target::End),
                    (0u64..=end.saturating_add(2)).prop_map(Target::At),
                ],
            )
                .prop_map(|(catalog, position, target)| {
                    (
                        catalog,
                        position.map(|position| Backup { position }),
                        target,
                    )
                })
        })
}

/// A catalogue with no shape at all: repeated, reversed, overlapping and unrecorded ranges.
/// Positions and generations stay in their domain (both start at one).
fn messy() -> impl Strategy<Value = (JournalCatalog, Option<Backup>, Target)> {
    (
        prop::collection::vec((1u64..3, 1u64..6, any::<bool>(), -4i64..4), 0..3),
        prop::collection::vec((1u64..3, 1u64..9, 1u64..9), 0..4),
        prop::collection::vec((1u64..3, 1u64..9), 0..3),
        prop::option::of(1u64..9),
        prop_oneof![Just(Target::End), (1u64..11).prop_map(Target::At)],
    )
        .prop_map(|(snapshots, segments, forks, backup, target)| {
            let catalog = JournalCatalog {
                snapshots: snapshots
                    .into_iter()
                    .map(
                        |(generation, position, verified, taken_at_millis)| SnapshotInfo {
                            position,
                            generation,
                            verified,
                            taken_at_millis,
                        },
                    )
                    .collect(),
                segments: segments
                    .into_iter()
                    .map(|(generation, first_seq, last_seq)| SegmentRange {
                        generation,
                        first_seq,
                        last_seq,
                    })
                    .collect(),
                forks: forks
                    .into_iter()
                    .map(|(generation, from_position)| ForkInfo {
                        generation,
                        from_position,
                    })
                    .collect(),
            };
            (catalog, backup.map(|position| Backup { position }), target)
        })
}

/// The shape every returned plan must have.
fn check(catalog: &JournalCatalog, backup: Option<&Backup>, target: Target) -> Result<(), String> {
    let first: Result<RebuildPlan, PlanError> = plan(catalog, backup, target);
    if plan(catalog, backup, target) != first {
        return Err("the same input planned twice differently".to_owned());
    }
    let Ok(built) = first else {
        return Ok(());
    };
    let mut position = built.base_position();
    let mut generation = built.base.generation();
    for step in &built.steps {
        if step.first_seq() != position.saturating_add(1) {
            return Err(format!(
                "step {step:?} does not continue position {position}"
            ));
        }
        if step.first_seq() > step.last_seq() {
            return Err(format!("step {step:?} is empty or reversed"));
        }
        if let Some(previous) = generation
            && previous != step.generation()
            && !catalog.forks.contains(&ForkInfo {
                generation: step.generation(),
                from_position: position,
            })
        {
            return Err(format!(
                "step {step:?} enters generation {} at {position} with no fork recorded",
                step.generation()
            ));
        }
        position = step.last_seq();
        generation = Some(step.generation());
    }
    if position != built.ends_at {
        return Err(format!(
            "the steps end at {position}, the plan says {}",
            built.ends_at
        ));
    }
    if let Target::At(wanted) = target
        && wanted != built.ends_at
    {
        return Err(format!("the plan ends at {} not {wanted}", built.ends_at));
    }
    if built.preserves_positions != built.fork_point.is_none() {
        return Err("preserves_positions disagrees with the fork point".to_owned());
    }
    if built.new_generation.is_some() != built.fork_point.is_some() {
        return Err(
            "a new generation without a fork, or a fork without a new generation".to_owned(),
        );
    }
    if let Some(fork) = built.fork_point
        && (fork.from_position != built.ends_at || fork.abandoned_generation == 0)
    {
        return Err("the fork point is not the end of the plan".to_owned());
    }
    if !built.projectors_after || !built.invariant_suite_after {
        return Err("a rebuild always runs the projectors and the invariant suite".to_owned());
    }
    Ok(())
}

proptest! {
    #[test]
    fn a_returned_plan_walks_the_journal_in_order((catalog, backup, target) in journal()) {
        if let Err(reason) = check(&catalog, backup.as_ref(), target) {
            prop_assert!(false, "{reason} for catalogue {catalog:?}, backup {backup:?}, target {target:?}");
        }
    }

    #[test]
    fn an_unshaped_catalogue_is_refused_or_planned_in_order((catalog, backup, target) in messy()) {
        if let Err(reason) = check(&catalog, backup.as_ref(), target) {
            prop_assert!(false, "{reason} for catalogue {catalog:?}, backup {backup:?}, target {target:?}");
        }
    }
}
