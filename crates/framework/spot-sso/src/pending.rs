//! The pending authorization a sign-in is in the middle of, and the store that keeps it.
//!
//! [`PendingAuth`] is everything the callback needs and nothing a reader could use: it holds
//! the expected `state` and `nonce`, the PKCE verifier, where the sign-in came from and when
//! it expires. It is serialisable so the caller can store it server-side, keyed by a hash of
//! the browser's cookie, and single use.
//!
//! This crate never stores it: the caller supplies a [`PendingStore`], which is asynchronous,
//! fallible and fails closed (an error refuses the callback), and whose `take` is atomic so a
//! `state` is consumed once even across nodes. The only implementation here is an in-memory
//! fake for tests and single-node development; the PostgreSQL one is in `spot-sso-pg`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::error::{self as e, Result, SsoError};
use crate::provider::ProviderId;
use crate::replay::StoreFuture;

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

/// The pending store could not answer. The caller must refuse the callback (fail closed).
///
/// It carries no binding, state, nonce or verifier, so it is safe to log and to return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingError {
    _private: (),
}

impl PendingError {
    /// The store is unreachable, or in a state it cannot vouch for.
    #[must_use]
    pub fn unavailable() -> Self {
        Self { _private: () }
    }

    /// The stable machine-readable code, `sso.pending_store_unavailable`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        e::codes::PENDING_STORE_UNAVAILABLE
    }
}

impl std::fmt::Display for PendingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The pending sign-in store could not be reached; the sign-in was refused.")
    }
}

impl std::error::Error for PendingError {}

impl From<PendingError> for SsoError {
    /// A refusal: the callback is turned away, with the stable code and no detail.
    fn from(_: PendingError) -> Self {
        SsoError::new(
            e::codes::PENDING_STORE_UNAVAILABLE,
            "The sign-in could not be checked and was refused.",
            "The pending-authorisation store is unavailable; restore it and the user may start again.",
        )
    }
}

/// Stores pending authorizations server-side, keyed by the browser binding.
///
/// A store serves **one tenant** (the tenant is fixed when the store is built), so a binding
/// of tenant A is invisible to tenant B: `take` answers exactly as for a missing one.
///
/// The port is asynchronous, takes `&self` (a shared store behind an `Arc`, shared between
/// nodes in production) and is fallible: an error is a refusal of the callback, never a pass.
/// `take` removes the entry **atomically**, which is what makes a single use enforceable even
/// when two nodes race for one `state`: of any number of concurrent takes, exactly one gets the
/// entry.
pub trait PendingStore: Send + Sync {
    /// Stores `pending` under `binding` (a hash of the browser cookie), replacing any entry.
    ///
    /// # Errors
    ///
    /// [`PendingError`] when the store cannot record it; the sign-in must not start.
    fn put<'a>(
        &'a self,
        binding: &'a str,
        pending: &'a PendingAuth,
    ) -> StoreFuture<'a, core::result::Result<(), PendingError>>;

    /// Removes and returns the live entry for `binding`: `None` when there is none, when it was
    /// taken already, or when it had expired at `now` (an expired entry is removed too).
    ///
    /// # Errors
    ///
    /// [`PendingError`] when the store cannot give a trustworthy answer; the callback is
    /// refused, and the entry is **not** consumed unless the store removed it.
    fn take<'a>(
        &'a self,
        binding: &'a str,
        now: i64,
    ) -> StoreFuture<'a, core::result::Result<Option<PendingAuth>, PendingError>>;
}

impl<T: PendingStore + ?Sized> PendingStore for std::sync::Arc<T> {
    fn put<'a>(
        &'a self,
        binding: &'a str,
        pending: &'a PendingAuth,
    ) -> StoreFuture<'a, core::result::Result<(), PendingError>> {
        (**self).put(binding, pending)
    }

    fn take<'a>(
        &'a self,
        binding: &'a str,
        now: i64,
    ) -> StoreFuture<'a, core::result::Result<Option<PendingAuth>, PendingError>> {
        (**self).take(binding, now)
    }
}

/// The entries of every tenant of one in-memory backend, keyed `(tenant, binding)`.
type Entries = Arc<Mutex<HashMap<(String, String), PendingAuth>>>;

/// An in-memory [`PendingStore`] for **tests and single-node development only**.
///
/// It is per process: it shares nothing with another node and forgets everything on restart.
/// [`MemoryPendingStore::for_tenant`] gives another tenant's view of the same backend, which is
/// how the tenant-isolation case of the contract suite is run.
#[derive(Clone, Debug)]
pub struct MemoryPendingStore {
    tenant: String,
    entries: Entries,
}

impl Default for MemoryPendingStore {
    /// An empty store for the tenant named `default`.
    fn default() -> Self {
        Self {
            tenant: "default".to_owned(),
            entries: Entries::default(),
        }
    }
}

impl MemoryPendingStore {
    /// An empty store for the tenant named `default`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Another tenant's view of the same backend.
    #[must_use]
    pub fn for_tenant(&self, tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            entries: Arc::clone(&self.entries),
        }
    }

    /// How many entries this tenant holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |entries| {
            entries
                .keys()
                .filter(|(tenant, _)| *tenant == self.tenant)
                .count()
        })
    }

    /// Whether this tenant holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn key(&self, binding: &str) -> (String, String) {
        (self.tenant.clone(), binding.to_owned())
    }
}

impl PendingStore for MemoryPendingStore {
    fn put<'a>(
        &'a self,
        binding: &'a str,
        pending: &'a PendingAuth,
    ) -> StoreFuture<'a, core::result::Result<(), PendingError>> {
        let outcome = match self.entries.lock() {
            Ok(mut entries) => {
                entries.insert(self.key(binding), pending.clone());
                Ok(())
            }
            // A poisoned lock means an operation panicked mid-update: fail closed.
            Err(_) => Err(PendingError::unavailable()),
        };
        Box::pin(std::future::ready(outcome))
    }

    fn take<'a>(
        &'a self,
        binding: &'a str,
        now: i64,
    ) -> StoreFuture<'a, core::result::Result<Option<PendingAuth>, PendingError>> {
        let outcome = match self.entries.lock() {
            Ok(mut entries) => Ok(entries
                .remove(&self.key(binding))
                .filter(|pending| !pending.is_expired(now))),
            Err(_) => Err(PendingError::unavailable()),
        };
        Box::pin(std::future::ready(outcome))
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
