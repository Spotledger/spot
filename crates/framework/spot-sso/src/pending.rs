//! The pending authorization a sign-in is in the middle of, and the store that keeps it.
//!
//! [`PendingAuth`] is everything the callback needs and nothing a reader could use: it holds
//! the expected `state` and `nonce`, the PKCE verifier, where the sign-in came from and when
//! it expires. It is serialisable so the caller can store it server-side, keyed by a hash of
//! the browser's cookie, and single use.
//!
//! This crate never stores it: the caller supplies a [`PendingStore`]. The only implementation
//! here is an in-memory fake for tests and single-node development.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{self as e, Result, SsoError};
use crate::provider::ProviderId;

/// How long a pending authorization is valid, in seconds.
pub const PENDING_TTL_SECONDS: i64 = 600;

/// A pending authorization-code sign-in.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingAuth {
    /// The provider the sign-in is with.
    pub provider: ProviderId,
    /// The expected `state`.
    pub state: String,
    /// The expected ID-token `nonce`.
    pub nonce: String,
    /// The PKCE `code_verifier` (never logged).
    pub code_verifier: String,
    /// The PKCE `code_challenge` the verifier must still match.
    pub code_challenge: String,
    /// The exact redirect URI sent to the provider.
    pub redirect_uri: String,
    /// The same-site relative path to return the browser to after sign-in.
    pub return_to: String,
    /// When the sign-in began, in unix seconds.
    pub created_at: i64,
    /// When it stops being valid, in unix seconds.
    pub expires_at: i64,
    /// Whether it has already been used (single use).
    pub used: bool,
}

impl std::fmt::Debug for PendingAuth {
    /// Redacts the `state`, `nonce` and PKCE verifier: they are single-use secrets that must
    /// never reach a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingAuth")
            .field("provider", &self.provider)
            .field("state", &"<redacted>")
            .field("nonce", &"<redacted>")
            .field("code_verifier", &"<redacted>")
            .field("code_challenge", &self.code_challenge)
            .field("redirect_uri", &self.redirect_uri)
            .field("return_to", &self.return_to)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("used", &self.used)
            .finish()
    }
}

impl PendingAuth {
    /// Whether the pending authorization has expired at `now`.
    #[must_use]
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }

    /// Consumes the pending authorization once, refusing a replay or an expiry.
    ///
    /// # Errors
    ///
    /// [`e::codes::STATE_REPLAYED`] when it was already used; [`e::codes::STATE_EXPIRED`] when
    /// its time has passed.
    pub fn consume(&self, now: i64) -> Result<PendingAuth> {
        if self.used {
            return Err(SsoError::new(
                e::codes::STATE_REPLAYED,
                "This sign-in attempt has already been completed.",
                "Start the sign-in again from the beginning.",
            ));
        }
        if self.is_expired(now) {
            return Err(SsoError::new(
                e::codes::STATE_EXPIRED,
                "This sign-in attempt has taken too long.",
                "Start the sign-in again; the attempt expires after ten minutes.",
            ));
        }
        Ok(PendingAuth {
            used: true,
            ..self.clone()
        })
    }
}

/// Stores pending authorizations server-side, keyed by the browser binding.
///
/// `take` removes the entry, which is what makes a single use enforceable even if a caller
/// ignores the `used` flag: a second callback finds nothing.
pub trait PendingStore {
    /// Stores `pending` under `binding` (a hash of the browser cookie), replacing any entry.
    fn put(&mut self, binding: &str, pending: PendingAuth);
    /// Removes and returns the entry for `binding`.
    fn take(&mut self, binding: &str) -> Option<PendingAuth>;
}

/// An in-memory [`PendingStore`] for tests and single-node development.
#[derive(Debug, Default)]
pub struct MemoryPendingStore {
    entries: HashMap<String, PendingAuth>,
}

impl MemoryPendingStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many entries are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl PendingStore for MemoryPendingStore {
    fn put(&mut self, binding: &str, pending: PendingAuth) {
        self.entries.insert(binding.to_owned(), pending);
    }

    fn take(&mut self, binding: &str) -> Option<PendingAuth> {
        self.entries.remove(binding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_the_secrets() {
        let pending = PendingAuth {
            provider: ProviderId::new("acme"),
            state: "the-state-value".to_owned(),
            nonce: "the-nonce-value".to_owned(),
            code_verifier: "the-verifier-value".to_owned(),
            code_challenge: "the-challenge".to_owned(),
            redirect_uri: "https://ledger.example.com/cb".to_owned(),
            return_to: "/app".to_owned(),
            created_at: 0,
            expires_at: 600,
            used: false,
        };
        let rendered = format!("{pending:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("the-state-value"));
        assert!(!rendered.contains("the-nonce-value"));
        assert!(!rendered.contains("the-verifier-value"));
    }
}
