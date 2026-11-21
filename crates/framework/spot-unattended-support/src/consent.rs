//! Consent for crash reporting and telemetry: two separate switches, each `off`, `ask` or
//! `on`, read from settings and defaulting to `ask` at first run.
//!
//! What each value allows:
//!
//! | value | spool a redacted report locally | upload |
//! | --- | --- | --- |
//! | `off` | no | never |
//! | `ask` | crash reports only, held for review | only a report the user approved |
//! | `on` | yes | yes |
//!
//! Telemetry has no review queue, so for telemetry `ask` behaves as `off` until the user
//! answers. Nothing is ever enabled by default: the default is `ask`, which sends nothing.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::error::{SupportError, codes};

/// The settings keys the host reads the two switches from.
pub const CRASH_SETTING: &str = "support_crash_reporting";
/// The settings key for telemetry.
pub const TELEMETRY_SETTING: &str = "support_telemetry";

/// One consent value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Consent {
    /// Never.
    Off,
    /// Not yet decided: nothing leaves the machine.
    #[default]
    Ask,
    /// Allowed.
    On,
}

impl Consent {
    /// Parses a settings value (`off`, `ask`, `on`, any case).
    ///
    /// # Errors
    /// `consent_invalid` for anything else.
    pub fn parse(text: &str) -> Result<Self, SupportError> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "ask" => Ok(Self::Ask),
            "on" => Ok(Self::On),
            _ => Err(SupportError::new(
                codes::CONSENT_INVALID,
                "the consent value must be off, ask or on",
            )),
        }
    }

    /// The settings spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ask => "ask",
            Self::On => "on",
        }
    }
}

/// Which switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConsentKind {
    /// Crash reporting.
    Crash,
    /// Telemetry.
    Telemetry,
}

/// A recorded change of a switch, kept so the user can see when they decided.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentChange {
    /// Which switch.
    pub kind: ConsentKind,
    /// The value before.
    pub from: Consent,
    /// The value after.
    pub to: Consent,
    /// When, in seconds since the Unix epoch.
    pub at: u64,
}

/// The two switches and the history of their changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentState {
    crash: Consent,
    telemetry: Consent,
    history: Vec<ConsentChange>,
}

impl ConsentState {
    /// The state at first run: both switches `ask`, nothing allowed.
    #[must_use]
    pub fn first_run() -> Self {
        Self::default()
    }

    /// The state from the two settings values; a missing value keeps the first-run default and
    /// an unparsable one is an error rather than a silent `on`.
    ///
    /// # Errors
    /// `consent_invalid` when a present value does not parse.
    pub fn from_settings(
        crash: Option<&str>,
        telemetry: Option<&str>,
    ) -> Result<Self, SupportError> {
        Ok(Self {
            crash: crash.map(Consent::parse).transpose()?.unwrap_or_default(),
            telemetry: telemetry
                .map(Consent::parse)
                .transpose()?
                .unwrap_or_default(),
            history: Vec::new(),
        })
    }

    /// The current value of a switch.
    #[must_use]
    pub fn get(&self, kind: ConsentKind) -> Consent {
        match kind {
            ConsentKind::Crash => self.crash,
            ConsentKind::Telemetry => self.telemetry,
        }
    }

    /// Sets a switch at `now`. A change to the same value records nothing.
    pub fn set(&mut self, kind: ConsentKind, to: Consent, now: u64) -> Option<ConsentChange> {
        let from = self.get(kind);
        if from == to {
            return None;
        }
        match kind {
            ConsentKind::Crash => self.crash = to,
            ConsentKind::Telemetry => self.telemetry = to,
        }
        let change = ConsentChange {
            kind,
            from,
            to,
            at: now,
        };
        self.history.push(change.clone());
        Some(change)
    }

    /// Answers an `ask`: `allow` makes the switch `on`, otherwise `off`. Does nothing when the
    /// switch is not at `ask`.
    pub fn answer(&mut self, kind: ConsentKind, allow: bool, now: u64) -> Option<ConsentChange> {
        if self.get(kind) != Consent::Ask {
            return None;
        }
        self.set(kind, if allow { Consent::On } else { Consent::Off }, now)
    }

    /// Whether a crash report may be written to the local spool.
    #[must_use]
    pub fn may_spool_crash(&self) -> bool {
        self.crash != Consent::Off
    }

    /// Whether spooled crash reports may be uploaded without asking about each one.
    #[must_use]
    pub fn may_upload_crash(&self) -> bool {
        self.crash == Consent::On
    }

    /// Whether the user has to approve each crash report before it is uploaded.
    #[must_use]
    pub fn must_ask_per_report(&self) -> bool {
        self.crash == Consent::Ask
    }

    /// Whether telemetry may be collected and uploaded.
    #[must_use]
    pub fn may_send_telemetry(&self) -> bool {
        self.telemetry == Consent::On
    }

    /// Every recorded change, oldest first.
    #[must_use]
    pub fn history(&self) -> &[ConsentChange] {
        &self.history
    }
}

/// The encoding of a [`Consent`] in a [`ConsentStore`]'s lock-free copy. A byte that is none of
/// these reads as `off`: privacy fails closed.
const OFF: u8 = 0;
const ASK: u8 = 1;
const ON: u8 = 2;

const fn encode(value: Consent) -> u8 {
    match value {
        Consent::Off => OFF,
        Consent::Ask => ASK,
        Consent::On => ON,
    }
}

const fn decode(byte: u8) -> Consent {
    match byte {
        ON => Consent::On,
        ASK => Consent::Ask,
        _ => Consent::Off,
    }
}

/// The consent state behind a lock, with a lock-free copy of the crash switch beside it.
///
/// The crash switch is read by the panic hook, which must never wait for a lock: a panic can
/// happen on the very thread that holds one, and a crash handler that blocks is no crash
/// handler. Reading it from the lock alone meant a busy lock read as the first-run default
/// (`ask`) and spooled a report the user had turned off. The copy is refreshed
/// whenever the state is read or changed through this type, and it is initialised from the
/// state given to [`ConsentStore::new`] at start-up, so a busy lock can only ever cost the
/// last few microseconds of a decided change, never the user's `off`.
///
/// Share it as an `Arc`; it is the object the host hands to
/// [`install_panic_hook`](crate::crash::install_panic_hook).
#[derive(Debug)]
pub struct ConsentStore {
    state: Mutex<ConsentState>,
    crash: AtomicU8,
}

impl ConsentStore {
    /// A store for `state`, with the lock-free copy taken from it. Call this with the state
    /// read at start-up (`ConsentState::from_settings`), so the copy is right before anything
    /// reads the switches.
    #[must_use]
    pub fn new(state: ConsentState) -> Self {
        Self {
            crash: AtomicU8::new(encode(state.get(ConsentKind::Crash))),
            state: Mutex::new(state),
        }
    }

    /// A store at first run: both switches `ask`.
    #[must_use]
    pub fn first_run() -> Self {
        Self::new(ConsentState::first_run())
    }

    /// The lock, recovering a poisoned one: a switch the user set before the panic is still
    /// the switch they set.
    fn guard(&self) -> MutexGuard<'_, ConsentState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Copies the crash switch into the lock-free copy.
    fn refresh(&self, state: &ConsentState) {
        self.crash
            .store(encode(state.get(ConsentKind::Crash)), Ordering::Relaxed);
    }

    /// The whole state, copied.
    #[must_use]
    pub fn state(&self) -> ConsentState {
        let state = self.guard();
        self.refresh(&state);
        state.clone()
    }

    /// The last known state, copied **without taking the lock**, so a crash handler can read
    /// it. Only the crash switch is carried: telemetry reads as the first-run default and the
    /// history is empty, so a caller of this must not depend on either.
    #[must_use]
    pub fn last_known(&self) -> ConsentState {
        ConsentState {
            crash: decode(self.crash.load(Ordering::Relaxed)),
            telemetry: Consent::Ask,
            history: Vec::new(),
        }
    }

    /// Whether a crash report may be spooled, decided from the lock-free copy alone.
    #[must_use]
    pub fn may_spool_crash_now(&self) -> bool {
        decode(self.crash.load(Ordering::Relaxed)) != Consent::Off
    }

    /// Runs `f` on the state under the lock, then refreshes the lock-free copy. Every change
    /// goes through here, so the copy never lags a change made through this type.
    pub fn with_state<R>(&self, f: impl FnOnce(&mut ConsentState) -> R) -> R {
        let mut state = self.guard();
        let out = f(&mut state);
        self.refresh(&state);
        out
    }

    /// Sets a switch at `now`; see [`ConsentState::set`].
    pub fn set(&self, kind: ConsentKind, to: Consent, now: u64) -> Option<ConsentChange> {
        self.with_state(|state| state.set(kind, to, now))
    }

    /// Answers an `ask`; see [`ConsentState::answer`].
    pub fn answer(&self, kind: ConsentKind, allow: bool, now: u64) -> Option<ConsentChange> {
        self.with_state(|state| state.answer(kind, allow, now))
    }
}
