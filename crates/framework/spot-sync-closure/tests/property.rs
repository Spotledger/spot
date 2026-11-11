#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Property run on random reference graphs: every record in every bundle has all of its
//! references in an earlier bundle, the same bundle, or known to the destination.
mod common;

use common::{Dest, Src, k, node};
use proptest::prelude::*;
use spot_sync_closure::{Limits, NoRedaction, References, StaticReferences, build};
use std::collections::BTreeSet;

proptest! {
    #[test]
    fn every_reference_is_earlier_same_bundle_or_known(
        n in 2usize..24,
        edges in proptest::collection::vec((0usize..24, 0usize..24), 0..60),
        positions in proptest::collection::vec(0u64..40, 24),
        owned in proptest::collection::vec(any::<bool>(), 24),
        in_scope in proptest::collection::vec(any::<bool>(), 24),
        extras_flag in proptest::collection::vec(any::<bool>(), 24),
        acked in proptest::option::of(0u64..40),
        n_roots in 1usize..4,
        max_records in 1usize..6,
    ) {
        let id = |i: usize| format!("N{i}");
        let mut links: Vec<Vec<String>> = vec![Vec::new(); n];
        for (a, b) in edges { if a < n && b < n { links[a].push(id(b)); } }
        let mut src = Src::default();
        let mut imgs = Vec::new();
        for i in 0..n {
            let l: Vec<&str> = links[i].iter().map(String::as_str).collect();
            imgs.push(src.put("t", &id(i), node(&l), positions[i]));
        }
        let mut dest = Dest { acked, ..Dest::default() };
        for i in 0..n {
            if owned[i] { dest.owned.insert(k("t", &id(i))); }
            if in_scope[i] { dest.scope_ids.insert(k("t", &id(i))); }
            if extras_flag[i] { dest.extras.record_delivered_extra(k("t", &id(i))); }
        }
        let refs = StaticReferences::new().with_fixed_child("t", "rows", "to", "t");
        let roots: Vec<_> = imgs.iter().take(n_roots.min(n)).cloned().collect();
        let limits = Limits { max_records, ..Limits::default() };
        let plan = build(&roots, &[], &dest, &src, &refs, &NoRedaction, limits).unwrap();

        let known = |key: &spot_sync_closure::RecordKey| {
            dest.owned.contains(key)
                || dest.extras.contains(key)
                || (dest.scope_ids.contains(key)
                    && acked.is_some_and(|a| src.images[key].position <= a))
        };
        let mut seen: BTreeSet<_> = BTreeSet::new();
        for bundle in &plan.bundles {
            let here: BTreeSet<_> = bundle.records.iter().map(|r| r.key.clone()).collect();
            for r in &bundle.records {
                for t in refs.references(&r.key.entity, &src.images[&r.key].image).unwrap() {
                    prop_assert!(seen.contains(&t) || here.contains(&t) || known(&t), "{:?} -> {:?}", r.key, t);
                }
            }
            seen.extend(here);
        }
        // No record appears in two bundles.
        let total: usize = plan.bundles.iter().map(|b| b.records.len()).sum();
        prop_assert_eq!(total, seen.len());
    }
}
