//! Replay protection for single-use identifiers.
//!
//! A signed message may be perfectly authentic and still be a replay of one already used.
//! SAML assertions and `LogoutRequest`s name an `ID`; an OIDC back-channel logout token names a
//! `jti`. The caller supplies a [`ReplayStore`] and the verifiers claim the identifier once,
//! after every other check has passed, so an unauthenticated message cannot fill the store.
//!
//! # The port is fallible and fails closed
//!
//! [`ReplayStore::claim`] is asynchronous, takes `&self` (a shared store behind an `Arc`) and
//! returns a `Result`: a durable store can be down. Every caller in this crate treats a
//! [`ReplayError`] as "refuse the message", never as "fresh", so an outage of the store can
//! turn a sign-out away but can never let a replayed assertion through.
//!
//! # Namespaces
//!
//! The store sees one flat key space. The verifiers prefix every id with its kind (`oidc-logout`,
//! `saml-assertion`, `saml-logout`) and the provider it belongs to, each part length-prefixed
//! (see [`namespaced`]), so two kinds or two providers never collide. A store that serves more
//! than one tenant must also scope its rows by tenant (the caller's connection or schema).
//!
//! # Production deployments
//!
//! [`MemoryReplayStore`] is per process and for tests and single-node development only. On
//! any deployment with more than one node the replay store **must** be shared and durable:
//! a token accepted on node A would otherwise be accepted again on node B, and a restart would
//! forget every id. The PostgreSQL implementation is WP-IAM-05.T6.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use crate::error::{self as e, SsoError};

/// A boxed, `Send` future: the return type of every store method. The output is the whole
/// result, so one alias serves every store port.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The outcome of claiming an identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// The identifier was not recorded (or its record had expired); it is recorded now.
    Fresh,
    /// The identifier is recorded and has not expired: this is a replay.
    Replayed,
}

/// The store could not answer. The caller must refuse the message (fail closed).
///
/// It carries no identifier, token or assertion, so it is safe to log and to return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayError {
    _private: (),
}

impl ReplayError {
    /// The store is unreachable, full, or in a state it cannot vouch for.
    #[must_use]
    pub fn unavailable() -> Self {
        Self { _private: () }
    }

    /// The stable machine-readable code, `sso.replay_store_unavailable`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        e::codes::REPLAY_STORE_UNAVAILABLE
    }
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("The replay store could not be reached; the message was refused.")
    }
}

impl std::error::Error for ReplayError {}

impl From<ReplayError> for SsoError {
    /// A refusal: the message is turned away, with the stable code and no detail.
    fn from(_: ReplayError) -> Self {
        SsoError::new(
            e::codes::REPLAY_STORE_UNAVAILABLE,
            "The request could not be checked and was refused.",
            "The replay store is unavailable; restore it and the sender may retry.",
        )
    }
}

/// Records identifiers that must not be used twice.
pub trait ReplayStore: Send + Sync {
    /// Atomically records `id` until `expires_at` and returns [`Claim::Fresh`]; returns
    /// [`Claim::Replayed`] when `id` is already recorded and has not yet expired at `now`.
    /// Of any number of concurrent claims of one id, exactly one is fresh.
    ///
    /// The caller must claim exactly once, after the signature and conditions have been
    /// verified, so an unauthenticated message cannot fill the store.
    ///
    /// # Errors
    ///
    /// [`ReplayError`] when the store cannot give a trustworthy answer. The caller refuses the
    /// message; it must never treat an error as fresh.
    fn claim<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        expires_at: i64,
    ) -> StoreFuture<'a, Result<Claim, ReplayError>>;
}

impl<T: ReplayStore + ?Sized> ReplayStore for std::sync::Arc<T> {
    fn claim<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        expires_at: i64,
    ) -> StoreFuture<'a, Result<Claim, ReplayError>> {
        (**self).claim(id, now, expires_at)
    }
}

/// Builds the store key for `id` of `kind` under `scope` (the provider): each part is
/// length-prefixed, so no choice of ids can make two different triples produce one key.
#[must_use]
pub fn namespaced(kind: &str, scope: &str, id: &str) -> String {
    format!(
        "{}:{kind}{}:{scope}{}:{id}",
        kind.len(),
        scope.len(),
        id.len()
    )
}

/// The most ids a [`MemoryReplayStore`] holds unless told otherwise.
pub const DEFAULT_MEMORY_CAPACITY: usize = 100_000;

/// An in-memory [`ReplayStore`] for **tests and single-process development only**.
///
/// It is per process: it shares nothing with another node and forgets everything on restart,
/// so it must not back a deployment with more than one node (see the module documentation).
/// Its size is bounded: when full, expired ids are pruned, and if it is still full a claim
/// fails closed with a [`ReplayError`] rather than forgetting a live id.
#[derive(Debug)]
pub struct MemoryReplayStore {
    seen: Mutex<HashMap<String, i64>>,
    capacity: usize,
}

impl Default for MemoryReplayStore {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_MEMORY_CAPACITY)
    }
}

impl MemoryReplayStore {
    /// An empty store with the default bound.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store that holds at most `capacity` live ids.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            seen: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// How many entries are held (expired ones count until the next prune).
    #[must_use]
    pub fn len(&self) -> usize {
        self.seen.lock().map_or(0, |seen| seen.len())
    }

    /// Whether the store holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn claim_now(&self, id: &str, now: i64, expires_at: i64) -> Result<Claim, ReplayError> {
        // A poisoned lock means a claim panicked mid-update: fail closed.
        let mut seen = self.seen.lock().map_err(|_| ReplayError::unavailable())?;
        if let Some(expiry) = seen.get(id)
            && now < *expiry
        {
            return Ok(Claim::Replayed);
        }
        if seen.len() >= self.capacity && !seen.contains_key(id) {
            seen.retain(|_, expiry| now < *expiry);
            if seen.len() >= self.capacity {
                return Err(ReplayError::unavailable());
            }
        }
        seen.insert(id.to_owned(), expires_at);
        Ok(Claim::Fresh)
    }
}

impl ReplayStore for MemoryReplayStore {
    fn claim<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        expires_at: i64,
    ) -> StoreFuture<'a, Result<Claim, ReplayError>> {
        Box::pin(std::future::ready(self.claim_now(id, now, expires_at)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::block_on;

    #[test]
    fn the_same_id_is_refused_until_it_expires() {
        let store = MemoryReplayStore::new();
        assert_eq!(block_on(store.claim("a", 100, 200)), Ok(Claim::Fresh));
        assert_eq!(block_on(store.claim("a", 150, 200)), Ok(Claim::Replayed));
        // Once expired, the id is forgotten and may be recorded again.
        assert_eq!(block_on(store.claim("a", 200, 300)), Ok(Claim::Fresh));
    }

    #[test]
    fn distinct_ids_are_independent() {
        let store = MemoryReplayStore::new();
        assert_eq!(block_on(store.claim("a", 0, 100)), Ok(Claim::Fresh));
        assert_eq!(block_on(store.claim("b", 0, 100)), Ok(Claim::Fresh));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn a_full_store_prunes_expired_ids_and_then_fails_closed() {
        let store = MemoryReplayStore::with_capacity(2);
        assert_eq!(block_on(store.claim("a", 0, 10)), Ok(Claim::Fresh));
        assert_eq!(block_on(store.claim("b", 0, 100)), Ok(Claim::Fresh));
        // "a" has expired at 10: pruned to make room.
        assert_eq!(block_on(store.claim("c", 10, 100)), Ok(Claim::Fresh));
        assert_eq!(store.len(), 2);
        // Nothing is expired: the store refuses rather than forgetting a live id.
        assert_eq!(
            block_on(store.claim("d", 20, 100)),
            Err(ReplayError::unavailable())
        );
        assert_eq!(block_on(store.claim("b", 20, 100)), Ok(Claim::Replayed));
    }

    #[test]
    fn keys_of_different_kind_scope_or_id_never_collide() {
        let keys = [
            namespaced("a", "bc", "d"),
            namespaced("ab", "c", "d"),
            namespaced("a", "b", "cd"),
            namespaced("a", "bc", "d:"),
        ];
        for (i, left) in keys.iter().enumerate() {
            for right in &keys[i + 1..] {
                assert_ne!(left, right);
            }
        }
    }

    #[test]
    fn the_error_names_no_identifier_and_converts_to_a_refusal() {
        let error = ReplayError::unavailable();
        assert_eq!(error.code(), "sso.replay_store_unavailable");
        let sso: SsoError = error.into();
        assert_eq!(sso.code(), "sso.replay_store_unavailable");
    }
}
