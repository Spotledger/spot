//! Property tests: no sequence of accepted messages gives a lease longer than the latest
//! accepted message allows, and no reboot gives back time.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use proptest::prelude::*;
use spot_offline_identity::*;

#[derive(Clone, Debug)]
enum Op {
    Snapshot {
        bump: u64,
        leases: [u64; 3],
        hub_step: u64,
        mono_step: u64,
        back_dated: u64,
    },
    Timestamp {
        leases: [u64; 3],
        hub_step: u64,
        mono_step: u64,
    },
}

fn lease() -> impl Strategy<Value = u64> {
    prop_oneof![
        Just(FLOOR_MS),
        Just(CEILING_MS),
        FLOOR_MS..=CEILING_MS,
        FLOOR_MS..=48 * HOUR,
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (
            0u64..3,
            [lease(), lease(), lease()],
            0u64..40 * HOUR,
            0u64..40 * HOUR,
            0u64..30 * HOUR
        )
            .prop_map(
                |(bump, leases, hub_step, mono_step, back_dated)| Op::Snapshot {
                    bump,
                    leases,
                    hub_step,
                    mono_step,
                    back_dated
                }
            ),
        (
            [lease(), lease(), lease()],
            0u64..40 * HOUR,
            0u64..40 * HOUR
        )
            .prop_map(|(leases, hub_step, mono_step)| {
                Op::Timestamp {
                    leases,
                    hub_step,
                    mono_step,
                }
            }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn a_lease_never_outlives_the_latest_accepted_message(ops in proptest::collection::vec(op(), 1..14)) {
        let w = World::new();
        let mut store = SnapshotStore::open(w.anchor(), MemoryPersistence::default(), clock(0, 1));
        let (mut hub, mut mono, mut epoch) = (T0, 0u64, 0u64);
        let mut held: Option<(Snapshot, u64, TierDeadlines)> = None; // snapshot, attested, deadlines
        for op in ops {
            let before: [u64; 3] = Tier::ALL.map(|t| store.lease_remaining_ms(t, clock(mono, 1)));
            let result = match op {
                Op::Snapshot { bump, leases, hub_step, mono_step, back_dated } => {
                    hub += hub_step;
                    mono += mono_step;
                    let candidate = epoch + bump; // bump 0 re-sends the held epoch
                    let issued = hub.saturating_sub(back_dated);
                    let snap = snapshot_at(candidate.max(1), issued, leases);
                    let res = store.accept(&w.signed(snap.clone()), rx(mono, 1));
                    if res.is_ok() {
                        epoch = snap.epoch;
                        held = Some((snap.clone(), issued, snap.valid_until));
                    }
                    res
                }
                Op::Timestamp { leases, hub_step, mono_step } => {
                    hub += hub_step;
                    mono += mono_step;
                    let Some((snap, _, _)) = held.clone() else { continue };
                    let msg = w.timestamp(&snap, hub, leases);
                    let res = store.accept(&msg, rx(mono, 1));
                    if res.is_ok() {
                        held = Some((snap, hub, TierDeadlines(leases.map(|l| hub + l))));
                    }
                    res
                }
            };
            let after: [u64; 3] = Tier::ALL.map(|t| store.lease_remaining_ms(t, clock(mono, 1)));
            match result {
                Ok(accepted) => {
                    let (_, attested, deadlines) = held.clone().unwrap();
                    for t in Tier::ALL {
                        let cap = deadlines.get(t).saturating_sub(attested);
                        prop_assert!(accepted.remaining_ms[t.index()] <= cap);
                        prop_assert!(accepted.remaining_ms[t.index()] <= CEILING_MS);
                        prop_assert_eq!(after[t.index()], accepted.remaining_ms[t.index()]);
                    }
                }
                Err(_) => {
                    // A rejection changes nothing; time only runs down.
                    for t in Tier::ALL {
                        prop_assert!(after[t.index()] <= before[t.index()]);
                    }
                }
            }
            // At every moment, every tier's remaining time is within what the latest
            // accepted message granted.
            if let Some((_, attested, deadlines)) = &held {
                for t in Tier::ALL {
                    prop_assert!(after[t.index()] <= deadlines.get(t).saturating_sub(*attested));
                }
            }
        }
    }

    #[test]
    fn no_reboot_sequence_gives_time_back(
        checkpoint_at in 0u64..2 * HOUR,
        boots in proptest::collection::vec((0u64..100 * HOUR, 0u64..2 * HOUR), 1..6),
    ) {
        let w = World::new();
        let mut store = w.store_with(snapshot(1));
        store.checkpoint(clock(checkpoint_at, 1)).unwrap();
        let mut ceiling: [u64; 3] = Tier::ALL.map(|t| store.lease_remaining_ms(t, clock(checkpoint_at, 1)));
        let mut persisted = store.into_persistence();
        for (index, (start, run)) in boots.into_iter().enumerate() {
            let boot = 2 + index as u64;
            let mut s = SnapshotStore::open(w.anchor(), persisted, clock(start, boot));
            for t in Tier::ALL {
                let got = s.lease_remaining_ms(t, clock(start, boot));
                prop_assert!(got <= ceiling[t.index()], "tier {:?} grew: {} > {}", t, got, ceiling[t.index()]);
                ceiling[t.index()] = got;
            }
            s.checkpoint(clock(start + run, boot)).unwrap();
            for t in Tier::ALL {
                ceiling[t.index()] = ceiling[t.index()].min(s.lease_remaining_ms(t, clock(start + run, boot)));
            }
            persisted = s.into_persistence();
        }
    }
}
