//! Random chains and random refusal points always end fully undone or in a consistent decision.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::collections::BTreeSet;

use common::*;
use spot_app_api::prelude::Decimal;
use spot_bpm_effects::{NoGraph, UndoContext, UndoOutcome};

/// A small deterministic generator, so a failing seed reproduces.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn random_chains_and_refusal_points_end_undone_or_consistent() {
    for seed in 0..400u64 {
        let mut rng = Lcg(seed + 1);
        let rig_app = FakeApp::new();
        let svc = service();
        let partner = Partner::new(&rig_app);
        let req = case("prop");
        let len = 1 + rng.below(8);
        let mut links_of: Vec<Vec<u64>> = Vec::new();
        for i in 0..len {
            let record = rec(i + 1);
            // Each record depends on up to two earlier ones.
            let mut links = Vec::new();
            if i > 0 {
                for _ in 0..rng.below(3) {
                    let l = rng.below(i);
                    if !links.contains(&l) {
                        links.push(l);
                    }
                }
            }
            // The fake app refuses a cancel while a live record depends on it, through one link.
            rig_app.post(
                &record,
                i64::try_from(10 + i).unwrap(),
                links.first().map(|l| rec(l + 1)).as_ref(),
            );
            let link_refs: Vec<_> = links.iter().map(|l| rec(l + 1)).collect();
            svc.record(
                &req,
                new_entry(
                    &format!("n{i}"),
                    &record,
                    &link_refs.iter().collect::<Vec<_>>(),
                ),
            )
            .unwrap();
            links_of.push(links);
        }
        // Some records refuse their reversal (a closed period, a type rule).
        let mut refused = BTreeSet::new();
        for i in 0..len {
            if rng.below(4) == 0 {
                refused.insert(rec(i + 1));
            }
        }
        rig_app.state.lock().unwrap().refuse = refused.clone();
        let ids: Vec<u64> = (1..=len).collect();
        let before = probe(&rig_app, &ids);

        let plan = svc.plan_undo(&req, None, &NoGraph).unwrap();
        // Reverse dependency order holds in the plan.
        for (i, links) in links_of.iter().enumerate() {
            let pos = |n: u64| {
                plan.steps()
                    .iter()
                    .position(|s| s.record == rec(n + 1))
                    .unwrap()
            };
            for l in links {
                assert!(
                    pos(u64::try_from(i).unwrap()) < pos(*l),
                    "seed {seed}: {i} must go before {l}"
                );
            }
        }
        let ctx = UndoContext {
            principal: "u",
            now: NOW,
            port: rig_app.as_ref(),
            partner: &partner,
        };
        let out = svc.execute_undo(&plan, &ctx).unwrap();
        let after = probe(&rig_app, &ids);
        for (b, a) in before.iter().zip(&after) {
            assert!(a.1 >= b.1, "seed {seed}: counts only grow");
        }
        let file = svc.file(&req).unwrap();
        let reversed_in_file: BTreeSet<_> = file
            .entries()
            .iter()
            .filter_map(|e| {
                e.reverses.map(|r| {
                    file.entries()[usize::try_from(r.entry).unwrap()]
                        .record
                        .clone()
                })
            })
            .collect();
        match out {
            UndoOutcome::Done { reversed, .. } => {
                assert!(
                    refused.is_empty(),
                    "seed {seed}: a refusing record cannot be undone"
                );
                assert_eq!(reversed, usize::try_from(len).unwrap());
                for a in &after {
                    assert_eq!(
                        a.0,
                        Decimal::from(0),
                        "seed {seed}: fully undone nets to zero"
                    );
                }
                assert!(!file.is_locked());
            }
            UndoOutcome::NeedsDecision(d) => {
                let blocked = d.blockers[0].record.clone();
                assert!(file.is_locked(), "seed {seed}");
                assert_eq!(file.pending_decision(), Some(&d));
                // The file and the stores agree: reversed records net to zero, the others do not.
                for (i, a) in after.iter().enumerate() {
                    let r = rec(u64::try_from(i).unwrap() + 1);
                    if reversed_in_file.contains(&r) {
                        assert_eq!(
                            a.0,
                            Decimal::from(0),
                            "seed {seed}: {i} reversed in file and store"
                        );
                    } else {
                        assert_ne!(
                            a.0,
                            Decimal::from(0),
                            "seed {seed}: {i} unreversed stays posted"
                        );
                    }
                }
                assert!(!reversed_in_file.contains(&blocked), "seed {seed}");
                // Everything that depends on a reversed record was reversed first.
                for (i, links) in links_of.iter().enumerate() {
                    for l in links {
                        if reversed_in_file.contains(&rec(l + 1)) {
                            assert!(
                                reversed_in_file.contains(&rec(u64::try_from(i).unwrap() + 1)),
                                "seed {seed}: dependant {i} of {l}"
                            );
                        }
                    }
                }
            }
        }
    }
}
