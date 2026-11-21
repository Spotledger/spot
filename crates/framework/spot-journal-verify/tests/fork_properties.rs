//! Property (WP-JRN-03.T10): any sequence of appends and forks yields manifests that `validate`
//! accepts, and a plan whose steps cover each live position exactly once, with positions starting
//! again at 1 in every generation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use proptest::prelude::*;
use spot_journal_manifest::{Manifest, SegmentSummary, append_segments};
use spot_journal_rebuild_plan::{ForkInfo, JournalCatalog, SegmentRange, Target, plan};

/// One thing that happens to the journal.
#[derive(Clone, Debug)]
enum Op {
    /// Seal a segment of this many entries in the current generation.
    Append(u64),
    /// Fork at the `n`th segment boundary (modulo the segments the generation holds).
    Fork(usize),
}

fn ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            3 => (1u64..6).prop_map(Op::Append),
            1 => (0usize..8).prop_map(Op::Fork),
        ],
        1..24,
    )
}

fn hash(n: u64) -> [u8; 32] {
    let mut out = [0_u8; 32];
    for (slot, byte) in out.iter_mut().zip(n.to_be_bytes().iter().cycle()) {
        *slot = *byte;
    }
    out
}

proptest! {
    #[test]
    fn every_history_validates_and_its_plan_covers_each_live_position_once(ops in ops()) {
        let mut manifest = Manifest::first("acme-site", 0).unwrap();
        let mut history: Vec<Manifest> = Vec::new();
        let mut counter = 1_u64;
        // The live length of each generation that was cut by a fork.
        let mut cut: Vec<u64> = Vec::new();
        for op in ops {
            match op {
                Op::Append(len) => {
                    let first = manifest.next_seq();
                    let previous = manifest
                        .segments
                        .last()
                        .map_or(hash(0), |s| s.segment_hash);
                    counter += 1;
                    let segment = SegmentSummary::new(
                        manifest.generation,
                        first,
                        first + len - 1,
                        len,
                        hash(counter),
                        previous,
                    );
                    manifest = append_segments(&manifest, &[segment]).unwrap();
                }
                Op::Fork(n) => {
                    if manifest.segments.is_empty() {
                        continue;
                    }
                    let point = manifest.segments[n % manifest.segments.len()].last_seq;
                    let next = manifest.fork(point, "rebuild", 0, 0).unwrap();
                    history.push(manifest.clone());
                    cut.push(point);
                    manifest = next;
                }
            }
            prop_assert!(manifest.validate().is_ok());
        }
        history.push(manifest.clone());
        for each in &history {
            prop_assert!(each.validate().is_ok());
        }

        // The planner's catalogue: every manifest's segments and the newest manifest's forks.
        let catalog = JournalCatalog {
            snapshots: Vec::new(),
            segments: history
                .iter()
                .flat_map(|m| m.segments.iter())
                .map(|s| SegmentRange {
                    generation: s.generation,
                    first_seq: s.first_seq,
                    last_seq: s.last_seq,
                })
                .collect(),
            forks: manifest
                .forks
                .iter()
                .map(|f| ForkInfo {
                    generation: f.generation,
                    from_position: f.forked_from_position,
                })
                .collect(),
        };

        // The live positions: each cut generation up to its fork point, the newest to its end.
        let mut live: BTreeSet<(u64, u64)> = BTreeSet::new();
        for (index, len) in cut.iter().enumerate() {
            let generation = u64::try_from(index).unwrap() + 1;
            live.extend((1..=*len).map(|seq| (generation, seq)));
        }
        live.extend((1..=manifest.last_seq()).map(|seq| (manifest.generation, seq)));

        // A history with no sealed segment at all has nothing to plan.
        if catalog.segments.is_empty() {
            prop_assert!(plan(&catalog, None, Target::End).is_err());
            return Ok(());
        }
        let planned = plan(&catalog, None, Target::End).unwrap();
        let mut covered: Vec<(u64, u64)> = Vec::new();
        for step in &planned.steps {
            covered.extend((step.first_seq()..=step.last_seq()).map(|seq| (step.generation(), seq)));
        }
        let unique: BTreeSet<(u64, u64)> = covered.iter().copied().collect();
        prop_assert_eq!(covered.len(), unique.len(), "a position is planned twice");
        prop_assert_eq!(unique, live);
        prop_assert!(planned.preserves_positions);
    }
}
