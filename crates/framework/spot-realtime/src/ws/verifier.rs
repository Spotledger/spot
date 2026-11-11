//! The authentication seam of the WebSocket transport: [`TokenVerifier`].
//!
//! The transport never interprets a token: it hands the `auth` message's token to the
//! verifier and, on success, trusts the returned [`Principal`](crate::Principal) (whose id
//! must be the user's `RecordId` per the native API contract — it becomes `ready.user` and
//! the auto-joined `user:<id>` room). Session revocation is detected through
//! [`TokenVerifier::expiry_watch`].

use std::future::Future;
use std::pin::Pin;

use spot_core::SiteId;

use crate::event::Principal;

/// Why a token was rejected by a [`TokenVerifier`].
///
/// The transport only reports it as close code 4001 (sign in again); whatever was here is not
/// the client's business, so the reason is for the server log, not the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rejected {
    reason: String,
}

impl Rejected {
    /// A rejection with the given reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// Why the token was rejected.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("token rejected: ")
    }
}

impl std::error::Error for Rejected {}

/// Detects the expiry or revocation of one authenticated session ([`TokenVerifier`]).
///
/// The transport keeps a watcher task alive that calls [`wait`](ExpiryWatch::wait) and
/// re-runs `verify` when it resolves: a token that was valid at `auth` but no longer is a
/// session that expired or was revoked, and the connection closes with 4009. A watch that
/// never resolves simply means the implementation cannot detect expiry and the connection
/// stays until the client goes away — a TTL-only token should fire `wait` when its lifetime
/// ends.
pub trait ExpiryWatch: Send + Sync {
    /// Resolves when the session this watch tracks **may** have expired or been revoked.
    /// Resolving is a hint: the transport re-runs `verify` and closes 4009 only when the
    /// token is actually gone. The boxed form keeps the trait object-safe.
    fn wait(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Verifies the `auth` token of a realtime WebSocket connection.
///
/// Object-safe (`Arc<dyn TokenVerifier>`); `verify` returns a boxed future (like
/// [`ExpiryWatch::wait`]) so a verifier over a durable session store can await its lookup
/// instead of caching or blocking inside the connection task. The transport awaits it inside
/// the 10 s auth window and the expiry re-check, never while holding a lock, so a slow
/// verifier ends in the same close codes (4001 / 4009) as a rejecting one.
pub trait TokenVerifier: Send + Sync {
    /// Verifies `token` for `site`. `Ok(principal)` authenticates the connection;
    /// `Err(Rejected)` closes it with 4001.
    fn verify<'a>(
        &'a self,
        site: &'a SiteId,
        token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Principal, Rejected>> + Send + 'a>>;

    /// A watch for the just-verified session, when the implementation can detect its expiry
    /// or revocation after `verify` succeeded. `None` (the default) means the transport does
    /// not watch this connection — it stays until the client disconnects or the server shuts
    /// down.
    fn expiry_watch(&self, token: &str) -> Option<std::sync::Arc<dyn ExpiryWatch>> {
        let _ = token;
        None
    }
}
