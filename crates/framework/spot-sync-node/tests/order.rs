#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Property: arrival order does not change the final applied set, and no record is applied
//! before the records it references.
mod common;

use common::*;
use proptest::prelude::*;
use spot_sync_closure::RecordKey;

/// Bundle i holds record `r{i}` and references the records of earlier bundles in `deps[i]`.
fn build(deps: &[Vec<usize>]) -> Vec<spot_sync_closure::Bundle> {
    deps.iter()
        .enumerate()
        .map(|(i, d)| {
            let ids: Vec<String> = d.iter().map(|j| format!("r{j}")).collect();
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            bundle(vec![rec(&format!("r{i}"), (i as u64 + 1) * 10, &refs)])
        })
        .collect()
}

fn deps_strategy() -> impl Strategy<Value = Vec<Vec<usize>>> {
    (2usize..8).prop_flat_map(|n| {
        (0..n)
            .map(|i| prop::collection::vec(0..i.max(1), if i == 0 { 0..1 } else { 0..3 }))
            .collect::<Vec<_>>()
    })
}

proptest! {
    #[test]
    fn any_arrival_order_gives_the_same_applied_set(
        deps in deps_strategy(),
        seed in any::<u64>(),
    ) {
        let deps: Vec<Vec<usize>> = deps
            .into_iter()
            .enumerate()
            .map(|(i, d)| if i == 0 { Vec::new() } else { d.into_iter().filter(|j| *j < i).collect() })
            .collect();
        let bundles = build(&deps);

        // Reference run: in order.
        let mut ordered = intake();
        for (t, b) in bundles.iter().enumerate() {
            ordered.receive(deliver(b.clone()), t as u64).expect("ok");
        }

        // Shuffled run (deterministic from the seed).
        let mut idx: Vec<usize> = (0..bundles.len()).collect();
        let mut s = seed | 1;
        for i in (1..idx.len()).rev() {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            idx.swap(i, (s % (i as u64 + 1)) as usize);
        }
        let mut shuffled = intake();
        for (t, i) in idx.iter().enumerate() {
            shuffled.receive(deliver(bundles[*i].clone()), t as u64).expect("ok");
        }

        prop_assert_eq!(&ordered.applier().stored, &shuffled.applier().stored);
        prop_assert_eq!(ordered.applier().stored.len(), bundles.len());
        prop_assert_eq!(ordered.applied_prefix(TENANT), shuffled.applied_prefix(TENANT));
        prop_assert_eq!(shuffled.applied_prefix(TENANT), Some(bundles.len() as u64 * 10));

        // No record before its references.
        let order = &shuffled.applier().order;
        for (i, d) in deps.iter().enumerate() {
            let me = order.iter().position(|k| k.1 == RecordKey::new("Doc", format!("r{i}")));
            for j in d {
                let dep = order.iter().position(|k| k.1 == RecordKey::new("Doc", format!("r{j}")));
                prop_assert!(dep < me);
            }
        }
    }
}
