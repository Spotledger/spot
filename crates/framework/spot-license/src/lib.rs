//! Spotledger licences and entitlements (blueprint WS18, WP-CTL-04.T1).
//!
//! A licence is a small, canonical, versioned JSON payload signed with Ed25519. A machine
//! verifies it **offline**: the only inputs are the licence file, the trusted issuer keys and
//! the time, which the caller injects through [`spot_core::Clock`] — this crate never reads
//! the system clock. On top of the verified licence sits a pure state machine that decides,
//! for `now` and the last time the install was seen, what the install may do today, and a
//! renewal path that swaps in a newer signed payload under the same licence id.
//!
//! # Pieces
//!
//! - [`Licence`](model::Licence): the canonical payload — editions, entitlements, kind,
//!   validity window, the renewal point, the grace period, the per-licence-id `serial` and
//!   the issuer key id. `Edition` is raw data (`Edition(String)`); nothing here interprets
//!   which edition means which feature.
//! - [`SignedLicence`](envelope::SignedLicence) + [`verify`](envelope::verify): the signed
//!   envelope and offline verification against [`TrustedKeys`](envelope::TrustedKeys), a list
//!   of issuer public keys with `valid_from`/`valid_to` windows so the issuer can rotate.
//! - [`evaluate`](state::evaluate) + [`Status`](state::Status) + [`Action`](state::Action):
//!   the entitlement state machine, whose decision carries a stable reason code the UI can
//!   show (`licence.not_yet_valid`, `licence.renewal_due`, `licence.restricted`, ...).
//! - [`ClockGuard`](guard::ClockGuard): offline clock-tamper resistance. It keeps the highest
//!   time ever observed, persisted by the caller through [`guard::Store`]; a clock that moved
//!   back by more than the tolerance yields [`Status::ClockSuspect`](state::Status::ClockSuspect),
//!   which behaves like `Grace` until a licence renewal is applied.
//! - [`apply_renewal`](renewal::apply_renewal): swaps in a newer licence of the same id and
//!   tenant; an older or equal `serial` is [`licence.stale`](error::RenewalError::Stale), an
//!   unrelated tenant is [`licence.wrong_tenant`](error::RenewalError::WrongTenant).
//!
//! # The lifecycle, in one table
//!
//! | Status | Meaning | What the install may do |
//! | --- | --- | --- |
//! | [`NotYetValid`](state::Status::NotYetValid) | `now` is before `not_before` (or before `issued_at` for a licence without one) | Refuse activation; show `licence.not_yet_valid` |
//! | [`Active`](state::Status::Active) | valid, before `renew_after` | Everything the entitlements allow |
//! | [`RenewalDue`](state::Status::RenewalDue) | valid, at or past `renew_after` | Everything, plus a visible renewal prompt (`licence.renewal_due`) |
//! | [`Grace { until }`](state::Status::Grace) | past `expires_at` but within `grace` | Full function, visible warning (`licence.grace`) |
//! | [`Restricted`](state::Status::Restricted) | past `not_before` + lifetime + grace; a subscription/perpetual that expired | Read-only: open, export; no new documents (`licence.restricted`) |
//! | [`Expired`](state::Status::Expired) | past `expires_at` + grace; **trials only** | Nothing; the trial ended (`licence.expired_trial`) |
//! | [`ClockSuspect { .. }`](state::Status::ClockSuspect) | the clock moved back harder than [`DEFAULT_CLOCK_TOLERANCE`](guard::DEFAULT_CLOCK_TOLERANCE) | Behaves like `Grace` (full function + warning, `licence.clock_suspect`) |
//!
//! Every boundary checks `now` inclusively: a state that starts at an instant is in force
//! **at** that instant, one that ends at an instant is over **at** that instant, so an
//! install never sees a second of unowned time between states.
//!
//! # Errors
//!
//! Every check returns [`LicenceError`](error::LicenceError) or
//! [`RenewalError`](error::RenewalError) with a stable machine-readable code
//! ([`LicenceError::code`](error::LicenceError::code)), a plain-language message and no echo
//! of untrusted input.
//!
//! # What this crate is not
//!
//! No network, no storage, no user model: nothing here fetches licences, persists them, or
//! decides what a feature costs. Callers persist the licence file, the highest observed
//! clock reading ([`guard::Store`]) and the last-seen instant, all under their own IO. The
//! issuing helper lives behind the `issue` cargo feature (used by tests and later by the
//! control plane, never compiled into the product binary); keys are always passed in, never
//! generated or stored here.

#![forbid(unsafe_code)]

pub mod envelope;
pub mod error;
pub mod guard;
#[cfg(feature = "issue")]
pub mod issuing;
pub mod model;
pub mod renewal;
pub mod state;

pub use envelope::{SignedLicence, TrustedKey, TrustedKeys, verify};
pub use error::{LicenceError, RenewalError};
pub use guard::{ClockGuard, DEFAULT_CLOCK_TOLERANCE, Observation, Store};
pub use model::{DEFAULT_GRACE, Entitlements, Kind, Licence, Limit, MAX_GRACE};
pub use renewal::apply_renewal;
pub use state::{Action, Decision, LicenceState, Status, evaluate};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-license";
