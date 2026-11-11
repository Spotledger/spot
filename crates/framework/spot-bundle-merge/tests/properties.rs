//! Property tests: the item-wise merge matches an independent reference model over 200 randomized
//! three-way cases, and the merged digest ignores the input bundles' entry order.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use proptest::prelude::*;
use spot_bundle_merge::merge3;
use spot_custom_bundle::{Bundle, FakeSite, Limits};

/// Rule expressions that always parse.
const EXPRESSIONS: &[&str] = &[
    "doc.a > 0",
    "doc.a > 1",
    "doc.b < 2",
    "doc.a == doc.b",
    "not doc.c",
    "doc.a + 1 < doc.b",
];

/// Rule names.
const NAMES: &[&str] = &["r0", "r1", "r2", "r3"];

/// A random map of rule name to expression.
fn rule_map() -> impl Strategy<Value = BTreeMap<String, String>> {
    prop::collection::btree_map(
        prop::sample::select(NAMES.to_vec()),
        prop::sample::select(EXPRESSIONS.to_vec()),
        0..=4,
    )
    .prop_map(|map| {
        map.into_iter()
            .map(|(name, expression)| (name.to_owned(), expression.to_owned()))
            .collect()
    })
}

/// A site carrying `rules`.
fn site_with_rules(rules: &BTreeMap<String, String>) -> FakeSite {
    let mut site = empty_site();
    for (name, expression) in rules {
        site = site.with_rule(name, expression);
    }
    site
}

/// An independent, naive three-way merge over `Option` maps, the reference the crate must match.
fn reference(
    base: &BTreeMap<String, String>,
    ours: &BTreeMap<String, String>,
    theirs: &BTreeMap<String, String>,
) -> (BTreeMap<String, String>, Vec<(String, String)>) {
    let mut keys: BTreeSet<&String> = BTreeSet::new();
    keys.extend(base.keys());
    keys.extend(ours.keys());
    keys.extend(theirs.keys());
    let mut merged = BTreeMap::new();
    let mut conflicts = Vec::new();
    for key in keys {
        let b = base.get(key);
        let o = ours.get(key);
        let t = theirs.get(key);
        if o == t {
            if let Some(value) = o {
                merged.insert(key.clone(), value.clone());
            }
        } else if o == b {
            if let Some(value) = t {
                merged.insert(key.clone(), value.clone());
            }
        } else if t == b {
            if let Some(value) = o {
                merged.insert(key.clone(), value.clone());
            }
        } else {
            let kind = if b.is_none() {
                "both_added_differently"
            } else if o.is_none() || t.is_none() {
                "modified_vs_removed"
            } else {
                "both_modified"
            };
            conflicts.push((key.clone(), kind.to_owned()));
        }
    }
    (merged, conflicts)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn merge3_matches_the_reference_model(
        base in rule_map(),
        ours in rule_map(),
        theirs in rule_map(),
    ) {
        let result = merge3(
            &bundle(&site_with_rules(&base), "base"),
            &bundle(&site_with_rules(&ours), "ours"),
            &bundle(&site_with_rules(&theirs), "theirs"),
        )
        .expect("merge");

        let merged: BTreeMap<String, String> = items_of(&result.merged, "rules")
            .into_iter()
            .map(|(key, bytes)| (key, String::from_utf8(bytes).expect("utf8")))
            .collect();
        let (expected, expected_conflicts) = reference(&base, &ours, &theirs);
        prop_assert_eq!(merged, expected);

        let mut got: Vec<(String, String)> = result
            .conflicts
            .iter()
            .map(|conflict| (conflict.key().to_owned(), conflict.kind.slug().to_owned()))
            .collect();
        got.sort();
        let mut want = expected_conflicts;
        want.sort();
        prop_assert_eq!(got, want);
    }
}

/// Reopens a bundle whose entries were written in the reverse order.
fn reordered(bundle: &Bundle) -> Bundle {
    let mut entries = entries_of(bundle);
    entries.reverse();
    Bundle::open(&raw_zip(&entries), &Limits::default(), &registry(), true)
        .expect("the reordered archive opens")
}

#[test]
fn merged_digest_is_independent_of_input_order() {
    let mut base = empty_site();
    base = base.with_rule("r0", "doc.a > 0");
    let mut ours = empty_site();
    ours = ours
        .with_rule("r0", "doc.a > 0")
        .with_rule("r1", "doc.b < 2");
    let mut theirs = empty_site();
    theirs = theirs
        .with_rule("r0", "doc.a > 0")
        .with_rule("r2", "not doc.c");

    let (b, o, t) = (
        bundle(&base, "base"),
        bundle(&ours, "ours"),
        bundle(&theirs, "theirs"),
    );
    let direct = merge3(&b, &o, &t).expect("merge");
    assert!(direct.is_clean());

    let shuffled = merge3(&reordered(&b), &reordered(&o), &reordered(&t)).expect("merge");
    assert_eq!(direct.merged.digest(), shuffled.merged.digest());
    assert_eq!(direct.merged.to_bytes(), shuffled.merged.to_bytes());
}

#[test]
fn a_clean_merge_is_symmetric() {
    let base = empty_site();
    let mut ours = empty_site();
    ours = ours.with_rule("r1", "doc.a > 1");
    let mut theirs = empty_site();
    theirs = theirs.with_rule("r2", "not doc.c");

    let (b, o, t) = (
        bundle(&base, "base"),
        bundle(&ours, "ours"),
        bundle(&theirs, "theirs"),
    );
    let forward = merge3(&b, &o, &t).expect("merge");
    let backward = merge3(&b, &t, &o).expect("merge");
    assert!(forward.is_clean() && backward.is_clean());
    // The header is our side's metadata, so the bundles' own bytes may differ; the merged content
    // does not: swapping the two sides of a clean merge yields the same items.
    for kind in ["meta", "views", "scripts", "rules", "fixtures"] {
        assert_eq!(
            items_of(&forward.merged, kind),
            items_of(&backward.merged, kind),
            "the merged {kind} items are symmetric"
        );
    }
}
