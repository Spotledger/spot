#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Properties over random graphs and random gaps (ADR-064 section 8, ADR-066 section 2):
//! no record is ever placed before a record it references unless they share a group, and the mode
//! is always the one the rules describe.

mod common;

use common::{MapReferences, rid};
use proptest::prelude::*;
use spot_sync_catchup::{
    ClassCounts, GapSummary, Image, Mode, ModeReason, Payload, Settings, Share, choose_mode,
    order_by_dependency,
};
use std::collections::BTreeMap;

proptest! {
    #[test]
    fn no_record_precedes_a_reference_unless_they_share_a_group(
        n in 1usize..30,
        edges in proptest::collection::vec((0usize..30, 0usize..30), 0..80),
        positions in proptest::collection::vec(0u64..50, 30),
    ) {
        let name = |i: usize| format!("r{i}");
        let mut images = Vec::with_capacity(n);
        for (i, position) in positions.iter().enumerate().take(n) {
            images.push(Image::new(rid(&name(i)), *position, Payload::new("{}")));
        }
        let mut refs = MapReferences::new();
        for (from, to) in edges {
            if from < n && to < n {
                refs = refs.link(&name(from), &name(to));
            }
        }
        let groups = order_by_dependency(&images, &refs).unwrap();

        let mut group_of: BTreeMap<String, usize> = BTreeMap::new();
        let mut seen = 0usize;
        for (index, group) in groups.iter().enumerate() {
            for record in group {
                group_of.insert(record.id.clone(), index);
                seen += 1;
            }
        }
        prop_assert_eq!(seen, n, "every image appears exactly once");

        for i in 0..n {
            let here = group_of[&name(i)];
            for target in refs.targets(&rid(&name(i))) {
                if let Some(&there) = group_of.get(&target) {
                    prop_assert!(
                        there <= here,
                        "{} appears before the record it references ({})",
                        name(i),
                        target
                    );
                }
            }
        }

        // The same input always gives the same plan.
        let again = order_by_dependency(&images, &refs).unwrap();
        prop_assert_eq!(groups, again);
    }

    #[test]
    fn the_mode_is_always_the_one_the_rules_describe(
        applied in 0u64..1_000,
        end in 0u64..2_000,
        floor in 0u64..2_000,
        count in 0u64..20_000,
        coalesced_bytes in 0u64..1_000_000,
        snapshot_bytes in 0u64..1_000_000,
        new_or_resync in any::<bool>(),
        basis_points in 0u16..=10_000,
    ) {
        let stream_end = end.max(applied);
        let summary = GapSummary {
            applied_prefix: applied,
            stream_end,
            hot_retention_floor: floor.min(stream_end),
            in_scope_event_count: count,
            class_counts: ClassCounts::default(),
            estimated_coalesced_records: 0,
            estimated_coalesced_bytes: coalesced_bytes,
            estimated_snapshot_bytes: snapshot_bytes,
            node_is_new_or_resync: new_or_resync,
        };
        let settings = Settings {
            snapshot_fraction: Share::from_basis_points(basis_points).unwrap(),
            ..Settings::default()
        };
        let decision = choose_mode(&summary, &settings).unwrap();

        let over = count > settings.incremental_limit;
        let outside = applied < summary.hot_retention_floor;
        let expensive = u128::from(coalesced_bytes) * u128::from(Share::WHOLE)
            > u128::from(snapshot_bytes) * u128::from(basis_points);

        if new_or_resync {
            prop_assert_eq!(decision.mode, Mode::Snapshot);
            prop_assert_eq!(decision.reasons, vec![ModeReason::NewOrResync]);
        } else {
            match decision.mode {
                Mode::Incremental => {
                    prop_assert!(!over && !outside);
                    prop_assert_eq!(
                        decision.reasons,
                        vec![ModeReason::SmallGap {
                            in_scope_event_count: count,
                            incremental_limit: settings.incremental_limit,
                        }]
                    );
                }
                Mode::Coalesced => {
                    prop_assert!(over || outside);
                    prop_assert!(!expensive);
                }
                Mode::Snapshot => {
                    prop_assert!(over || outside);
                    prop_assert!(expensive);
                }
            }
        }
    }
}
