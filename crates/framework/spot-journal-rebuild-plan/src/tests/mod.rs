//! Tests of the rebuild planner: one table-driven suite per rule, then a property run.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod base_tests;
mod catalog_tests;
mod explain_tests;
mod plan_tests;
mod property_tests;

use crate::{
    Backup, ForkInfo, JournalCatalog, PlanError, RebuildPlan, SegmentRange, SnapshotInfo, Target,
};

/// A segment range of one generation, for the terse tables below.
pub(crate) fn ranges(generation: u64, spans: &[(u64, u64)]) -> Vec<SegmentRange> {
    spans
        .iter()
        .map(|(first_seq, last_seq)| SegmentRange {
            generation,
            first_seq: *first_seq,
            last_seq: *last_seq,
        })
        .collect()
}

/// A catalogue from its three parts.
pub(crate) fn cat(
    snapshots: Vec<SnapshotInfo>,
    segments: Vec<SegmentRange>,
    forks: Vec<ForkInfo>,
) -> JournalCatalog {
    JournalCatalog {
        snapshots,
        segments,
        forks,
    }
}

/// A verified snapshot, taken a millisecond later than the one before it.
pub(crate) fn snapshot(position: u64, generation: u64, verified: bool) -> SnapshotInfo {
    SnapshotInfo {
        position,
        generation,
        verified,
        taken_at_millis: 1_728_000_000_000 + position as i64,
    }
}

pub(crate) fn fork(generation: u64, from_position: u64) -> ForkInfo {
    ForkInfo {
        generation,
        from_position,
    }
}

pub(crate) fn backup(position: u64) -> Backup {
    Backup { position }
}

pub(crate) fn at(position: u64) -> Target {
    Target::At(position)
}

/// What a case expects: a plan (base position, steps, end) or a refusal.
#[derive(Debug, PartialEq)]
pub(crate) enum Want {
    /// A plan that reaches `ends_at` through `steps` from `base`.
    Plan {
        /// The last position the base holds.
        base: u64,
        /// `(generation, first_seq, last_seq)` per step.
        steps: Vec<(u64, u64, u64)>,
        /// The last position the plan applies.
        ends_at: u64,
    },
    /// A refusal.
    Error(PlanError),
}

/// The steps of a plan as `(generation, first_seq, last_seq)`.
pub(crate) fn steps_of(plan: &RebuildPlan) -> Vec<(u64, u64, u64)> {
    plan.steps
        .iter()
        .map(|s| (s.generation(), s.first_seq(), s.last_seq()))
        .collect()
}

/// Runs a table of cases: `(label, catalogue, backup, target, want)`.
pub(crate) fn run(name: &str, cases: Vec<(&str, JournalCatalog, Option<Backup>, Target, Want)>) {
    for (label, catalog, backup, target, want) in cases {
        let got = crate::plan(&catalog, backup.as_ref(), target);
        match (&want, &got) {
            (
                Want::Plan {
                    base,
                    steps,
                    ends_at,
                },
                Ok(plan),
            ) => {
                assert_eq!(plan.base_position(), *base, "{name} / {label}: base");
                assert_eq!(&steps_of(plan), steps, "{name} / {label}: steps");
                assert_eq!(plan.ends_at, *ends_at, "{name} / {label}: ends_at");
            }
            (Want::Error(expected), Err(actual)) => {
                assert_eq!(actual, expected, "{name} / {label}: refusal");
            }
            _ => panic!("{name} / {label}: expected {want:?}, got {got:?}"),
        }
    }
}
