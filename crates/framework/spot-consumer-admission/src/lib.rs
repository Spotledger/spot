//! The admission rules the consumer front door runs **before** it touches a password, a code or
//! a user table (WP-IAM-11.T14, ADR-083 tasks 9 and 14).
//!
//! [`CoordinatorThrottle`] digests whatever string it is given. That is the right thing for a
//! throttle and the wrong thing for a front door on its own: `Alice@Example.com`,
//! `alice@example.com ` and `alice+x@example.com` are three strings, so they are three
//! destination buckets, and one victim's inbox can be pumped with codes from three spellings of
//! the same address. This crate sits in front of the throttle and is the single entry point both
//! the consumer login service and the one-time-code path call:
//!
//! - it **canonicalises** the destination ([`canonicalise`]) so case, surrounding whitespace and
//!   a `+tag` suffix land in one bucket;
//! - it **prices** an attempt by purpose ([`Purpose`], [`AdmissionPolicy`]): a code request costs
//!   the destination more than a sign-in does, because a code is a message someone pays for and
//!   a victim receives;
//! - it enforces a **resend cooldown** per canonical destination, so a second code inside the
//!   window is refused even when every rate bucket would still allow it;
//! - it never hands the raw destination on: [`Admission`] carries the canonical form and every
//!   `Debug` in the crate is redacted.
//!
//! # What the caller does
//!
//! ```ignore
//! let admission = ConsumerAdmission::with_coordinator(
//!     Arc::new(CoordinatorThrottle::with_ring(coordinator.clone(), ring, throttle_policy)),
//!     coordinator,
//!     AdmissionPolicy::consumer_default(),
//! )?;
//! let request = Request {
//!     purpose: Purpose::SendCode,
//!     tenant: "acme",
//!     account: None,
//!     destination: Some(Destination::Email(as_typed)),
//!     source: SourceAddr::new(peer),
//! };
//! let admission = admission.admit(&request).await.map_err(|error| error.code())?;
//! // `admission.destination()` is the canonical address the code goes to.
//! ```
//!
//! # The account spelling
//!
//! Admit with the **raw** account, then look the user up with [`Admission::account`]: it is
//! [`fold_account`] of the raw name (trimmed, Unicode lower-cased, the staff login's operation),
//! the very spelling the throttle counted. Looking up any other spelling lets the count and the
//! lookup drift apart. No other normalisation is applied or promised.
//!
//! # Fail closed
//!
//! A coordinator that cannot count, a salt the source cannot supply, a malformed request and an
//! unreadable policy are all errors ([`AdmissionError`]). The crate never answers `Allow` when it
//! could not count: `Err` means refuse.
//!
//! # No enumeration oracle
//!
//! Nothing here looks at a user table. A destination that has an account and one that has none
//! take the same path, charge the same buckets and produce the same [`Admission`] or the same
//! error: the crate cannot tell them apart because it never asks.
//!
//! # What is never written
//!
//! No raw destination, account name or source address reaches a bucket key (the throttle digests
//! them), an error message, or a `Debug`. [`CanonicalDestination`] redacts itself, and
//! [`Admission`] redacts the destination it carries.
//!
//! # Time
//!
//! Nothing here reads a clock: the resend cooldown is a bucket on the coordinator, whose time is
//! the store's, so a restart does not hand anyone a fresh cooldown and two nodes agree.

mod account;
mod admission;
mod destination;
mod error;
mod policy;

pub use account::fold_account;
pub use admission::{Admission, ConsumerAdmission, Request};
pub use destination::{CanonicalDestination, Destination, DestinationKind, canonicalise};
pub use error::{
    AdmissionError, BAD_DESTINATION, BAD_POLICY, BAD_REQUEST, RATE_LIMITED, UNAVAILABLE,
};
pub use policy::{AdmissionPolicy, Purpose};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-consumer-admission";
