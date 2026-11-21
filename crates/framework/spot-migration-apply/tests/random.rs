//! Five hundred random plans: whatever the shape, the end state is always "new version,
//! verified, open" or "old version, restored, open" — never open unverified.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_migration_apply::testing::{StepConfig, sample_plan};
use spot_migration_apply::{AdmissionGate, ApplyState, Idempotency, KillPoint, Version};
use spot_migration_plan::Area;
use support::{Harness, step_ids};

/// A tiny deterministic PRNG, so the five hundred cases are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[tokio::test]
async fn five_hundred_random_plans_never_open_unverified() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for case in 0..500u64 {
        let step_count = rng.below(6);
        let areas: Vec<Area> = (0..step_count)
            .map(|_| match rng.below(4) {
                0 => Area::Schema,
                1 => Area::Metadata,
                2 => Area::Patch,
                _ => Area::Behaviour,
            })
            .collect();
        let plan = sample_plan(&areas);
        let total = plan.steps().len();

        let mut configs = Vec::with_capacity(total);
        for _ in 0..total {
            let transactional = rng.below(2) == 0;
            let idempotency = match rng.below(3) {
                0 => Some(Idempotency::Idempotent),
                1 => Some(Idempotency::AtMostOnce),
                _ => None,
            };
            configs.push(StepConfig {
                transactional,
                idempotency,
                timeout: None,
                fail: rng.below(8) == 0,
                kill: rng.below(6) == 0,
            });
        }

        let harness = Harness::new(move |mut runner| {
            for (index, config) in configs.iter().enumerate() {
                runner = runner.configure(index, config.clone());
            }
            runner
        });
        harness.verify.set_pass(rng.below(4) != 0);
        let snapshot = harness.take().await;

        let point = match rng.below(10) {
            0 => Some(KillPoint::BeforeVerify),
            1 => Some(KillPoint::AfterVerify),
            2 => Some(KillPoint::BeforeRestore),
            3 => Some(KillPoint::AfterRestore),
            4 => Some(KillPoint::BeforeJournal(1 + rng.below(20))),
            5 => Some(KillPoint::AfterJournal(1 + rng.below(20))),
            6 if total > 0 => Some(KillPoint::BeforeStep(
                usize::try_from(rng.below(total as u64)).unwrap(),
            )),
            7 if total > 0 => Some(KillPoint::AfterStep(
                usize::try_from(rng.below(total as u64)).unwrap(),
            )),
            _ => None,
        };

        let engine = match &point {
            Some(point) => harness.engine_killing_at(point.clone()),
            None => harness.engine(),
        };
        let result = engine.apply(harness.request(&plan, snapshot, 1_000)).await;
        let state = match result {
            Ok(_) => engine.state(),
            Err(error) if error.is_killed() => {
                if support::gate_must_be_closed(&harness) {
                    assert!(
                        !harness.gate.status().is_open(),
                        "case {case}: a killed run left the gate open"
                    );
                }
                // A step may crash again while the restart resumes; keep recovering until the
                // run reaches an end state (the fake's kill fires once).
                let mut state = None;
                for _ in 0..6 {
                    match harness.engine().recover(&plan, 2_000).await {
                        Ok(recovery) => {
                            state = Some(recovery.state);
                            break;
                        }
                        Err(error) if error.is_killed() => continue,
                        Err(error) => panic!("case {case}: recover failed: {error}"),
                    }
                }
                state.unwrap_or_else(|| panic!("case {case}: recovery never finished"))
            }
            Err(error) => panic!("case {case}: unexpected error {error}"),
        };

        assert!(
            harness.gate.status().is_open(),
            "case {case}: the gate is closed at the end"
        );
        match state {
            ApplyState::Open {
                version: Version::New,
                verified,
            } => {
                assert!(verified, "case {case}: a new version was opened unverified");
                assert_eq!(
                    harness.site.applied(),
                    step_ids(&plan),
                    "case {case}: the new version is not fully applied"
                );
            }
            ApplyState::Open {
                version: Version::Old,
                ..
            } => {
                assert!(
                    harness.site.applied().is_empty(),
                    "case {case}: the old version is not the snapshot"
                );
            }
            ApplyState::Idle => {}
            other => panic!("case {case}: unexpected end state {other:?}"),
        }
    }
}
