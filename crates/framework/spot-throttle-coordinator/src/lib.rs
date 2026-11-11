//! The coordinator-backed sign-in throttle (WP-IAM-11.T7, ADR-083 task 7).
//!
//! `spot-sessions::throttle::LoginThrottle` keeps its counters in one process, evicts locked
//! names when its unknown-name map is full, and on the native API once bucketed every
//! caller together because the source was empty. A public listener cannot rely on
//! that. [`CoordinatorThrottle`] is a small crate any front door (staff, partner, consumer)
//! calls with one function, [`CoordinatorThrottle::admit`]; its counters live on the
//! coordinator, so every node shares them and a restart does not reset them.
//!
//! # Four dimensions
//!
//! Each attempt is charged one unit against each bucket that applies, in this fixed order:
//!
//! 1. **source** (always): one peer address, or one connection when the address is empty;
//! 2. **destination** (when the attempt names one): the e-mail or phone a code would go to,
//!    shared by every kind, so one-time-code pumping to a victim or a premium number is capped;
//! 3. **account** (when the attempt names one): one account name **per kind** of attempt;
//! 4. **tenant** (always): a ceiling that protects the database.
//!
//! The first bucket that denies ends the call with [`Verdict::Deny`]. The coordinator has no
//! refund, so buckets charged before the denying one stay charged and the later ones are never
//! touched. That is why the order runs from what an attacker controls (source) to what is
//! shared: a flood from one source is refused at step 1 and never reaches anyone's account or
//! destination bucket. The cost of the order is that an attempt refused by the account bucket
//! has already been counted against its own source and destination, which only ever hurts the
//! caller that is being refused.
//!
//! # Rotation: a salt ring, never a bare salt swap
//!
//! Every bucket is keyed by a [`ThrottleSalt`]. Swapping that salt on its own would start every
//! counter afresh at once, so an attacker who knows the rotation schedule would get a free reset
//! on every account and source he had been failing for. A rotation therefore goes through a
//! [`SaltRing`] (WP-IAM-11.T12):
//!
//! - [`SaltRing::rotate`] moves the current salt to `previous` and the new one to `current`;
//! - while a previous salt is present, [`CoordinatorThrottle::admit`] checks the previous salt's
//!   buckets as well — a denial from either salt is a denial — so **every limit that was
//!   exhausted at rotation time stays exhausted until it refills on its own**;
//! - [`SaltRing::safe_to_retire`] says when that has happened (one full window of the longest
//!   bucket, from the two instants the caller passes in), and [`SaltRing::retire_previous`]
//!   drops the previous salt then. Rotating twice drops the older previous salt.
//!
//! The previous window is checked with a real `take` of one unit, not with a read: the
//! coordinator has no uncharged read of a rate-limit bucket (`Coordinator::peek` reads its
//! *counter* namespace, which is a different store from `take`'s buckets). A take that is
//! denied changes nothing, so a denial from the previous window costs nothing; a take that is
//! allowed spends the unit, which makes the overlap stricter than a pure read and never looser.
//! It also runs *before* the current salt's buckets, so an attempt the old window refuses cannot
//! pre-spend the fresh allowance of the account it targets.
//!
//! # Where the salt comes from
//!
//! [`DerivedSalt`] implements [`SaltSource`] over a [`spot_secrets::KeyProvider`]: the salt of a
//! tenant at a version is an HKDF-SHA256 expansion of the provider's master key, labelled
//! [`SALT_INFO`] and bound to the tenant and the version. No raw salt is stored anywhere, two
//! tenants never share one, and a node rebuilds the same ring after a restart.
//! [`load_ring`] loads both salts of an overlap. A source that cannot answer is
//! `Err(SaltUnavailable)` and the caller refuses (fail closed).
//!
//! # What this does and does not do
//!
//! - **No lock from source evidence.** Only the account bucket can deny an account name, and it
//!   is a *rate*, not a lock: it refills continuously. A flood from one source is capped by the
//!   source bucket and cannot charge the account bucket of anyone it was refused for. The
//!   trade-off, as in `LoginThrottle`: an attacker with many sources can still spend a
//!   victim's account bucket (`per_account` attempts per window) and so slow that name's
//!   sign-in for the window. The account bucket is per kind so a recovery or code flood cannot
//!   do the same to sign-in.
//! - **No enumeration oracle.** Known and unknown account names go down the same code path at
//!   the same cost and produce the same verdicts and the same key shape. There is no
//!   in-process map to fill or evict, which closes a recorded security finding for every caller that adopts the
//!   crate. The caller must pass the name as typed whether or not it exists.
//! - **Fail closed.** A coordinator error is `Err(Unavailable)` and a salt the source cannot
//!   supply is `Err(SaltUnavailable)`; the throttle never answers `Allow` after either. Callers
//!   refuse the attempt.
//! - **No raw value outside the process.** A bucket key is `thr:{tenant}:{dimension}:{digest}`
//!   where the digest is HMAC-SHA-256 under a ring salt. Account names and destinations are
//!   trimmed and lower-cased before hashing, so case variants share a bucket. No error, `Debug`
//!   output or key carries a raw value, and no salt or derived key is ever written by `Debug`.
//! - **Empty source.** An empty address is bucketed per connection id when the caller
//!   has one and refused when not; it is never one bucket shared by every caller.
//! - **Time** is the coordinator's: this crate reads no clock. [`SaltRing::safe_to_retire`]
//!   takes both instants as arguments, so the rotation policy stays with the caller.
//! - Not here: adopting the crate in `spot-login` or the consumer login service, the rotation
//!   schedule, and the one-time-code path itself (follow-ups).

mod attempt;
mod error;
mod policy;
mod ring;
mod source;
mod throttle;

pub use attempt::{Attempt, Kind, MAX_TENANT_BYTES, MAX_VALUE_BYTES, SourceAddr, Verdict};
pub use error::{
    BAD_ATTEMPT, BAD_POLICY, BAD_ROTATION, RATE_LIMITED, SALT_UNAVAILABLE, ThrottleError,
    UNAVAILABLE,
};
pub use policy::{MIN_SALT_BYTES, ThrottlePolicy, ThrottleSalt};
pub use ring::SaltRing;
pub use source::{DerivedSalt, SALT_INFO, SaltFuture, SaltSource, load_ring};
pub use throttle::CoordinatorThrottle;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-throttle-coordinator";
