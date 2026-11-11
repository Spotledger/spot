//! The [`InMemorySessionStore`]: the reference [`SessionStore`] in one mutex.
//!
//! Same shape as `spot-scheduler`'s `InMemoryLeaseStore`: a `Mutex` over a plain map,
//! poisoned-lock recovery via `unwrap_or_else`, lazily-determined expiry, ids and digests
//! as the only keys. Every decision carries a reason in its doc comment. Single-node hosts
//! and every test in this crate run on this fake; WP-PLT-02.T3 builds the real storage
//! backend and runs the same [`crate::contract`] suite against it.
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use spot_auth::TokenDigest;

use crate::error::SessionError;
use crate::model::{SessionEndReason, SessionState, SourceHash, TOUCH_GRANULARITY, UserKey};
use crate::store::{RevokeReport, SessionFuture, SessionRecord, SessionStore, Touched};

/// One id counter shared by every in-memory store instance (ids are unique per store, not
/// globally, so each store counts from 1).
fn next_id(ids: &mut u64) -> String {
    *ids += 1;
    format!("sess-{ids:016x}")
}

/// One session inside the map: the record plus nothing else — there is no token text or
/// side state to keep.
type Entry = SessionRecord;

/// The whole mutable state of the store.
#[derive(Debug, Default)]
struct State {
    /// Sessions keyed by id; BTreeMap so `list_for_user` is in insertion (then id) order —
    /// creation order for `oldest_active` and eviction.
    by_id: BTreeMap<String, Entry>,
    /// Which id each digest belongs to. TokenDigest does not hash, so the lookup key is
    /// its bytes.
    by_digest: BTreeMap<[u8; 32], String>,
    /// The session-id counter.
    ids: u64,
}

/// A [`SessionStore`] in one mutex, for tests and single-node hosts.
///
/// The clock is injectable: callers pass their instants in. Expiry is lazy — no sweep
/// thread; every reader decides against the `now` it was given, and `purge_expired`
/// reclaims.
#[derive(Debug)]
pub struct InMemorySessionStore {
    state: Mutex<State>,
}

impl InMemorySessionStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The guarded data is a plain map; a panic elsewhere cannot leave it inconsistent.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for InMemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for InMemorySessionStore {
    fn create<'a>(&'a self, session: SessionRecord) -> SessionFuture<'a, SessionRecord> {
        let mut state = self.lock();
        if state
            .by_digest
            .contains_key(&digest_key(&session.token_digest))
        {
            // The manager never re-issues the same digest; reaching here means a bug.
            return Box::pin(async move {
                Err(SessionError::StoreFailed(
                    "a session with this digest already exists".to_owned(),
                ))
            });
        }
        let id = if session.id.is_empty() {
            next_id(&mut state.ids)
        } else {
            session.id.clone()
        };
        let mut record = session;
        record.id = id.clone();
        state.by_digest.insert(digest_key(&record.token_digest), id);
        let stored = record.clone();
        state.by_id.insert(record.id.clone(), record);
        Box::pin(async move { Ok(stored) })
    }

    fn find_by_digest<'a>(
        &'a self,
        digest: &'a TokenDigest,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        let state = self.lock();
        let found = state
            .by_digest
            .get(&digest_key(digest))
            .and_then(|id| state.by_id.get(id))
            .cloned(); // The record holds only digests: cloning it leaks no secret text.
        Box::pin(async { Ok(found) })
    }

    fn touch<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        idle: i64,
        absolute_expires: i64,
    ) -> SessionFuture<'a, Touched> {
        let mut state = self.lock();
        let touched = match state.by_id.get_mut(id) {
            Some(record) => {
                // Only a live session touches: revoked or already-expired rows freeze.
                let revoked = record.is_revoked();
                let expired = record.expired_at(now);
                if revoked || expired {
                    Touched::Skipped
                } else {
                    // The candidate window end and the *gain* over the stored window: a
                    // write happens only when the gain is at least the granularity, so a
                    // busy client writes at most once per [`TOUCH_GRANULARITY`] instead
                    // of per request, and any client polling at least that often stays
                    // alive. A client silent past its stored window end is expired
                    // before any touch can save it — correct, and it makes the idle
                    // bound honest.
                    let candidate = now + idle;
                    let gain = candidate - record.idle_expires;
                    if gain < granularity_secs() {
                        Touched::Skipped
                    } else {
                        // Never past absolute: the clamp makes a touch at the absolute
                        // boundary a no-op for the idle window, and `expired_at` for any
                        // later `now` still reads the stored absolute bound.
                        let clamped = candidate.min(absolute_expires);
                        record.idle_expires = clamped;
                        record.last_seen = now;
                        Touched::Extended
                    }
                }
            }
            None => Touched::Skipped,
        };
        Box::pin(async move { Ok(touched) })
    }

    fn rotate<'a>(&'a self, id: &'a str, new_digest: TokenDigest) -> SessionFuture<'a, ()> {
        let mut state = self.lock();
        let new_key = digest_key(&new_digest);
        let outcome = if state.by_digest.contains_key(&new_key) {
            Err(SessionError::StoreFailed(
                "rotate: the new digest is already in use".to_owned(),
            ))
        } else {
            // Two sequential borrows, never overlapping: first read the old digest's key
            // through the record; then (borrow closed) update both maps in one borrow.
            let old_key = match state.by_id.get_mut(id) {
                Some(record) => digest_key(&record.token_digest),
                None => {
                    return Box::pin(async move {
                        Err(SessionError::StoreFailed(format!(
                            "rotate: no session {id}"
                        )))
                    });
                }
            };
            state.by_digest.remove(&old_key);
            let id_string = match state.by_id.get_mut(id) {
                Some(record) => {
                    // The swap is in place: the same session (same id, window, identity)
                    // continues under the new token; the old digest stops resolving the
                    // instant this lands. The state stays Active — rotation is not an
                    // end; the *old token* is what dies.
                    record.token_digest = new_digest;
                    record.id.clone()
                }
                // The id existed one line ago; the second borrow cannot miss. The match
                // keeps the compiler's flow analysis simple in the panicky lock guard.
                None => String::new(),
            };
            if !id_string.is_empty() {
                state.by_digest.insert(new_key, id_string);
            }
            Ok(())
        };
        Box::pin(async move { outcome })
    }

    fn revoke<'a>(&'a self, id: &'a str, reason: String) -> SessionFuture<'a, RevokeReport> {
        let mut state = self.lock();
        let report = match state.by_id.get_mut(id) {
            Some(record) if record.is_revoked() => {
                // Idempotent: the first reason stays (who revoked first is the audit fact).
                RevokeReport { revoked: 0 }
            }
            Some(record) => {
                record.state = SessionState::Revoked {
                    reason: SessionEndReason::Other(reason),
                };
                RevokeReport { revoked: 1 }
            }
            None => RevokeReport { revoked: 0 },
        };
        Box::pin(async { Ok(report) })
    }

    fn revoke_all_for_user<'a>(
        &'a self,
        user: &'a UserKey,
        except: Option<&'a str>,
        reason: String,
    ) -> SessionFuture<'a, RevokeReport> {
        let mut state = self.lock();
        let mut revoked = 0;
        let ids: Vec<String> = state
            .by_id
            .values()
            .filter(|record| record.user == *user && Some(record.id.as_str()) != except)
            .map(|record| record.id.clone())
            .collect();
        for id in ids {
            if let Some(record) = state.by_id.get_mut(&id) {
                if record.is_revoked() {
                    continue;
                }
                record.state = SessionState::Revoked {
                    reason: SessionEndReason::Other(reason.clone()),
                };
                revoked += 1;
            }
        }
        Box::pin(async move { Ok(RevokeReport { revoked }) })
    }

    fn list_for_user<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SessionRecord>> {
        let state = self.lock();
        let mut listed: Vec<SessionRecord> = state
            .by_id
            .values()
            .filter(|record| record.user == *user)
            .cloned()
            .collect();
        // Creation order: `created`, then id for determinism.
        listed.sort_by(|left, right| (left.created, &left.id).cmp(&(right.created, &right.id)));
        Box::pin(async { Ok(listed) })
    }

    fn purge_expired<'a>(&'a self, now: i64, limit: usize) -> SessionFuture<'a, usize> {
        let mut state = self.lock();
        let expired_ids: Vec<String> = state
            .by_id
            .values()
            .filter(|record| record.expired_at(now))
            .map(|record| record.id.clone())
            .take(limit)
            .collect();
        for id in &expired_ids {
            if let Some(record) = state.by_id.remove(id) {
                state.by_digest.remove(&digest_key(&record.token_digest));
            }
        }
        Box::pin(async move { Ok(expired_ids.len()) })
    }

    fn oldest_active<'a>(
        &'a self,
        user: &'a UserKey,
        now: i64,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        let state = self.lock();
        let found: Option<SessionRecord> = state
            .by_id
            .values()
            .filter(|record| record.user == *user && record.alive_at(now))
            .cloned()
            .min_by_key(|record| (record.created, record.id.clone()));
        Box::pin(async { Ok(found) })
    }

    fn exists<'a>(&'a self, digest: &'a TokenDigest) -> SessionFuture<'a, bool> {
        let state = self.lock();
        let present = state.by_digest.contains_key(&digest_key(digest));
        Box::pin(async move { Ok(present) })
    }

    fn active_sources<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SourceHash>> {
        let state = self.lock();
        let sources: Vec<SourceHash> = state
            .by_id
            .values()
            .filter(|record| record.user == *user && !record.is_revoked())
            .map(|record| record.source.clone())
            .collect();
        Box::pin(async { Ok(sources) })
    }
}

/// The digest's map key: its bytes (`TokenDigest` is not `Hash` because it is
/// secret-class material; the raw bytes are the storage key by design).
fn digest_key(digest: &TokenDigest) -> [u8; 32] {
    *digest.as_bytes()
}

/// [`TOUCH_GRANULARITY`] in whole seconds (the store works in unix seconds).
fn granularity_secs() -> i64 {
    // A granularity above i64::MAX seconds cannot happen for a constant of 60.
    i64::try_from(TOUCH_GRANULARITY.as_secs()).unwrap_or(i64::MAX)
}
