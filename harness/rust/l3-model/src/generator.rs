//! The seeded sequence generator.
//!
//! A sequence is a pure function of its seed. The weights favour the interesting corners: a call
//! on a deleted document, a stale version, the same call twice, post then cancel then correct, a
//! correct of a correct, delete of a referred document then of the referrer, an update with an
//! unchanged value.

use spot_l7_invariants::schedule::Rng;

use crate::model::{Call, Ver, Who};

/// The Time Entry slots a sequence uses.
pub const TE_SLOTS: u8 = 6;
/// The Task slots a sequence uses.
pub const TASK_SLOTS: u8 = 3;

/// How long a sequence is.
#[derive(Clone, Copy, Debug)]
pub struct GenConfig {
    /// The fewest calls.
    pub min_calls: u64,
    /// The most calls.
    pub max_calls: u64,
}

impl Default for GenConfig {
    fn default() -> Self {
        GenConfig {
            min_calls: 20,
            max_calls: 200,
        }
    }
}

fn ver(rng: &mut Rng) -> Ver {
    if rng.below(8) == 0 {
        Ver::Stale
    } else {
        Ver::Current
    }
}

fn who(rng: &mut Rng) -> Who {
    if rng.below(8) == 0 {
        Who::Viewer
    } else {
        Who::Manager
    }
}

/// The sequence of `seed`.
pub fn generate(seed: u64, config: GenConfig) -> Vec<Call> {
    let mut rng = Rng::new(seed ^ 0x9E37_79B9_7F4A_7C15);
    let span = config.max_calls.saturating_sub(config.min_calls).max(1);
    let len = config.min_calls + rng.below(span + 1);
    // Two Tasks and two Time Entries first, so most calls have something to act on.
    let mut calls = vec![
        Call::CreateTask { slot: 0 },
        Call::CreateTask { slot: 1 },
        Call::CreateTe { slot: 0, task: 0 },
        Call::CreateTe { slot: 1, task: 1 },
    ];
    let mut next_te: u8 = 2;
    let mut next_task: u8 = 2;
    let mut tag: u16 = 0;
    while (calls.len() as u64) < len {
        let te = u8::try_from(rng.below(u64::from(TE_SLOTS))).unwrap_or(0);
        let task = u8::try_from(rng.below(u64::from(TASK_SLOTS))).unwrap_or(0);
        let call = match rng.below(100) {
            0..=9 if next_te < TE_SLOTS => {
                let slot = next_te;
                next_te += 1;
                Call::CreateTe { slot, task }
            }
            10..=13 if next_task < TASK_SLOTS => {
                let slot = next_task;
                next_task += 1;
                Call::CreateTask { slot }
            }
            14..=29 => Call::Post {
                te,
                ver: ver(&mut rng),
                who: who(&mut rng),
            },
            30..=41 => Call::Cancel {
                te,
                ver: ver(&mut rng),
                who: who(&mut rng),
            },
            42..=53 => {
                let into = u8::try_from(rng.below(u64::from(TE_SLOTS))).unwrap_or(0);
                Call::Correct {
                    te,
                    into,
                    ver: ver(&mut rng),
                    who: who(&mut rng),
                }
            }
            54..=67 => Call::SetHours {
                te,
                // Often the value the document already has (an unchanged patch).
                hours: if rng.below(3) == 0 {
                    2
                } else {
                    u8::try_from(1 + rng.below(8)).unwrap_or(1)
                },
                ver: ver(&mut rng),
                who: who(&mut rng),
            },
            68..=76 => Call::DeleteTe {
                te,
                ver: ver(&mut rng),
            },
            77..=84 => {
                tag += 1;
                Call::SetSubject {
                    task,
                    tag,
                    ver: ver(&mut rng),
                    who: who(&mut rng),
                }
            }
            85..=90 => {
                tag += 1;
                Call::RenameTask {
                    task,
                    tag,
                    ver: ver(&mut rng),
                    who: who(&mut rng),
                }
            }
            91..=95 => Call::DeleteTask {
                task,
                ver: ver(&mut rng),
            },
            // The same call twice.
            _ => calls.last().copied().unwrap_or(Call::Post {
                te,
                ver: Ver::Current,
                who: Who::Manager,
            }),
        };
        calls.push(call);
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_sequence_and_another_seed_another() {
        let a = generate(7, GenConfig::default());
        assert_eq!(a, generate(7, GenConfig::default()));
        assert_ne!(a, generate(8, GenConfig::default()));
    }

    #[test]
    fn the_length_is_within_the_bounds_and_slots_stay_in_range() {
        for seed in 0..50 {
            let calls = generate(seed, GenConfig::default());
            assert!(
                (20..=200).contains(&(calls.len() as u64)),
                "{}",
                calls.len()
            );
            for c in &calls {
                if let Call::CreateTe { slot, task } = c {
                    assert!(*slot < TE_SLOTS && *task < TASK_SLOTS);
                }
            }
        }
    }
}
