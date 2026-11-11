//! Sessions and login throttling for the Spotledger Platform (WP-PLT-02.T2, ADR-070 §5).
//!
//! A **session** is the thing a signed-in person lives under: it carries who ([`UserKey`]),
//! which token verifies it (only as a [`TokenDigest`] — the raw token never reaches this
//! crate's storage), from where (a salted [`SourceHash`], never a raw address), a bounded
//! [`ClientInfo`] label, and two deadlines the user cannot outlive: idle and absolute
//! (ADR-070 §5.4: idle and absolute timeouts are tenant policy; [`SessionPolicy`] holds the
//! defaults). [`SessionManager`] starts, validates, ends, ends-others and lists sessions;
//! its errors are stable `session.*` codes that never tell *why* a token failed beyond one
//! `session.invalid` (an internal reason rides along only for audit, never to the caller).
//!
//! [`LoginThrottle`] is a pure state machine over a [`ThrottleStore`]: repeated failed
//! sign-ins back off exponentially per account and per source, a hard lockout follows M
//! failures, and every state change hands the caller an [`AuditNote`] to journal as an
//! identity event (ADR-070 §8.4: sign-in success and failure, lockout and unlock are
//! audited; credentials, tokens and secrets never appear in an event). The check runs
//! *before* the password hash so its timing cannot leak password validity.
//!
//! # Shape
//!
//! - [`model`]: [`Session`], [`SessionState`], [`SessionPolicy`], [`UserKey`],
//!   [`ClientInfo`], [`SourceHash`], [`SaltKey`] and the derived status rules.
//! - [`store`]: the object-safe [`SessionStore`] contract (boxed futures, so a
//!   database-backed store can implement the same trait) and the digest-only proof.
//! - [`memory`]: [`InMemorySessionStore`], the reference fake for tests and single-node hosts.
//! - [`manager`]: [`SessionManager`] and [`SessionError`].
//! - [`contract`]: [`run_all`](contract::run_all), the store suite, behind feature
//!   `testing` — WP-PLT-02.T3's storage-backed store runs the very same code.
//! - [`throttle`]: [`LoginThrottle`], [`ThrottleStore`], [`ThrottleConfig`],
//!   [`AuditNote`], [`ThrottleDecision`], [`ThrottleAction`] and the in-memory fake.
//! - [`error`]: [`SessionError`], the one error type, with stable codes.
//!
//! # The time convention (the boundary choice, stated once)
//!
//! Time is always an argument: neither the crate nor any store reads a clock. Windows are
//! **half-open** exactly as `spot-scheduler`'s leases and `spot-support-session`'s grants
//! treat them: a session covers `[created, absolute_expires)` — at `absolute_expires` it is
//! already expired — and its idle window covers `(last_seen, last_seen + idle]` so a
//! session touched exactly `idle` after its last use is still alive. Purging and expiry
//! checks use the same convention everywhere, which makes the boundary tests exact.
//!
//! # Purity
//!
//! No clock, no sleep, no threads, no floats, no `unsafe` (workspace lints forbid them).
//! Secret material — token texts and digests — never appears in `Debug` output
//! ([`TokenDigest`]'s Debug is already redacted) and never in an [`AuditNote`].

#![forbid(unsafe_code)]

mod crypto;

pub mod error;
pub mod manager;
pub mod memory;
pub mod model;
pub mod store;
pub mod throttle;

/// Deterministic digests and salts for tests and the contract suite; compiled only for
/// tests and for backends running the suite (no production path can use fixed key
/// material — every constructor here takes `first_byte` or a fixed text).
#[cfg(any(test, feature = "testing"))]
pub mod tests_support {
    use spot_auth::TokenDigest;

    /// The digest of a fixed text, so tests can name a token verifiably.
    #[must_use]
    pub fn digest_of(text: &str) -> TokenDigest {
        match TokenDigest::from_bytes(&crate::crypto::sha256(text.as_bytes())) {
            Ok(digest) => digest,
            Err(_) => unreachable!("a 32-byte slice always builds a TokenDigest"),
        }
    }

    /// A fixed salt, so two tests digesting the same address get the same source hash.
    #[must_use]
    pub fn fixed_salt(first_byte: u8) -> crate::model::SaltKey {
        match crate::model::SaltKey::from_bytes(&[first_byte; 32]) {
            Ok(salt) => salt,
            Err(_) => unreachable!("32 bytes always build a SaltKey"),
        }
    }

    /// A [`SourceHash`](crate::model::SourceHash) under the fixed test salt.
    #[must_use]
    pub fn source_of(address: &str) -> crate::model::SourceHash {
        fixed_salt(1).digest_source(address)
    }
}

/// The store contract suite — compiled for this crate's own tests (which run it against
/// [`InMemorySessionStore`](crate::memory)) and for backend suites behind the `testing`
/// feature.
#[cfg(any(test, feature = "testing"))]
pub mod contract;

pub use error::SessionError;
pub use manager::{EndReport, SessionManager};
pub use memory::InMemorySessionStore;
pub use model::{ClientInfo, SaltKey, Session, SessionPolicy, SessionState, SourceHash, UserKey};
pub use store::{SessionFuture, SessionStore};
pub use throttle::{
    AuditNote, ThrottleAction, ThrottleConfig, ThrottleDecision, ThrottleError, ThrottleKind,
    ThrottleStore, UnlockedBy,
};
