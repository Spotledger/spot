//! Registration order never changes a plan, an explanation or a conflict list.
#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_app_api::{Controller, HookPoint, OverrideKind::*, StepOverride};
use spot_seams::{Registry, RegistryBuilder};

fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    if items.len() <= 1 {
        return vec![items.to_vec()];
    }
    let mut out = Vec::new();
    for i in 0..items.len() {
        let mut rest = items.to_vec();
        let head = rest.remove(i);
        for mut p in permutations(&rest) {
            p.insert(0, head.clone());
            out.push(p);
        }
    }
    out
}

fn fingerprint(r: &Registry, doctypes: &[&str], points: &[HookPoint]) -> String {
    let mut s = String::new();
    for d in doctypes {
        for p in points {
            s += &format!("{d} {} {:?}\n", p.name(), names(r.plan(d, p)));
            for l in r.explain(d, p) {
                s += &format!("  {l:?}\n");
            }
        }
    }
    s
}

fn controllers() -> Vec<(&'static str, Arc<dyn Controller>)> {
    vec![
        (
            "core",
            ctrl(
                "item",
                vec![
                    decl("core:a", before_save(), 10),
                    decl("core:b", before_save(), 10),
                    decl("core:c", before_create(), 5),
                ],
            ),
        ),
        (
            "ext",
            ctrl(
                "item",
                vec![
                    decl("ext:x", HookPoint::Load, 1),
                    decl("ext:y", HookPoint::Load, 1),
                    decl("ext:z", before_save(), 1),
                ],
            ),
        ),
        (
            "sales",
            ctrl("order", vec![decl("sales:s", before_save(), 1)]),
        ),
        (
            "late",
            ctrl("item", vec![decl("late:l", before_save(), 10)]),
        ),
    ]
}

fn overrides() -> Vec<(&'static str, StepOverride)> {
    vec![
        ("ext", ov("core:a", Before, Some("ext:x"))),
        ("ext", ov("core:a", Before, Some("ext:y"))),
        ("ext", ov("core:b", Wrap, Some("late:l"))),
        ("late", ov("core:b", After, Some("ext:z"))),
        ("ext", ov("core:c", Disable, None)),
    ]
}

#[test]
fn any_registration_order_gives_identical_plans() {
    let points = [before_save(), before_create(), HookPoint::Load];
    let mut seen: Option<String> = None;
    let ctrls = controllers();
    let ovs = overrides();
    let ctrl_perms = permutations(&(0..ctrls.len()).collect::<Vec<_>>());
    let ov_perms = permutations(&(0..ovs.len()).collect::<Vec<_>>());
    assert_eq!(ctrl_perms.len() * ov_perms.len(), 24 * 120);
    for cp in &ctrl_perms {
        for op in &ov_perms {
            let mut b = RegistryBuilder::new();
            // Controllers and overrides interleave differently on each run too.
            for &i in cp {
                b.add_controller(ctrls[i].0, ctrls[i].1.clone());
            }
            for &i in op {
                b.add_override(ovs[i].0, ovs[i].1.clone());
            }
            let r = b.build().unwrap();
            let f = fingerprint(&r, &["item", "order"], &points);
            match &seen {
                None => seen = Some(f),
                Some(first) => assert_eq!(first, &f),
            }
        }
    }
    let first = seen.unwrap();
    assert!(
        first.contains("\"ext:x\", \"ext:y\", \"core:a\""),
        "{first}"
    );
}

#[test]
fn any_registration_order_gives_identical_conflicts() {
    let ctrls = controllers();
    let bad = [
        ("one", ov("core:a", Replace, Some("ext:x"))),
        ("two", ov("core:a", Replace, Some("late:l"))),
        ("ext", ov("core:zzz", After, Some("ext:z"))),
        ("ext", ov("core:b", Wrap, Some("core:c"))),
        ("ext", ov("core:c", Wrap, Some("core:b"))),
    ];
    let mut first: Option<Vec<String>> = None;
    for op in permutations(&(0..bad.len()).collect::<Vec<_>>()) {
        for cp in [vec![0, 1, 2, 3], vec![3, 2, 1, 0]] {
            let mut b = RegistryBuilder::new();
            for i in cp {
                b.add_controller(ctrls[i].0, ctrls[i].1.clone());
            }
            for &i in &op {
                b.add_override(bad[i].0, bad[i].1.clone());
            }
            let got: Vec<String> = b
                .build()
                .err()
                .unwrap()
                .iter()
                .map(|c| format!("{c:?}"))
                .collect();
            match &first {
                None => first = Some(got),
                Some(f) => assert_eq!(f, &got),
            }
        }
    }
}
