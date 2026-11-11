//! The admin override listing: data, text form, JSON field order and order independence
//! (WP-EXT-03).
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use proptest::prelude::*;
use serde_json::json;
use spot_app_api::{OverrideKind, StepOverride};
use spot_seams::{OverrideRowKind, RegistryBuilder};

fn site() -> RegistryBuilder {
    let mut b = base();
    b.add_override("ext", ov("core:b", OverrideKind::Wrap, Some("ext:x")));
    b.add_override("ext", ov("core:a", OverrideKind::Before, Some("ext:y")));
    b.add_override(
        "erp",
        StepOverride {
            target: sn("core:c"),
            kind: OverrideKind::Disable,
            by: None,
            reason: "no longer used by this site".to_owned(),
        },
    );
    b.add_seam_provider("tax", "tax:line", 1, Arc::new(Const(json!("v1"))));
    b.add_seam_provider("tax", "tax:line", 2, Arc::new(Const(json!("v2"))));
    b.replace_seam(
        "local_tax",
        "tax:line",
        1,
        Arc::new(Const(json!("local"))),
        "local tax rules",
    );
    b
}

fn label(k: OverrideRowKind) -> &'static str {
    match k {
        OverrideRowKind::Replace => "replace",
        OverrideRowKind::Wrap => "wrap",
        OverrideRowKind::Disable => "disable",
        OverrideRowKind::Reorder => "reorder",
    }
}

#[test]
fn the_report_lists_every_override_with_who_what_and_why() {
    let r = site().build().unwrap();
    let rep = r.override_report();
    let rows: Vec<(String, &str, String, usize, &str)> = rep
        .overrides
        .iter()
        .map(|o| {
            (
                o.app.clone(),
                label(o.kind),
                o.target_step.clone(),
                o.effective_order,
                o.reason.as_str(),
            )
        })
        .collect();
    // Rows are sorted by (app, kind, target step, ...), so `erp`'s disable comes first.
    assert_eq!(
        rows,
        vec![
            (
                "erp".to_owned(),
                "disable",
                "core:c".to_owned(),
                3,
                "no longer used by this site"
            ),
            // Within one app the rows sort by kind (`wrap` before `reorder`).
            (
                "ext".to_owned(),
                "wrap",
                "core:b".to_owned(),
                2,
                "Wrap core:b"
            ),
            (
                "ext".to_owned(),
                "reorder",
                "core:a".to_owned(),
                0,
                "Before core:a"
            ),
        ]
    );
    assert_eq!(rep.seams.len(), 2);
    assert_eq!(rep.seams[0].name, "tax:line");
    assert_eq!(rep.seams[0].version, 1);
    assert_eq!(rep.seams[0].provider_app, "tax");
    assert_eq!(rep.seams[0].replaced_by.as_deref(), Some("local_tax"));
    assert_eq!(rep.seams[0].reason, "local tax rules");
    assert_eq!(rep.seams[1].version, 2);
    assert_eq!(rep.seams[1].replaced_by, None);
}

#[test]
fn the_text_form_is_a_fixed_width_table() {
    let r = site().build().unwrap();
    let text = r.override_report().to_text();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("OVERRIDES"));
    let header = lines.next().unwrap();
    assert!(header.starts_with("app"), "{header:?}");
    assert!(header.contains("target_step"), "{header:?}");
    let rule = lines.next().unwrap();
    assert!(rule.chars().all(|c| c == '-' || c == ' '), "{rule:?}");
    assert!(text.contains("SEAMS"), "{text:?}");
    assert!(text.contains("local_tax"), "{text:?}");
    assert!(text.contains("no longer used by this site"), "{text:?}");
}

#[test]
fn the_json_form_has_a_stable_field_order() {
    let r = site().build().unwrap();
    let rep = r.override_report();
    // `to_string` writes struct fields in declaration order; a stable field order is part of
    // the listing's contract, so assert on the raw text.
    let s = serde_json::to_string(&rep).unwrap();
    let first = &s[s.find("\"overrides\"").unwrap()..];
    for key in [
        "\"app\"",
        "\"kind\"",
        "\"target_step\"",
        "\"doctype\"",
        "\"hook_point\"",
        "\"reason\"",
        "\"effective_order\"",
    ] {
        let at = first
            .find(key)
            .unwrap_or_else(|| panic!("{key} missing in {first}"));
        assert!(first.contains(key));
        let _ = at;
    }
    // The order in the raw text: app < kind < target_step < doctype < hook_point < reason
    // < effective_order, for the first row.
    let row_start = first.find('{').unwrap();
    let row_end = first[row_start..].find('}').unwrap() + row_start;
    let row = &first[row_start..=row_end];
    let mut at = 0;
    for key in [
        "\"app\"",
        "\"kind\"",
        "\"target_step\"",
        "\"doctype\"",
        "\"hook_point\"",
        "\"reason\"",
        "\"effective_order\"",
    ] {
        let next = row[at..]
            .find(key)
            .unwrap_or_else(|| panic!("{key} out of order in {row}"))
            + at;
        assert!(next >= at, "{key} out of order in {row}");
        at = next;
    }
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    // The first row after sorting is `erp`'s disable.
    assert_eq!(v["overrides"][0]["kind"], "disable");
    assert_eq!(v["overrides"][0]["hook_point"], "before_save");
    assert_eq!(v["seams"][0]["name"], "tax:line");
    assert_eq!(v["seams"][0]["replaced_by"], "local_tax");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, rng_seed: proptest::test_runner::RngSeed::Fixed(0x5EA_0002), ..ProptestConfig::default() })]

    #[test]
    fn registration_order_never_changes_the_report(keys in prop::array::uniform5(any::<u8>())) {
        // A permutation of the five contributions, from five random keys.
        fn perm(keys: [u8; 5]) -> Vec<u8> {
            let mut idx: Vec<usize> = (0..5).collect();
            idx.sort_by_key(|&i| (keys[i], i));
            idx.into_iter().map(|i| i as u8).collect()
        }
        fn build(order: &[u8]) -> spot_seams::Registry {
            let mut b = RegistryBuilder::new();
            b.add_controller("core", ctrl(
                "item",
                vec![
                    decl("core:a", before_save(), 10),
                    decl("core:b", before_save(), 20),
                    decl("core:c", before_save(), 30),
                ],
            ));
            b.add_controller("ext", ctrl(
                "item",
                vec![
                    decl("ext:x", before_save(), 50),
                    decl("ext:y", before_save(), 50),
                ],
            ));
            for k in order {
                match k {
                    0 => b.add_override("ext", ov("core:b", OverrideKind::Wrap, Some("ext:x"))),
                    1 => b.add_override("ext", ov("core:a", OverrideKind::Before, Some("ext:y"))),
                    2 => b.add_override("erp", StepOverride {
                        target: sn("core:c"),
                        kind: OverrideKind::Disable,
                        by: None,
                        reason: "unused".to_owned(),
                    }),
                    3 => b.add_seam_provider("tax", "tax:line", 1, Arc::new(Const(json!("v1")))),
                    _ => b.replace_seam("local_tax", "tax:line", 1, Arc::new(Const(json!("local"))), "rules"),
                };
            }
            b.build().unwrap()
        }
        let a = build(&perm(keys));
        let b = build(&[0, 1, 2, 3, 4]);
        prop_assert_eq!(a.override_report(), b.override_report());
        prop_assert_eq!(a.override_report().to_text(), b.override_report().to_text());
    }
}
