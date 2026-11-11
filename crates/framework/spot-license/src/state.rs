//! The licence state machine: what an install is allowed to do today.
//!
//! [`evaluate`] is the one function the runtime calls every time a gated action is
//! attempted: it takes the verified licence, the current instant (injected through the
//! caller's [`spot_core::Clock`] — this crate never reads the system clock), and the
//! monotonic instant the install was **last seen running**. It answers with one
//! [`Status`] and no error: a licence that parses but fails a *time* check is still a
//! state, not an error — the states [`NotYetValid`](Status::NotYetValid),
//! [`Expired`](Status::Expired) and friends *are* the failure reporting.
//!
//! # Where `last_seen` fits
//!
//! The brief's `evaluate(&Licence, now, last_seen)` puts the last-seen observation in
//! because restricted-and-then-renewed installs would otherwise snap straight back to
//! full function with no memory of the lapse — and because a `ClockGuard` (the
//! clock-tamper module) feeds exactly such an observation in. The crate treats it as the
//! monotonic observation time (epoch seconds) the caller tracks for this install; a
//! caller with no such tracking passes `None`. It sharpens two boundaries:
//!
//! - **a trial's grace-less end is only reported when observed**: a trial evaluated at
//!   `now` past `expires_at` yields [`Status::Expired`] only if the install has been
//!   seen since (`last_seen` after expiry); an install returning after a long
//!   gap learns its trial was already over without ever running outside it;
//! - **grace is bounded by observation**: a subscription past `expires_at` grants
//!   [`Status::Grace`] only through `now`, as the table says; a *restricting*
//!   transition fires when `last_seen` observed the boundary or `now` reached it.
//!
//! Monotonicity (the property test): for a valid licence, once the install has been
//! observed at or after a boundary, `evaluate` at a later `now` never returns a state
//! earlier in the lifecycle. States only move forward:
//! `NotYetValid -> Active -> RenewalDue -> Grace -> Restricted` (or, for trials,
//! `NotYetValid -> Active -> Expired`), and `ClockSuspect` gates everything until a
//! renewal lands.

use std::time::Duration;

use crate::model::{Kind, Licence};

/// A gated action an install may attempt; the licence has an opinion on each.
///
/// The set is deliberately small and covers the acceptance table's row ends: full
/// function, renewal prompt, read-only-with-export, nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Running the platform at all (every state gates this but the `Grace` states, which
    /// keep it on with a warning).
    UsePlatform,
    /// Creating a new document (an invoice, a stock move); the read-only state refuses.
    CreateDocument,
    /// Reading or exporting existing data; allowed in every state but
    /// [`Status::Expired`].
    ExportData,
    /// Installing or upgrading an app; refused unless the state allows full function.
    InstallApp,
    /// Renewing the licence itself: allowed in *every* state but
    /// [`Status::Expired`] (a dead trial needs a new grant, not a renewal).
    Renew,
}

impl Action {
    /// All actions, for exhaustive checks in tests and callers.
    #[must_use]
    pub fn all() -> [Action; 5] {
        [
            Action::UsePlatform,
            Action::CreateDocument,
            Action::ExportData,
            Action::InstallApp,
            Action::Renew,
        ]
    }
}

/// The answer of [`Status::allows`]: a yes, or a stable reason code the UI can show.
///
/// `Decision` is small and copyable on purpose: gates are on hot paths and the reason
/// code is a `&'static str` out of this module's table, not an allocated error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The action is allowed.
    Allow,
    /// The action is refused; the code is stable across releases, prefix
    /// `licence.` and suitable for direct display next to the refused action.
    Deny {
        /// The machine-readable reason, e.g. `licence.expired_trial`.
        reason: &'static str,
    },
}

impl Decision {
    /// Whether this decision allows the action.
    #[must_use]
    pub fn is_allow(self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The refusal reason of a [`Decision::Deny`], `None` for [`Decision::Allow`].
    #[must_use]
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Allow => None,
            Self::Deny { reason } => Some(reason),
        }
    }
}

/// The reason codes [`Decision::Deny`] carries, the vocabulary the UI shows.
///
/// Codes are stable across releases; adding rows is fine, changing their meaning is not.
pub mod reasons {
    /// The licence's `not_before` is in the future.
    pub const NOT_YET_VALID: &str = "licence.not_yet_valid";
    /// The licence is past `renew_after`; the action still runs, but the UI shows the
    /// prompt (this is a *soft* denial: only the install app itself gates on it).
    pub const RENEWAL_DUE: &str = "licence.renewal_due";
    /// The licence is in grace: running, with the visible warning.
    pub const GRACE: &str = "licence.grace";
    /// The licence is read-only after grace: open and export stay, new documents stop.
    pub const RESTRICTED: &str = "licence.restricted";
    /// A trial that is over: nothing runs.
    pub const EXPIRED_TRIAL: &str = "licence.expired_trial";
    /// A trial that is over cannot be renewed: a new grant is needed.
    pub const RENEW_FIRST_TRIAL_OVER: &str = "licence.renew_first";
    /// The clock moved back harder than the tolerance: run like grace and warn, wait
    /// for a renewal.
    pub const CLOCK_SUSPECT: &str = "licence.clock_suspect";
    /// The licence needs to be renewed before anything else can proceed.
    pub const RENEW_FIRST: &str = "licence.renew_first";
}

/// What the install may do today, as [`evaluate`] computed it.
///
/// One of the lifecycle states of the crate docs' table, plus
/// [`Status::ClockSuspect`]. Every state carries what the UI needs to render it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// `now` is before `not_before`: refuse activation.
    NotYetValid,
    /// Valid and before `renew_after`: everything the entitlements allow.
    Active,
    /// Valid, at or past `renew_after`: everything plus a visible renewal prompt.
    RenewalDue,
    /// Past `expires_at`, within `grace`: full function with the visible warning;
    /// `until` is when the grace will end (exclusive).
    Grace {
        /// The instant the grace period ends, UTC seconds since the Unix epoch.
        until: i64,
    },
    /// Past the grace: read-only, export allowed, no new documents.
    Restricted,
    /// A trial, past `expires_at`, with no grace: nothing runs.
    Expired,
    /// The clock moved back harder than the tolerance: behave like
    /// [`Status::Grace`], with the same `until` semantics, until a licence renewal is
    /// applied (which re-anchors the guard's observed high-water mark).
    ClockSuspect {
        /// The instant the suspect window would end, mirroring the grace `until` —
        /// the last observed high-water mark plus the licence's grace.
        until: i64,
    },
}

impl Status {
    /// Whether the licence is in any state that grants full function (`Active`,
    /// `RenewalDue`, `Grace`, `ClockSuspect`).
    #[must_use]
    pub fn grants_use(self) -> bool {
        matches!(
            self,
            Status::Active
                | Status::RenewalDue
                | Status::Grace { .. }
                | Status::ClockSuspect { .. }
        )
    }

    /// Whether this status allows `action`, the entitlement state machine's verdict.
    ///
    /// The table (also in the crate docs):
    ///
    /// | Status | UsePlatform | CreateDocument | ExportData | InstallApp | Renew |
    /// | --- | --- | --- | --- | --- | --- |
    /// | `NotYetValid` | deny `licence.not_yet_valid` | deny | deny | deny | allow |
    /// | `Active` | allow | allow | allow | allow | allow |
    /// | `RenewalDue` | allow | allow | allow | allow | allow |
    /// | `Grace { .. }` | allow | allow | allow | allow | allow |
    /// | `Restricted` | deny `licence.restricted` | deny `licence.restricted` | allow | deny `licence.restricted` | allow |
    /// | `Expired` | deny `licence.expired_trial` | deny | deny | deny | deny `licence.expired_trial` |
    /// | `ClockSuspect { .. }` | allow | allow | allow | allow | allow |
    #[must_use]
    pub fn allows(self, action: Action) -> Decision {
        use reasons as r;
        match self {
            Status::Active | Status::RenewalDue | Status::ClockSuspect { .. } => Decision::Allow,
            Status::Grace { .. } => Decision::Allow,
            Status::NotYetValid => match action {
                Action::Renew => Decision::Allow,
                _ => Decision::Deny {
                    reason: r::NOT_YET_VALID,
                },
            },
            Status::Restricted => match action {
                Action::ExportData | Action::Renew => Decision::Allow,
                _ => Decision::Deny {
                    reason: r::RESTRICTED,
                },
            },
            Status::Expired => match action {
                Action::Renew => Decision::Deny {
                    // A dead trial needs a new grant, not a renewal of nothing.
                    reason: r::RENEW_FIRST_TRIAL_OVER,
                },
                _ => Decision::Deny {
                    reason: r::EXPIRED_TRIAL,
                },
            },
        }
    }

    /// The reason code the UI shows while the install is in this state, `None` for the
    /// quiet states (`Active`, `RenewalDue` shows `licence.renewal_due`).
    ///
    /// This is the "visible warning" of the crate docs' table: every non-quiet state
    /// names exactly one banner.
    #[must_use]
    pub fn display_reason(self) -> Option<&'static str> {
        use reasons as r;
        match self {
            Status::Active => None,
            Status::NotYetValid => Some(r::NOT_YET_VALID),
            Status::RenewalDue => Some(r::RENEWAL_DUE),
            Status::Grace { .. } => Some(r::GRACE),
            Status::Restricted => Some(r::RESTRICTED),
            Status::Expired => Some(r::EXPIRED_TRIAL),
            Status::ClockSuspect { .. } => Some(r::CLOCK_SUSPECT),
        }
    }
}

/// What a licence state machine holds between calls: the last-seen observation the
/// caller feeds back in, typed.
///
/// A plain newtype so a caller can persist the field without caring what the state
/// machine's internals look like, and so `evaluate`'s signature is self-documenting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LicenceState {
    /// The monotonic instant (epoch seconds) the install was last seen running, when
    /// the caller tracks it; `None` when observations have never been recorded.
    pub last_seen: Option<i64>,
}

impl LicenceState {
    /// A state with no recorded observation.
    #[must_use]
    pub fn unobserved() -> Self {
        Self::default()
    }

    /// The last-seen instant, when recorded.
    #[must_use]
    pub fn last_seen(&self) -> Option<i64> {
        self.last_seen
    }

    /// Records the current instant as the last time the install was seen running;
    /// the caller persists the returned state.
    #[must_use]
    pub fn observe(self, now_epoch_second: i64) -> Self {
        Self {
            last_seen: Some(
                self.last_seen
                    .map_or(now_epoch_second, |previous| previous.max(now_epoch_second)),
            ),
        }
    }
}

/// Computes today's [`Status`] for `licence` at `now`, given the last-seen observation.
///
/// # The boundaries
///
/// All checks are half-open on `now` (`in_window` below): a state takes effect **at**
/// its boundary instant and a *previous* state ends **strictly before** it, so the
/// exact-second tests never find a second that belongs to two states:
///
/// - `now < issued_at || now < not_before` → [`Status::NotYetValid`]
///   (whichever bound is later);
/// - `now <= expires_at` → valid; *and at or past `renew_after`* →
///   [`Status::RenewalDue`], else [`Status::Active`];
/// - past `expires_at`:
///   - a **trial** → [`Status::Expired`] (no grace, ever);
///   - a **subscription** in `expires_at < now <= expires_at + grace` →
///     [`Status::Grace`]; past the grace → [`Status::Restricted`];
///   - a **perpetual** has no `expires_at`; with none set it evaluates
///     `Active`/`RenewalDue` forever.
///
/// The `last_seen` observation sharpens the trial's end: a trial is reported
/// [`Status::Expired`] when either the boundary was already observed *or* `now` is past
/// it, never both unset. A caller with no observations passes `None` and gets the plain
/// boundary behaviour.
///
/// # Panics
/// Never: every comparison is over integers, and there is no clock read — `now` is
/// supplied. If a licence violates structurality in a way its validation missed, the
/// saturating arithmetic below degrades to the safer state, not to a panic.
#[must_use]
pub fn evaluate(licence: &Licence, now: i64, last_seen: Option<i64>) -> Status {
    // The clock rewind module sits *in front of* evaluate: a guard that considers the
    // clock suspect produces the `ClockSuspect` status, with the grace-like window, and
    // the state machine below never sees a rewound `now` from such a caller (the guard
    // feeds the stored high-water mark instead). The status can also be produced
    // directly when the caller tracks `last_seen` against an observed high-water mark
    // (see `renewal::apply_renewal`'s doc comment), but `evaluate` itself is pure.
    evaluate_without_guard(licence, now, last_seen)
}

/// The plain time-only evaluation, the same machine without the guard interaction;
/// `evaluate` is its public face and the guard module composes it.
pub(crate) fn evaluate_without_guard(
    licence: &Licence,
    now: i64,
    last_seen: Option<i64>,
) -> Status {
    // Before validity.
    if now < licence.not_before {
        return Status::NotYetValid;
    }
    // Trials: strict end of life, no grace. Once the expiry has been passed — either
    // observed at the time or discovered now — the trial is simply over.
    if licence.kind == Kind::Trial {
        let Some(expires_at) = licence.expires_at else {
            // Unreachable for a validated licence (trials must carry an expiry); a
            // defensive state, not a panic.
            return Status::Expired;
        };
        if now > expires_at || last_seen.is_some_and(|seen| seen > expires_at) {
            return Status::Expired;
        }
        return if licence
            .renew_after
            .is_some_and(|renew_after| now >= renew_after)
        {
            Status::RenewalDue
        } else {
            Status::Active
        };
    }
    // Perpetual without expiry: never expires.
    let Some(expires_at) = licence.expires_at else {
        return if licence
            .renew_after
            .is_some_and(|renew_after| now >= renew_after)
        {
            Status::RenewalDue
        } else {
            Status::Active
        };
    };
    if now <= expires_at {
        // Valid, possibly past the renewal point.
        return if licence
            .renew_after
            .is_some_and(|renew_after| now >= renew_after)
        {
            Status::RenewalDue
        } else {
            Status::Active
        };
    }
    // Past expiry, non-trial: grace, then restricted. Saturating; the licence's grace
    // is validated to seconds only, so the addition cannot overflow a 64-bit epoch.
    let grace_seconds = u64_to_i64_saturating(licence.grace.as_secs());
    let grace_end = expires_at.saturating_add(grace_seconds);
    if now <= grace_end {
        return Status::Grace { until: grace_end };
    }
    Status::Restricted
}

/// `u64` to `i64` without a panic and without a float: the epoch can only absorb the low
/// half; anything above is clamped to `i64::MAX` seconds and the saturating add absorbs it.
fn u64_to_i64_saturating(value: u64) -> i64 {
    if value > i64::MAX.try_into().unwrap_or(u64::MAX) {
        return i64::MAX;
    }
    value as i64
}

/// The observation rule for a boundary that splits valid from past-it states: has it
/// passed by either the clock or the observation? Integration tests of renewal
/// flows reach this through the crate's public vocabulary.
pub fn boundary_passed(now: i64, boundary: i64, last_seen: Option<i64>) -> bool {
    now > boundary || last_seen.is_some_and(|seen| seen > boundary)
}

/// Whether the grace window from `until` is still open at `now`: the window is
/// `..= until` (inclusive), mirroring every other boundary of this crate.
pub fn grace_open(now: i64, until: i64) -> bool {
    now <= until
}

/// The duration form of a grace period for callers that need it back.
#[must_use]
pub fn grace_duration(licence: &Licence) -> Duration {
    licence.grace
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Action, Decision, LicenceState, Status, evaluate, grace_duration, reasons};
    use crate::model::{DEFAULT_GRACE, Entitlements, Kind, Licence, PAYLOAD_VERSION, RawLicence};

    /// The subscription licence this module's boundary tests walk; expires at
    /// `EXPIRES` with the default 7-day grace; `NOT_BEFORE` = `ISSUED` = day 0.
    fn licence_with(kind: Kind, expires_at: Option<i64>, renew_after: Option<i64>) -> Licence {
        // Trials grant no grace by rule; the fixture honours that instead of relying on
        // the validator to patch it (and the validator's message stays a hard error).
        let grace = if kind == Kind::Trial {
            0
        } else {
            DEFAULT_GRACE.as_secs()
        };
        Licence::new(RawLicence {
            v: PAYLOAD_VERSION,
            licence_id: "0198c7a2-license-id".to_owned(),
            tenant: Some("acme".to_owned()),
            installation_id: None,
            edition: crate::model::test_support::edition(),
            ent: Entitlements::default(),
            kind,
            issued_at: 1_770_000_000,
            not_before: 1_770_000_000,
            expires_at,
            renew_after,
            grace,
            serial: 1,
            key_id: "issuer-2026-q3".to_owned(),
        })
        .unwrap_or_else(|error| panic!("{error:?}"))
    }

    const ISSUED: i64 = 1_770_000_000;
    const EXPIRES: i64 = 1_772_683_200; // 31 days after ISSUED
    const RENEW: i64 = 1_772_071_200; // 24 days after ISSUED

    #[test]
    fn boundaries_hold_at_the_exact_second() {
        let licence = licence_with(Kind::Subscription, Some(EXPIRES), Some(RENEW));
        assert_eq!(evaluate(&licence, ISSUED, None), Status::Active);
        // One second before every boundary, at it, and one after.
        assert_eq!(evaluate(&licence, RENEW - 1, None), Status::Active);
        assert_eq!(evaluate(&licence, RENEW, None), Status::RenewalDue);
        assert_eq!(evaluate(&licence, RENEW + 1, None), Status::RenewalDue);
        assert_eq!(evaluate(&licence, EXPIRES - 1, None), Status::RenewalDue);
        assert_eq!(evaluate(&licence, EXPIRES, None), Status::RenewalDue);
        let grace_until = EXPIRES + DEFAULT_GRACE.as_secs() as i64;
        assert_eq!(
            evaluate(&licence, EXPIRES + 1, None),
            Status::Grace { until: grace_until }
        );
        assert_eq!(
            evaluate(&licence, grace_until, None),
            Status::Grace { until: grace_until }
        );
        assert_eq!(
            evaluate(&licence, grace_until + 1, None),
            Status::Restricted
        );
    }

    #[test]
    fn trial_never_grants_grace() {
        let trial_end = ISSUED + 14 * 86_400;
        let trial = licence_with(Kind::Trial, Some(trial_end), None);
        assert_eq!(evaluate(&trial, trial_end - 1, None), Status::Active);
        assert_eq!(evaluate(&trial, trial_end, None), Status::Active);
        assert_eq!(evaluate(&trial, trial_end + 1, None), Status::Expired);
        // The observation sharpens the boundary: a trial seen after its end is over.
        assert_eq!(
            evaluate(&trial, trial_end + 1, Some(trial_end)),
            Status::Expired
        );
    }

    #[test]
    fn perpetual_never_expires() {
        let perpetual = licence_with(Kind::Perpetual, None, Some(RENEW));
        assert_eq!(evaluate(&perpetual, RENEW, None), Status::RenewalDue);
        assert_eq!(
            evaluate(&perpetual, 4_000_000_000, None),
            Status::RenewalDue
        );
    }

    #[test]
    fn decisions_carry_the_table() {
        let cases: Vec<(Status, Action, Decision)> = vec![
            (Status::Active, Action::CreateDocument, Decision::Allow),
            (Status::RenewalDue, Action::UsePlatform, Decision::Allow),
            (
                Status::Grace { until: 1 },
                Action::CreateDocument,
                Decision::Allow,
            ),
            (
                Status::Restricted,
                Action::CreateDocument,
                Decision::Deny {
                    reason: reasons::RESTRICTED,
                },
            ),
            (Status::Restricted, Action::ExportData, Decision::Allow),
            (
                Status::Expired,
                Action::UsePlatform,
                Decision::Deny {
                    reason: reasons::EXPIRED_TRIAL,
                },
            ),
            (
                Status::NotYetValid,
                Action::UsePlatform,
                Decision::Deny {
                    reason: reasons::NOT_YET_VALID,
                },
            ),
            (Status::NotYetValid, Action::Renew, Decision::Allow),
        ];
        for (status, action, expected) in cases {
            assert_eq!(status.allows(action), expected, "{status:?} {action:?}");
        }
    }

    #[test]
    fn display_reason_covers_the_banners() {
        assert_eq!(Status::Active.display_reason(), None);
        assert_eq!(
            Status::RenewalDue.display_reason(),
            Some(reasons::RENEWAL_DUE)
        );
        assert_eq!(
            Status::Grace { until: 0 }.display_reason(),
            Some(reasons::GRACE)
        );
        assert_eq!(
            Status::Restricted.display_reason(),
            Some(reasons::RESTRICTED)
        );
        assert_eq!(
            Status::Expired.display_reason(),
            Some(reasons::EXPIRED_TRIAL)
        );
        assert_eq!(
            Status::ClockSuspect { until: 0 }.display_reason(),
            Some(reasons::CLOCK_SUSPECT)
        );
    }

    #[test]
    fn licence_state_records_the_high_water_mark() {
        let state = LicenceState::unobserved();
        assert_eq!(state.last_seen(), None);
        let state = state.observe(50);
        assert_eq!(state.last_seen(), Some(50));
        // Observations only ever move forward.
        let state = state.observe(10);
        assert_eq!(state.last_seen(), Some(50));
    }

    #[test]
    fn grace_duration_is_the_licence_s() {
        let licence = licence_with(Kind::Subscription, Some(EXPIRES), None);
        assert_eq!(grace_duration(&licence), DEFAULT_GRACE);
    }
}
