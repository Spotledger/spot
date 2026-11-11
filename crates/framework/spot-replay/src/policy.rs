//! The sample policy: which recent operations a replay re-executes.
//!
//! The sample is drawn from the journal over a **window** — the last N days before a reference
//! instant, or the last N documents — and **stratified** by `(DocType, sensitivity)`, so a replay
//! covers every high-sensitivity DocType even when an ordinary one dominates the journal. The
//! selection is **deterministic given a seed**: two replays with the same policy, reference
//! instant and seed draw the same sample, so a divergence can always be reproduced.
//!
//! The policy is a per-site setting (blueprint WS12): a routine update may sample little, a site
//! where failure is not an option samples everything.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;

use crate::error::{Error, Result};
use crate::journal::{JournalEntry, Sensitivity};

/// The window a sample is drawn from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    /// The last `days` days before the reference instant.
    LastDays {
        /// How many days back the window reaches. At least one.
        days: u32,
    },
    /// The most recent `documents` operations, whatever their age.
    LastDocuments {
        /// How many operations the window holds. At least one.
        documents: u32,
    },
}

/// The per-site sample policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPolicy {
    /// The window.
    pub window: Window,
    /// The seed the stratified selection is drawn with.
    pub seed: u64,
    /// The most operations drawn from any one `(DocType, sensitivity)` stratum. At least one.
    pub per_stratum: u32,
}

impl Default for ReplayPolicy {
    /// The last 30 days, seeded at zero, at most 250 operations per stratum.
    fn default() -> Self {
        ReplayPolicy {
            window: Window::LastDays { days: 30 },
            seed: 0,
            per_stratum: 250,
        }
    }
}

impl ReplayPolicy {
    /// A policy over `window`, seeded at zero.
    #[must_use]
    pub fn new(window: Window, per_stratum: u32) -> Self {
        ReplayPolicy {
            window,
            seed: 0,
            per_stratum,
        }
    }

    /// The same policy with another seed.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Refuses a policy that cannot draw a sample.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when the window or the per-stratum cap is zero.
    pub fn validate(&self) -> Result<()> {
        match self.window {
            Window::LastDays { days: 0 } => Err(Error::InvalidInput {
                what: "sample policy".to_owned(),
                reason: "the window must span at least one day".to_owned(),
            }),
            Window::LastDocuments { documents: 0 } => Err(Error::InvalidInput {
                what: "sample policy".to_owned(),
                reason: "the window must hold at least one document".to_owned(),
            }),
            _ if self.per_stratum == 0 => Err(Error::InvalidInput {
                what: "sample policy".to_owned(),
                reason: "the per-stratum cap must be at least one".to_owned(),
            }),
            _ => Ok(()),
        }
    }
}

/// What one `(DocType, sensitivity)` stratum contributed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stratum {
    /// The DocType.
    pub doctype: String,
    /// Its sensitivity.
    pub sensitivity: Sensitivity,
    /// How many candidates the window held for this stratum.
    pub available: usize,
    /// How many were selected.
    pub selected: usize,
}

/// The sample a policy drew, and the strata it covered.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The selected entries, in causal order `(source, sequence)`.
    pub entries: Vec<JournalEntry>,
    /// The strata, keyed by `(DocType, sensitivity)`.
    pub strata: Vec<Stratum>,
    /// How many candidates the window considered before stratification.
    pub considered: usize,
}

impl Sample {
    /// How many entries were selected.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the sample is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Draws the sample `policy` names from `candidates` against `reference_at`.
///
/// The selection is deterministic given the policy (including its seed) and the candidate set:
/// the same inputs always draw the same sample.
#[must_use]
pub fn select(
    policy: &ReplayPolicy,
    reference_at: OffsetDateTime,
    candidates: Vec<JournalEntry>,
) -> Sample {
    let considered = candidates.len();
    let windowed = apply_window(policy.window, reference_at, candidates);

    let mut strata: BTreeMap<(String, Sensitivity), Vec<JournalEntry>> = BTreeMap::new();
    for entry in windowed {
        strata
            .entry((entry.doctype.clone(), entry.sensitivity))
            .or_default()
            .push(entry);
    }

    let mut selected: Vec<JournalEntry> = Vec::new();
    let mut summary: Vec<Stratum> = Vec::new();
    for ((doctype, sensitivity), mut members) in strata {
        let available = members.len();
        let mut rng = Rng::seeded(stratum_seed(policy.seed, &doctype, sensitivity));
        // Assign a random rank to each candidate, then take the lowest ranks. Deterministic for a
        // seed, and uniform within the stratum rather than "the newest N", which would bias the
        // sample towards one shape of operation.
        let mut ranked: Vec<(u64, JournalEntry)> =
            members.drain(..).map(|e| (rng.next(), e)).collect();
        ranked.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.order_key().cmp(&b.1.order_key()))
        });
        ranked.truncate(policy.per_stratum as usize);
        let selected_here = ranked.len();
        for (_, entry) in ranked {
            selected.push(entry);
        }
        summary.push(Stratum {
            doctype,
            sensitivity,
            available,
            selected: selected_here,
        });
    }

    selected.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
    Sample {
        entries: selected,
        strata: summary,
        considered,
    }
}

/// Keeps the candidates the window holds.
fn apply_window(
    window: Window,
    reference_at: OffsetDateTime,
    candidates: Vec<JournalEntry>,
) -> Vec<JournalEntry> {
    match window {
        Window::LastDays { days } => {
            // Days are exactly 86_400 seconds each; no calendar arithmetic, no `time` dependency.
            let seconds = reference_at
                .unix_timestamp()
                .saturating_sub(i64::from(days) * 86_400);
            let cutoff =
                OffsetDateTime::from_unix_timestamp(seconds).unwrap_or(OffsetDateTime::UNIX_EPOCH);
            candidates
                .into_iter()
                .filter(|e| e.occurred_at >= cutoff)
                .collect()
        }
        Window::LastDocuments { documents } => {
            let mut recent = candidates;
            recent.sort_by(|a, b| {
                b.occurred_at
                    .cmp(&a.occurred_at)
                    .then_with(|| a.order_key().cmp(&b.order_key()))
            });
            recent.truncate(documents as usize);
            recent
        }
    }
}

/// The seed of one stratum: the policy seed mixed with the stratum's key, so the strata are
/// independent yet each is reproducible.
fn stratum_seed(seed: u64, doctype: &str, sensitivity: Sensitivity) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64; // FNV-1a offset basis.
    for byte in doctype.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash ^= match sensitivity {
        Sensitivity::Normal => 0,
        Sensitivity::High => 0x9E37_79B9_7F4A_7C15,
    };
    mix(seed ^ hash)
}

/// A splitmix64 step: a small, fast, deterministic mixing function. No external dependency.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A splitmix64 stream, seeded once per stratum.
struct Rng {
    state: u64,
}

impl Rng {
    fn seeded(seed: u64) -> Self {
        Rng { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.state)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_window_is_refused() {
        let policy = ReplayPolicy::new(Window::LastDays { days: 0 }, 1);
        assert!(policy.validate().is_err());
        let policy = ReplayPolicy::new(Window::LastDocuments { documents: 0 }, 1);
        assert!(policy.validate().is_err());
        let policy = ReplayPolicy::new(Window::LastDays { days: 1 }, 0);
        assert!(policy.validate().is_err());
        assert!(ReplayPolicy::default().validate().is_ok());
    }

    #[test]
    fn the_same_seed_draws_the_same_ranks() {
        let mut a = Rng::seeded(42);
        let mut b = Rng::seeded(42);
        for _ in 0..16 {
            assert_eq!(a.next(), b.next());
        }
        assert_ne!(Rng::seeded(42).next(), Rng::seeded(43).next());
    }

    #[test]
    fn stratum_seeds_differ_by_doctype_and_sensitivity() {
        let base = stratum_seed(7, "invoice", Sensitivity::Normal);
        assert_ne!(base, stratum_seed(7, "payment", Sensitivity::Normal));
        assert_ne!(base, stratum_seed(7, "invoice", Sensitivity::High));
        assert_eq!(base, stratum_seed(7, "invoice", Sensitivity::Normal));
    }
}
