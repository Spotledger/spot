//! Differential test against the stub host's ordering.
//!
//! `reference_plan` reproduces `StubHost::plan` from `contracts/app-api/src/stub/mod.rs`
//! (fn `plan`, private there) step for step, with the two rules this crate fixes that the stub
//! leaves open: a step used as a `by` step is detached from its own point, and the overrides of
//! one step are visited in (priority, name) order of their `by` steps (the stub visits them in
//! installation order, which would make the result depend on it).
#![allow(clippy::unwrap_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use proptest::prelude::*;
use spot_app_api::{HookPoint, OverrideKind, StepName, StepOverride};
use spot_seams::RegistryBuilder;

#[derive(Clone, Debug)]
struct Spec {
    /// (app index, point index, priority) of the 5 target steps then the 4 `by` steps.
    steps: Vec<(u8, u8, i32)>,
    /// Per target: main kind (0 none, 1 disable, 2 replace, 3 wrap), its `by`, befores, afters.
    targets: Vec<(u8, usize, Vec<bool>, Vec<bool>)>,
    shuffle: Vec<u8>,
}

fn spec() -> impl Strategy<Value = Spec> {
    (
        prop::collection::vec((0u8..2, 0u8..2, -3i32..4), 9),
        prop::collection::vec(
            (
                0u8..4,
                0usize..4,
                prop::collection::vec(any::<bool>(), 4),
                prop::collection::vec(any::<bool>(), 4),
            ),
            5,
        ),
        prop::collection::vec(any::<u8>(), 16),
    )
        .prop_map(|(steps, targets, shuffle)| Spec {
            steps,
            targets,
            shuffle,
        })
}

fn step_name(i: usize, app: u8) -> StepName {
    let app = if app == 0 { "a" } else { "b" };
    sn(&format!("{app}:s{i}"))
}

fn point(i: u8) -> HookPoint {
    if i == 0 {
        before_save()
    } else {
        before_create()
    }
}

fn overrides_of(s: &Spec) -> Vec<StepOverride> {
    let name = |i: usize| step_name(i, s.steps[i].0);
    let mut out = Vec::new();
    for (t, (kind, by, before, after)) in s.targets.iter().enumerate() {
        let mk = |kind, by: Option<usize>| StepOverride {
            target: name(t),
            kind,
            by: by.map(|b| name(5 + b)),
            reason: "r".into(),
        };
        match kind {
            1 => out.push(mk(OverrideKind::Disable, None)),
            2 => out.push(mk(OverrideKind::Replace, Some(*by))),
            3 => out.push(mk(OverrideKind::Wrap, Some(*by))),
            _ => {}
        }
        for (b, on) in before.iter().enumerate() {
            if *on {
                out.push(mk(OverrideKind::Before, Some(b)));
            }
        }
        for (b, on) in after.iter().enumerate() {
            if *on {
                out.push(mk(OverrideKind::After, Some(b)));
            }
        }
    }
    out
}

/// The stub host's `plan`, over plain data. Returns `step` or `wrapper(wrapped)` per entry.
fn reference_plan(s: &Spec, overrides: &[StepOverride], at: &HookPoint) -> Vec<String> {
    let all: BTreeMap<StepName, (i32, HookPoint)> = (0..9)
        .map(|i| {
            let (app, p, prio) = s.steps[i];
            (step_name(i, app), (prio, point(p)))
        })
        .collect();
    let detached: BTreeSet<&StepName> = overrides.iter().filter_map(|o| o.by.as_ref()).collect();
    let mut decls: Vec<(i32, StepName)> = all
        .iter()
        .filter(|(n, (_, p))| p == at && !detached.contains(n))
        .map(|(n, (prio, _))| (*prio, n.clone()))
        .collect();
    decls.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    // The deviation: visit overrides in (priority, name) order of `by`.
    let mut ordered: Vec<&StepOverride> = overrides.iter().collect();
    ordered.sort_by_key(|o| o.by.as_ref().map(|b| (all[b].0, b.clone())));
    let mut out = Vec::new();
    for (_, step) in decls {
        let ovs: Vec<&&StepOverride> = ordered.iter().filter(|o| o.target == step).collect();
        let mut main: Option<(StepName, Option<StepName>)> = Some((step.clone(), None));
        let mut before = Vec::new();
        let mut after = Vec::new();
        for o in ovs {
            let by = o.by.clone();
            match o.kind {
                OverrideKind::Disable => main = None,
                OverrideKind::Replace => main = by.map(|b| (b, None)),
                OverrideKind::Wrap => {
                    if let (Some(m), Some(b)) = (main.take(), by) {
                        main = Some((b, Some(m.0)));
                    }
                }
                OverrideKind::Before => before.extend(by.map(|b| (b, None))),
                OverrideKind::After => after.extend(by.map(|b| (b, None))),
            }
        }
        out.extend(before);
        out.extend(main);
        out.extend(after);
    }
    out.into_iter()
        .map(|(n, w)| match w {
            Some(w) => format!("{n}({w})"),
            None => n.to_string(),
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, rng_seed: proptest::test_runner::RngSeed::Fixed(0x5EA_0001), ..ProptestConfig::default() })]

    #[test]
    fn plan_matches_the_stub_hosts_ordering(s in spec()) {
        let overrides = overrides_of(&s);
        // Controllers per app, overrides installed in a shuffled order.
        let mut b = RegistryBuilder::new();
        for app in [0u8, 1] {
            let decls = (0..9)
                .filter(|&i| s.steps[i].0 == app)
                .map(|i| decl(step_name(i, app).as_str(), point(s.steps[i].1), s.steps[i].2))
                .collect();
            b.add_controller(if app == 0 { "a" } else { "b" }, ctrl("item", decls));
        }
        let mut shuffled: Vec<(u8, &StepOverride)> = overrides
            .iter()
            .enumerate()
            .map(|(i, o)| (s.shuffle[i % s.shuffle.len()], o))
            .collect();
        shuffled.sort_by_key(|(k, o)| (*k, o.target.clone()));
        for (_, o) in shuffled {
            b.add_override("ext", o.clone());
        }
        let r = b.build().map_err(|c| TestCaseError::fail(format!("{c:?}")))?;
        for p in [before_save(), before_create()] {
            let got: Vec<String> = r
                .plan("item", &p)
                .iter()
                .map(|e| match &e.wrapped {
                    Some(w) => format!("{}({})", e.step, w.step),
                    None => e.step.to_string(),
                })
                .collect();
            prop_assert_eq!(got, reference_plan(&s, &overrides, &p));
        }
    }
}
