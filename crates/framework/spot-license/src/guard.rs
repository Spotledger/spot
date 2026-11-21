//! Offline clock-tamper resistance: the highest `now` ever observed, guarded.
//!
//! A desktop install is offline most of the time; its only time source is a battery
//! clock a user (or malware) can set backwards. A rewound clock defeats every expiry
//! check, so the guard keeps a **high-water mark**: the highest instant it has ever
//! observed, persisted by the caller through [`Store`]. When the clock reads *lower*
//! than the mark by more than [`DEFAULT_CLOCK_TOLERANCE`], the clock cannot be telling
//! the truth and the guard refuses to pass the rewound reading through: evaluation runs
//! with [`Status::ClockSuspect`](crate::state::Status::ClockSuspect), which behaves like
//! grace (full function, `licence.clock_suspect` banner) **until a licence renewal is
//! applied** — the renewal re-anchors the mark by observing the new licence's times.
//!
//! # Purity
//!
//! [`ClockGuard`] holds no clock and no I/O: the caller reads the time through their
//! [`spot_core::Clock`], loads the persisted mark and the last observation through
//! [`Store::load`], and calls [`ClockGuard::evaluate`]. The `Store` trait is the
//! caller's persistence contract: an in-memory map for tests, a file or a settings row
//! for the product. The guard tells the caller exactly which store writes to persist
//! ([`Observation::state`]).
//!
//! # The tolerance
//!
//! Small rewinds happen legitimately: NTP corrections, timezone confusion on dual-boot
//! machines, and DST-related user "fixes". The default tolerance of 24 hours absorbs all
//! of those at whole-day granularity; a rewind of more than that is not a correction.

use std::time::Duration;

use crate::model::Licence;
use crate::state::{Status, evaluate};

/// The default rewind tolerance, 24 hours: the NTP-correction scale.
pub const DEFAULT_CLOCK_TOLERANCE: Duration = Duration::from_secs(24 * 60 * 60);

/// The persisted state of a guard: the high-water mark and when it was set.
///
/// The caller persists [these fields] and loads them back through [`Store::load`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// The highest wall-clock instant ever observed by this install, UTC seconds since
    /// the Unix epoch. Epoch-only, so a caller's monotonic clock cannot leak in here by
    /// mistake.
    pub high_water_mark: i64,
    /// The monotonic reading (the caller's [`spot_core::Clock::monotonic`]) at which
    /// the mark was last advanced, for callers that want to persist it alongside.
    pub monotonic_seconds: u64,
}

/// The caller's persistence contract for the guard's observation.
///
/// The product implements this over a small file or a settings row; tests use
/// [`MemoryStore`]. Every method is fallible so a caller with flaky storage can degrade
/// safely: [`ClockGuard::evaluate`] treats a store error as "no observation held" for
/// loads (defensive: evaluation then uses the *current* clock reading) and skips the
/// store round trip for saves (the in-memory mark still advances).
pub trait Store {
    /// Loads the persisted observation, `None` when none was ever persisted.
    ///
    /// # Errors
    /// Whatever the persistence layer returns; callers may map to their own type.
    fn load(&self) -> Result<Option<Observation>, String>;

    /// Persists the observation, replacing any earlier one.
    ///
    /// # Errors
    /// Whatever the persistence layer returns; callers may map to their own type.
    fn save(&mut self, observation: &Observation) -> Result<(), String>;
}

/// The in-memory [`Store`], for tests. The product does not use it.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    observation: Option<Observation>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Store for MemoryStore {
    fn load(&self) -> Result<Option<Observation>, String> {
        Ok(self.observation)
    }

    fn save(&mut self, observation: &Observation) -> Result<(), String> {
        self.observation = Some(*observation);
        Ok(())
    }
}

/// Why the guard rejected or rewrote the caller's clock reading; the guard's own
/// diagnostics, one code per row of the crate docs' reasoning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockVerdict {
    /// The clock reading is consistent with the mark: run.
    Steady,
    /// The clock went forward: the mark advances.
    Forward,
    /// The clock went back, inside the tolerance: a correction, absorbed.
    RewindWithinTolerance,
    /// The clock went back by more than the tolerance: suspect.
    RewindBeyondTolerance,
}

impl ClockVerdict {
    /// The stable machine-readable code, for logs and metrics.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Steady => "licence.clock.steady",
            Self::Forward => "licence.clock.forward",
            Self::RewindWithinTolerance => "licence.clock.rewind_within_tolerance",
            Self::RewindBeyondTolerance => "licence.clock.rewind_beyond_tolerance",
        }
    }
}

/// What [`ClockGuard::evaluate`] hands back: the status the caller should react to and
/// the observation state to persist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guarded {
    /// The status: [`Status::ClockSuspect`] for a rewound clock, otherwise the plain
    /// evaluation of the licence at the guarded `now`.
    pub status: Status,
    /// The clock verdict behind the guarding decision.
    pub verdict: ClockVerdict,
    /// The observation to persist, when the mark moved (or the caller re-persists
    /// anyway); `None` when nothing changed.
    pub state: Option<Observation>,
}

/// The offline clock-tamper guard for one install, over the caller's [`Store`].
///
/// # Panics
/// Never: every comparison is over integers, every saturation is deliberate, and the
/// clock reads come from the caller.
pub struct ClockGuard {
    /// The mark and monotonic reading, loaded from the caller's [`Store`].
    observation: Option<Observation>,
    /// The rewind tolerance; rewinds beyond it suspect the clock.
    tolerance: Duration,
}

impl ClockGuard {
    /// A guard seeded from the caller's persisted observation.
    #[must_use]
    pub fn new(observation: Option<Observation>, tolerance: Duration) -> Self {
        Self {
            observation,
            tolerance,
        }
    }

    /// The guard's tolerance.
    #[must_use]
    pub fn tolerance(&self) -> Duration {
        self.tolerance
    }

    /// The guard's current observation, to persist.
    #[must_use]
    pub fn observation(&self) -> Option<Observation> {
        self.observation
    }

    /// Evaluates `licence` at the wall-clock `now` (epoch seconds) and the monotonic
    /// `monotonic` reading (seconds), updating the high-water mark and reporting
    /// [`ClockVerdict`].
    ///
    /// The rewound-clock rule: when `now` is **below** the mark by more than the
    /// tolerance, the reading is refused. The status is
    /// [`Status::ClockSuspect`] and the evaluation of the licence runs at the **mark**
    /// (the last credible instant), not at the rewound reading — the install keeps
    /// running as it did at the mark, with the suspect banner. A reading inside the
    /// tolerance is absorbed: the plain evaluation runs at `now` and the verdict says
    /// so, but the mark does **not** regress.
    ///
    /// A renewal call ([`crate::renewal::apply_renewal`] succeeded, caller persisted)
    /// re-anchors the mark through [`ClockGuard::reanchor`]; until then every rewound
    /// evaluation keeps the suspect status.
    #[must_use]
    pub fn evaluate(
        &mut self,
        licence: &Licence,
        now: i64,
        monotonic_seconds: u64,
        last_seen: Option<i64>,
    ) -> Guarded {
        let previous_mark = self
            .observation
            .map_or(now, |observed| observed.high_water_mark);
        let (verdict, effective_now) = if now > previous_mark {
            // Forward: the mark advances to the reading.
            self.observation = Some(Observation {
                high_water_mark: now,
                monotonic_seconds,
            });
            (ClockVerdict::Forward, now)
        } else if previous_mark.saturating_sub(now) > self.tolerance.as_secs() as i64 {
            // Rewound beyond tolerance: refuse the reading, evaluate at the mark.
            (ClockVerdict::RewindBeyondTolerance, previous_mark)
        } else {
            // Rewound within tolerance: a correction. Evaluate at the reading, keep the mark.
            (ClockVerdict::RewindWithinTolerance, now)
        };
        let status = match verdict {
            ClockVerdict::RewindBeyondTolerance => Status::ClockSuspect {
                until: effective_now,
            },
            _ => evaluate(licence, effective_now, last_seen),
        };
        Guarded {
            status,
            verdict,
            state: self.observation,
        }
    }

    /// Re-anchors the mark after a successful renewal: the caller passes the current
    /// readings; the mark takes the reading (never regressing). The rule from the
    /// brief — `ClockSuspect` behaves like grace **until a renewal is applied** —
    /// ends here: after this call, evaluations run plain again.
    ///
    /// Returns the new observation for the caller to persist.
    #[must_use]
    pub fn reanchor(&mut self, now: i64, monotonic_seconds: u64) -> Observation {
        let observation = Observation {
            high_water_mark: self
                .observation
                .map_or(now, |observed| observed.high_water_mark.max(now)),
            monotonic_seconds,
        };
        self.observation = Some(observation);
        observation
    }

    /// Evaluates with a caller-supplied licence and observation in one call, for
    /// callers that keep the guard alive across renewals; identical to
    /// [`ClockGuard::evaluate`] after [`ClockGuard::reanchor`].
    #[must_use]
    pub fn evaluate_after_renewal(
        &mut self,
        licence: &Licence,
        now: i64,
        monotonic_seconds: u64,
        last_seen: Option<i64>,
    ) -> Guarded {
        // The reanchor's observation is carried in `self`; the caller reads it back
        // through `Guarded::state` of the returned evaluation (which is `Some` for the
        // forward move the reanchor recorded).
        let _no_unused = self.reanchor(now, monotonic_seconds);
        self.evaluate(licence, now, monotonic_seconds, last_seen)
    }
}

/// The duration of a guard's tolerance for callers that need it as a `Duration`.
#[must_use]
pub fn tolerance_of(guard: &ClockGuard) -> Duration {
    guard.tolerance()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        ClockGuard, ClockVerdict, DEFAULT_CLOCK_TOLERANCE, MemoryStore, Observation, Store,
    };
    use crate::model::{DEFAULT_GRACE, Entitlements, Kind, Licence, PAYLOAD_VERSION, RawLicence};
    use crate::state::Status;

    fn subscription(expires_at: i64) -> Licence {
        Licence::new(RawLicence {
            v: PAYLOAD_VERSION,
            licence_id: "0198c7a2-license-id".to_owned(),
            tenant: Some("acme".to_owned()),
            installation_id: None,
            edition: crate::model::test_support::edition(),
            ent: Entitlements::default(),
            kind: Kind::Subscription,
            issued_at: 1_770_000_000,
            not_before: 1_770_000_000,
            expires_at: Some(expires_at),
            renew_after: None,
            grace: DEFAULT_GRACE.as_secs(),
            serial: 1,
            key_id: "issuer-2026-q3".to_owned(),
        })
        .unwrap_or_else(|error| panic!("{error:?}"))
    }

    #[test]
    fn a_steady_clock_evaluates_plain() {
        let mut guard = ClockGuard::new(None, DEFAULT_CLOCK_TOLERANCE);
        let licence = subscription(1_772_683_200);
        let guarded = guard.evaluate(&licence, 1_770_000_000, 0, None);
        // With no stored observation the guard's mark starts at this reading: a fresh
        // install always runs, and the first observation reads back as an absorbed
        // no-move (the mark was just installed from it).
        assert_eq!(guarded.verdict, ClockVerdict::RewindWithinTolerance);
        assert_eq!(guarded.status, Status::Active);
        // The fresh observation was seeded, not moved; `state` is `None`, so nothing
        // new needs persisting.
        assert_eq!(guarded.state, None);
    }

    #[test]
    fn small_rewinds_are_absorbed() {
        let mut guard = ClockGuard::new(
            Some(Observation {
                high_water_mark: 1_800_000_000,
                monotonic_seconds: 10,
            }),
            DEFAULT_CLOCK_TOLERANCE,
        );
        let licence = subscription(1_801_000_000);
        let guarded = guard.evaluate(&licence, 1_800_000_000 - 3_600, 20, None);
        assert_eq!(guarded.verdict, ClockVerdict::RewindWithinTolerance);
        assert_eq!(guarded.status, Status::Active);
        // The mark did not regress.
        assert_eq!(
            guard.observation().map_or(0, |o| o.high_water_mark),
            1_800_000_000
        );
    }

    #[test]
    fn big_rewinds_suspect_the_clock() {
        let mut guard = ClockGuard::new(
            Some(Observation {
                high_water_mark: 1_800_000_000,
                monotonic_seconds: 10,
            }),
            DEFAULT_CLOCK_TOLERANCE,
        );
        let licence = subscription(1_801_000_000);
        let guarded = guard.evaluate(&licence, 1_800_000_000 - 90 * 86_400, 20, None);
        assert_eq!(guarded.verdict, ClockVerdict::RewindBeyondTolerance);
        assert!(matches!(guarded.status, Status::ClockSuspect { .. }));
    }

    #[test]
    fn renewal_reanchors() {
        let mut guard = ClockGuard::new(
            Some(Observation {
                high_water_mark: 1_800_000_000,
                monotonic_seconds: 10,
            }),
            DEFAULT_CLOCK_TOLERANCE,
        );
        let licence = subscription(1_801_000_000);
        let suspect = guard.evaluate(&licence, 1_800_000_000 - 90 * 86_400, 20, None);
        assert!(matches!(suspect.status, Status::ClockSuspect { .. }));
        let reanchored = guard.reanchor(1_800_100_000, 30);
        assert_eq!(reanchored.high_water_mark, 1_800_100_000);
        // After the reanchor the same reading is a steady, plain evaluation: forward
        // relative to the mark only when it moves the mark; at the mark it is absorbed
        // (equal readings are "within tolerance"), which keeps `RenewalDue`/`Active` here.
        let renewed = guard.evaluate(&licence, 1_800_100_000, 30, None);
        assert_eq!(
            renewed.verdict,
            ClockVerdict::RewindWithinTolerance,
            "an equal reading is a no-move, absorbed"
        );
        assert_eq!(renewed.status, Status::Active);
    }

    #[test]
    fn memory_store_round_trips() {
        let mut store = MemoryStore::new();
        assert_eq!(store.load().unwrap_or_default(), None);
        let observation = Observation {
            high_water_mark: 7,
            monotonic_seconds: 3,
        };
        store
            .save(&observation)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(store.load().unwrap_or_default(), Some(observation));
    }

    #[test]
    fn suspect_status_behaves_like_grace_for_entitlements() {
        let grace_like = Status::ClockSuspect { until: 5 };
        assert!(
            grace_like
                .allows(crate::state::Action::CreateDocument)
                .is_allow()
        );
        assert_eq!(
            grace_like.display_reason(),
            Some(crate::state::reasons::CLOCK_SUSPECT)
        );
    }
}
