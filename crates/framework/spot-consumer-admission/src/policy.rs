//! What the front door is counting, and what it charges for it.

use std::time::Duration;

use spot_throttle_coordinator::Kind;

use crate::error::AdmissionError;

/// The longest resend cooldown accepted, in seconds: a day.
const MAX_COOLDOWN_SECONDS: u64 = 86_400;

/// The front-door action an attempt is for.
///
/// The purpose decides which buckets are charged and how much; it is mapped onto the throttle's
/// [`Kind`] so a recovery flood cannot deny a name's sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// A password (or other credential) check: charges the account bucket and the source bucket.
    SignIn,
    /// A new registration: charges the source bucket and the destination bucket.
    Register,
    /// A one-time code: charges the destination bucket at `code_cost` units and the source
    /// bucket, and must pass the resend cooldown.
    SendCode,
    /// An account recovery: charges the account, destination, source and tenant buckets.
    Recover,
}

impl Purpose {
    /// The throttle kind this purpose is counted as (per-kind account buckets).
    pub(crate) fn kind(self) -> Kind {
        match self {
            Self::SignIn => Kind::SignIn,
            Self::Register => Kind::Register,
            Self::SendCode => Kind::CodeRequest,
            Self::Recover => Kind::Recovery,
        }
    }

    /// Whether the purpose counts the account bucket (a registration counts the destination
    /// instead, so an account flood cannot deny anybody's password check).
    pub(crate) fn charges_account(self) -> bool {
        matches!(self, Self::SignIn | Self::Recover)
    }

    /// The stable label, for logs that must not carry a destination.
    pub fn label(self) -> &'static str {
        match self {
            Self::SignIn => "sign_in",
            Self::Register => "register",
            Self::SendCode => "send_code",
            Self::Recover => "recover",
        }
    }
}

/// The admission rules that are **not** the throttle's own limits.
///
/// [`crate::ConsumerAdmission`] adds three things to the throttle: how a destination is
/// canonicalised, how many throttle units a code request costs the destination, and how long a
/// destination must wait between codes. The rate itself stays the throttle's
/// [`spot_throttle_coordinator::ThrottlePolicy`]; a preset here only prices the attempt against it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionPolicy {
    strip_plus_tags: bool,
    code_cost: u32,
    resend_cooldown: Option<Duration>,
}

impl AdmissionPolicy {
    /// A policy from its three parts, validated.
    ///
    /// `code_cost` must be at least 1 (a zero-cost attempt would never be counted) and
    /// `resend_cooldown` either `None` or at least one millisecond and at most a day.
    pub fn new(
        strip_plus_tags: bool,
        code_cost: u32,
        resend_cooldown: Option<Duration>,
    ) -> Result<Self, AdmissionError> {
        if code_cost == 0 {
            return Err(AdmissionError::BadPolicy {
                reason: "a code request must cost the destination at least one unit",
            });
        }
        if let Some(cooldown) = resend_cooldown {
            if cooldown.is_zero() {
                return Err(AdmissionError::BadPolicy {
                    reason: "a resend cooldown of zero would never refuse anything",
                });
            }
            if cooldown > Duration::from_secs(MAX_COOLDOWN_SECONDS) {
                return Err(AdmissionError::BadPolicy {
                    reason: "a resend cooldown is at most one day",
                });
            }
        }
        Ok(Self {
            strip_plus_tags,
            code_cost,
            resend_cooldown,
        })
    }

    /// The preset a consumer front door ships with: a `+tag` is the same mailbox, a code request
    /// costs the destination **three** units, and a destination may receive **one code a minute**
    /// however many rate units the throttle's per-destination bucket holds.
    ///
    /// Plain words, against the reference throttle limits (15 per destination per hour, 20 per
    /// source per hour): a destination can receive **five codes an hour** (15 units / 3), one
    /// every minute at most, and a single source can start **six** code requests an hour before
    /// its own bucket refuses.
    pub fn consumer_default() -> Self {
        Self {
            strip_plus_tags: true,
            code_cost: 3,
            resend_cooldown: Some(Duration::from_secs(60)),
        }
    }

    /// The careful preset: a `+tag` is the same mailbox, a code request costs the destination
    /// **five** units, and a destination may receive **one code every five minutes**.
    ///
    /// Plain words, against the same reference limits: **three codes an hour** per destination
    /// (15 units / 5), never closer together than five minutes.
    pub fn strict() -> Self {
        Self {
            strip_plus_tags: true,
            code_cost: 5,
            resend_cooldown: Some(Duration::from_secs(300)),
        }
    }

    /// Whether a `+tag` suffix is stripped from the local part before counting.
    pub fn strip_plus_tags(&self) -> bool {
        self.strip_plus_tags
    }

    /// How many throttle units a code request costs the destination bucket (1 or more).
    pub fn code_cost(&self) -> u32 {
        self.code_cost
    }

    /// How long one canonical destination must wait between codes, when armed.
    pub fn resend_cooldown(&self) -> Option<Duration> {
        self.resend_cooldown
    }
}

impl Default for AdmissionPolicy {
    /// [`AdmissionPolicy::consumer_default`].
    fn default() -> Self {
        Self::consumer_default()
    }
}
