//! The [`SessionStore`] contract: every command a session backend answers.
//!
//! The trait is object safe (`Arc<dyn SessionStore>`) and every multi-value or fallible
//! command returns a boxed future, exactly like `spot-cache`'s [`CacheStore`], so the
//! [`InMemorySessionStore`](crate::memory) fake and WP-PLT-02.T3's storage-backed store
//! are interchangeable behind one type. The store never reads a clock: every instant is a
//! unix-seconds argument.
//!
//! # The one security invariant
//!
//! A store holds **only digests**. [`SessionRecord`] carries a
//! [`TokenDigest`](spot_auth::TokenDigest), and no command returns the token text the user
//! presented — issuing a token is [`crate::manager::SessionManager::start`]'s job, and the
//! manager holds the text only as long as the response needs. A store type is
//! *type-level incapable* of answering "what was the token?": the bytes were never given
//! to it. The contract suite asserts this shape (`contract::run_all`, suite
//! `store_holds_only_digests`) by compiling a store from digests alone and proving every
//! public surface returns at most a [`SessionRecord`].
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_auth::TokenDigest;

use crate::error::SessionError;
use crate::model::{Session, SourceHash, UserKey};

/// The future every [`SessionStore`] command returns (object safety: boxed).
pub type SessionFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SessionError>> + Send + 'a>>;

/// The at-rest session record a store returns and holds.
///
/// Deliberately exactly [`Session`]: the record and the domain type are the same shape so
/// a storage layer cannot quietly hold more (a token text would need its own field, which
/// the type does not have).
pub type SessionRecord = Session;

/// What `touch` decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touched {
    /// The idle window moved (the extension met the granularity).
    Extended,
    /// The extension was under [`crate::model::TOUCH_GRANULARITY`] and was skipped: no
    /// write happened, the session is still alive.
    Skipped,
}

/// Which sessions a bulk revoke targeted, for the caller's audit note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevokeReport {
    /// How many sessions changed state to revoked.
    pub revoked: usize,
}

/// The session storage contract, in the spirit of `spot-scheduler`'s [`LeaseStore`]:
/// object safe, clock-injected, half-open windows. Expiry is lazy — a session whose
/// window has passed is expired to every reader at the next call with a later `now`, and
/// `purge_expired` is the store's chance to reclaim space (a store may also keep expired
/// rows for a retention window; correctness never depends on the purge).
///
/// Every command that changes a session's `last_seen` is a **touche** under the
/// granularity rule: extends idle only, never past `absolute_expires`.
pub trait SessionStore: Send + Sync {
    /// Inserts a session built by the manager (the digest is already computed; the store
    /// takes no token text, so it cannot store one) and returns the record **as stored**:
    /// a store that assigns the id itself echoes it back here, so the manager can hand
    /// the caller a record whose id is real. Fails with [`SessionError::StoreFailed`] if
    /// the backend cannot write.
    fn create<'a>(&'a self, session: SessionRecord) -> SessionFuture<'a, SessionRecord>;

    /// The live-or-recently-expired session for `digest`, if the store has one. Returns
    /// the record regardless of expiry — the **manager** decides expiry/revocation against
    /// `now`, so a store that pre-filters could never be tested for the half-open
    /// boundaries. `None` means unknown digest.
    fn find_by_digest<'a>(
        &'a self,
        digest: &'a TokenDigest,
    ) -> SessionFuture<'a, Option<SessionRecord>>;

    /// Extends the idle window of session `id` to `now + idle`, clamped to the session's
    /// `absolute_expires`, and writes only when the extension **gain** over the stored
    /// window is at least [`crate::model::TOUCH_GRANULARITY`] — a busy client writes at
    /// most once per granularity instead of per request. A dead session (revoked or
    /// expired at `now`) is a [`Touched::Skipped`] without a write.
    ///
    /// `idle` is the policy's idle duration; the store computes `candidate = now + idle`
    /// and never lets the window pass `absolute_expires` (the clamp), so no touch —
    /// however many run concurrently — extends a session past its absolute end.
    fn touch<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        idle: i64,
        absolute_expires: i64,
    ) -> SessionFuture<'a, Touched>;

    /// Swaps the session's token digest for `new_digest` **in place**: the same session
    /// (same id, window, user, context) continues under the new token and the old digest
    /// stops resolving the instant this lands (the old token is dead at once). The
    /// session stays Active — rotation is not an end. Fails when `new_digest` is already
    /// in use or the id is unknown.
    fn rotate<'a>(&'a self, id: &'a str, new_digest: TokenDigest) -> SessionFuture<'a, ()>;

    /// Marks the session revoked with `reason`. Idempotent: revoking again keeps the
    /// first reason (audit: who revoked first) and still succeeds.
    fn revoke<'a>(&'a self, id: &'a str, reason: String) -> SessionFuture<'a, RevokeReport>;

    /// Marks every active session of `user` revoked with `reason`, except `except` (the
    /// current session, when the caller is *that* session: "log out everywhere else",
    /// ADR-070 §5.4). Returns the count.
    fn revoke_all_for_user<'a>(
        &'a self,
        user: &'a UserKey,
        except: Option<&'a str>,
        reason: String,
    ) -> SessionFuture<'a, RevokeReport>;

    /// Every session of `user`, any state, oldest first (creation order), for the
    /// user-facing list (ADR-070 §5.4: a user can list their own sessions).
    fn list_for_user<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SessionRecord>>;

    /// Reclaims expired sessions up to `limit`, returning how many were removed.
    /// Only sessions already expired at `now` (half-open) are removed.
    fn purge_expired<'a>(&'a self, now: i64, limit: usize) -> SessionFuture<'a, usize>;

    /// The oldest still-alive session of `user` judged against `now`, or `None` when the
    /// user holds none. The manager uses it to decide which session a `max_per_user`
    /// overflow evicts (the oldest dies with reason `evicted`).
    fn oldest_active<'a>(
        &'a self,
        user: &'a UserKey,
        now: i64,
    ) -> SessionFuture<'a, Option<SessionRecord>>;

    /// Whether a digest is already in this store (rotation re-issues into a fresh record;
    /// this only guards a pathological double-create).
    fn exists<'a>(&'a self, digest: &'a TokenDigest) -> SessionFuture<'a, bool>;

    /// Counts a session's *source* toward the per-user source set (a convenience for
    /// tests): the `SourceHash` values of every active session of `user`.
    fn active_sources<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SourceHash>>;
}

/// Whether `session` is purgeable at `now`: expired under the half-open rule and not
/// already revoked-and-collected. Stores may keep revoked rows for audit; correctness
/// does not depend on it.
#[must_use]
pub fn purgeable(session: &SessionRecord, now: i64) -> bool {
    session.expired_at(now)
}

/// A boxed store is a store: the manager over `Arc<dyn SessionStore>` and the contract
/// suite over `Box<dyn SessionStore>` both forward through this blanket impl, so a
/// caller can erase the backend type without re-implementing anything.
impl<T: SessionStore + ?Sized> SessionStore for Box<T> {
    fn create<'a>(&'a self, session: SessionRecord) -> SessionFuture<'a, SessionRecord> {
        (**self).create(session)
    }

    fn find_by_digest<'a>(
        &'a self,
        digest: &'a TokenDigest,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        (**self).find_by_digest(digest)
    }

    fn touch<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        idle: i64,
        absolute_expires: i64,
    ) -> SessionFuture<'a, Touched> {
        (**self).touch(id, now, idle, absolute_expires)
    }

    fn rotate<'a>(&'a self, id: &'a str, new_digest: TokenDigest) -> SessionFuture<'a, ()> {
        (**self).rotate(id, new_digest)
    }

    fn revoke<'a>(&'a self, id: &'a str, reason: String) -> SessionFuture<'a, RevokeReport> {
        (**self).revoke(id, reason)
    }

    fn revoke_all_for_user<'a>(
        &'a self,
        user: &'a UserKey,
        except: Option<&'a str>,
        reason: String,
    ) -> SessionFuture<'a, RevokeReport> {
        (**self).revoke_all_for_user(user, except, reason)
    }

    fn list_for_user<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SessionRecord>> {
        (**self).list_for_user(user)
    }

    fn purge_expired<'a>(&'a self, now: i64, limit: usize) -> SessionFuture<'a, usize> {
        (**self).purge_expired(now, limit)
    }

    fn oldest_active<'a>(
        &'a self,
        user: &'a UserKey,
        now: i64,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        (**self).oldest_active(user, now)
    }

    fn exists<'a>(&'a self, digest: &'a TokenDigest) -> SessionFuture<'a, bool> {
        (**self).exists(digest)
    }

    fn active_sources<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SourceHash>> {
        (**self).active_sources(user)
    }
}

/// A shared store is a store (the production wiring: `Arc<dyn SessionStore>` behind the
/// HTTP layer).
impl<T: SessionStore + ?Sized> SessionStore for Arc<T> {
    fn create<'a>(&'a self, session: SessionRecord) -> SessionFuture<'a, SessionRecord> {
        (**self).create(session)
    }

    fn find_by_digest<'a>(
        &'a self,
        digest: &'a TokenDigest,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        (**self).find_by_digest(digest)
    }

    fn touch<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        idle: i64,
        absolute_expires: i64,
    ) -> SessionFuture<'a, Touched> {
        (**self).touch(id, now, idle, absolute_expires)
    }

    fn rotate<'a>(&'a self, id: &'a str, new_digest: TokenDigest) -> SessionFuture<'a, ()> {
        (**self).rotate(id, new_digest)
    }

    fn revoke<'a>(&'a self, id: &'a str, reason: String) -> SessionFuture<'a, RevokeReport> {
        (**self).revoke(id, reason)
    }

    fn revoke_all_for_user<'a>(
        &'a self,
        user: &'a UserKey,
        except: Option<&'a str>,
        reason: String,
    ) -> SessionFuture<'a, RevokeReport> {
        (**self).revoke_all_for_user(user, except, reason)
    }

    fn list_for_user<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SessionRecord>> {
        (**self).list_for_user(user)
    }

    fn purge_expired<'a>(&'a self, now: i64, limit: usize) -> SessionFuture<'a, usize> {
        (**self).purge_expired(now, limit)
    }

    fn oldest_active<'a>(
        &'a self,
        user: &'a UserKey,
        now: i64,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        (**self).oldest_active(user, now)
    }

    fn exists<'a>(&'a self, digest: &'a TokenDigest) -> SessionFuture<'a, bool> {
        (**self).exists(digest)
    }

    fn active_sources<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SourceHash>> {
        (**self).active_sources(user)
    }
}
/// Every command forwards to the referenced store. Generality for the contract suite over
/// `&S` and any wrapper the caller holds.
impl<T: SessionStore + ?Sized> SessionStore for &T {
    fn create<'a>(&'a self, session: SessionRecord) -> SessionFuture<'a, SessionRecord> {
        (**self).create(session)
    }

    fn find_by_digest<'a>(
        &'a self,
        digest: &'a TokenDigest,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        (**self).find_by_digest(digest)
    }

    fn touch<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        idle: i64,
        absolute_expires: i64,
    ) -> SessionFuture<'a, Touched> {
        (**self).touch(id, now, idle, absolute_expires)
    }

    fn rotate<'a>(&'a self, id: &'a str, new_digest: TokenDigest) -> SessionFuture<'a, ()> {
        (**self).rotate(id, new_digest)
    }

    fn revoke<'a>(&'a self, id: &'a str, reason: String) -> SessionFuture<'a, RevokeReport> {
        (**self).revoke(id, reason)
    }

    fn revoke_all_for_user<'a>(
        &'a self,
        user: &'a UserKey,
        except: Option<&'a str>,
        reason: String,
    ) -> SessionFuture<'a, RevokeReport> {
        (**self).revoke_all_for_user(user, except, reason)
    }

    fn list_for_user<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SessionRecord>> {
        (**self).list_for_user(user)
    }

    fn purge_expired<'a>(&'a self, now: i64, limit: usize) -> SessionFuture<'a, usize> {
        (**self).purge_expired(now, limit)
    }

    fn oldest_active<'a>(
        &'a self,
        user: &'a UserKey,
        now: i64,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        (**self).oldest_active(user, now)
    }

    fn exists<'a>(&'a self, digest: &'a TokenDigest) -> SessionFuture<'a, bool> {
        (**self).exists(digest)
    }

    fn active_sources<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SourceHash>> {
        (**self).active_sources(user)
    }
}
