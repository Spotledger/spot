#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Random link graphs: every in-scope record once, links never forward, cycles whole.

mod common;

use common::*;
use proptest::prelude::*;
use spot_sync_bootstrap::{Bootstrap, Budget, RecordKey, StartOptions};
use spot_sync_closure::Limits;
use std::collections::BTreeMap;

proptest! {
    #[test]
    fn dependency_order_and_scope_hold_on_random_graphs(
        n in 1_usize..40,
        edges in proptest::collection::vec((0_usize..40, 0_usize..40), 0..80),
        outside in proptest::collection::vec(0_usize..40, 0..10),
        max_records in 1_usize..8,
        budget_records in 1_usize..8,
    ) {
        let mut links: Vec<Vec<String>> = vec![Vec::new(); n];
        for (a, b) in edges {
            if a < n && b < n {
                links[a].push(format!("Invoice/v{b:02}"));
            }
        }
        for o in outside {
            if o < n {
                links[o].push("PayrollRun/p0".to_owned());
            }
        }
        let mut records: Vec<_> = (0..n)
            .map(|i| {
                let l: Vec<&str> = links[i].iter().map(String::as_str).collect();
                rec("Invoice", &format!("v{i:02}"), 1, &l)
            })
            .collect();
        records.push(rec("PayrollRun", "p0", 1, &[]));
        let src = source(records);
        let limits = Limits { max_records, max_bytes: 1 << 20, hard_limit_records: 1_000 };
        let mut s = Bootstrap::start(
            &src, seams(&SCOPE), limits, &node("n"), hub(), StartOptions::default(),
        ).unwrap();
        let budget = Budget { max_bytes: usize::MAX, max_records: budget_records };

        let mut bundle_of: BTreeMap<RecordKey, usize> = BTreeMap::new();
        let mut images = BTreeMap::new();
        let mut index = 0_usize;
        while let Some(c) = s.next_chunk(budget).unwrap() {
            for b in c.bundles {
                for r in b.records {
                    prop_assert!(bundle_of.insert(r.key.clone(), index).is_none());
                    images.insert(r.key, r.image);
                }
                index += 1;
            }
        }
        prop_assert_eq!(bundle_of.len(), n);
        prop_assert!(bundle_of.keys().all(|k| k.entity == "Invoice"));
        for (key, image) in &images {
            for t in image["links"].as_array().unwrap() {
                let (e, i) = t.as_str().unwrap().split_once('/').unwrap();
                if let Some(target) = bundle_of.get(&RecordKey::new(e, i)) {
                    prop_assert!(*target <= bundle_of[key], "{key:?} before {t}");
                }
            }
        }
    }
}
