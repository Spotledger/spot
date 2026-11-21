//! `Idempotency-Key` handling: the `IdempotencyStore` port and an in-memory implementation
//! (24 h TTL timed by `spot_core::Clock`), per ADR-055 and ADR-082 (class R).
//!
//! # The port is async and fail-closed (WP-PLT-05.T7)
//!
//! Every method that can be answered wrongly when the store is down is fallible: `reserve`,
//! `complete`, `wait_pending` and `prune` return [`IdempotencyError`]. The router answers
//! `503 server.unavailable` with `Retry-After` **before the command runs** when `reserve` or
//! `wait_pending` fail, so a request that carries an `Idempotency-Key` is never executed
//! without its reservation. [`IdempotencyStore::fail`] is best effort and never an error: the
//! durable store's reservation lease covers a release that did not arrive.
//!
//! A `complete` that fails **after** the command ran is logged by the router and the real
//! response is still returned. That is the one at-least-once edge of this protocol: a retry
//! then finds a pending row that expires with its lease, never a stored lie. The engine-level
//! command id (`Invocation::command_id` = the key, ADR-055, WP-DOC-04.T8) is the backstop for
//! the commands that reach the engine.
//!
//! The durable implementation is `spot-idempotency-pg`. [`MemoryIdempotencyStore`] stays as the
//! test double and for a one-process host; it never returns an error.
//!
//! Concurrency decision (the brief asks for one and a documented choice): **the second
//! request with the same in-flight key waits for the first to finish** and then replays its
//! stored response (or reports the conflict the first one revealed). A client that timed out
//! and retried while the original is still running gets the replay as soon as the original
//! completes, which is what an idempotency key is for; answering `409` to the retry would
//! push the client into a retry loop against its own in-flight request. A crashed handler
//! loses the in-flight entry (its TTL prunes a stale `Pending` eventually); the command-id
//! dedupe the outbox carries (WP-CON-04, ADR-053) is the durable backstop.
//!
//! Entry lifetime is measured in `Clock::monotonic()` intervals, so `ManualClock` can expire
//! entries in tests and wall-clock jumps cannot stretch or shrink the window.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use spot_core::Clock;
use tokio::sync::Notify;

/// The response a stored key replays: status, headers and body bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredResponse {
    /// The HTTP status the original produced.
    pub status: u16,
    /// Response headers to replay. `x-request-id` and `date` are excluded — the live request
    /// stamps its own id.
    pub headers: Vec<(String, String)>,
    /// The exact body bytes of the original response.
    pub body: Vec<u8>,
}

/// The outcome of reserving a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reservation {
    /// No live entry: the caller runs the request and must `complete` the key.
    Fresh,
    /// A completed entry with the same fingerprint exists; replay it.
    Completed(StoredResponse),
    /// A different body used this key: `409 api.idempotency_conflict`.
    Conflict,
    /// The same key and fingerprint is still running: wait on `wait_pending`, then reserve
    /// again.
    Pending,
}

/// The identity a key is scoped by: site and credential, never across them (ADR-055).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyScope {
    /// The site the key belongs to.
    pub site: String,
    /// A stable identity for the credential (the router hashes the bearer token), so two
    /// callers never see each other's keys.
    pub credential: String,
    /// The `Idempotency-Key` header value.
    pub key: String,
}

/// The boxed future of an [`IdempotencyStore`] method: object-safe, so the router holds an
/// `Arc<dyn IdempotencyStore>` (the shape `spot-event-stream` uses, ADR-082 section 3).
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The store cannot vouch for a key right now. The router refuses the request with
/// `503 server.unavailable` and `Retry-After`; it never runs the command unreserved.
///
/// The text is fixed: no driver message, SQLSTATE, key or fingerprint is ever carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdempotencyError {
    /// The store is unreachable, timed out, or ran out of wait budget.
    Unavailable {
        /// Seconds the client should wait before retrying (at least 1 once rendered).
        retry_after_secs: u64,
    },
}

impl IdempotencyError {
    /// An outage with a one-second retry hint.
    #[must_use]
    pub const fn unavailable() -> Self {
        IdempotencyError::Unavailable {
            retry_after_secs: 1,
        }
    }

    /// Seconds a client should wait before retrying.
    #[must_use]
    pub const fn retry_after_secs(&self) -> u64 {
        match self {
            IdempotencyError::Unavailable { retry_after_secs } => *retry_after_secs,
        }
    }
}

impl std::fmt::Display for IdempotencyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the idempotency store is unavailable")
    }
}

impl std::error::Error for IdempotencyError {}

/// The largest response body a store keeps by default (256 KiB). A larger response is not
/// stored: the reservation is released and a retry runs again.
pub const DEFAULT_MAX_BODY_BYTES: usize = 256 * 1024;

/// Storage of idempotency records, shared across requests (and, for a durable store, across
/// nodes and restarts).
///
/// A wait is resolved by **re-reading the store**, never by an in-process wakeup the store
/// alone could lose (ADR-082 section 3); the in-memory double may use one because it is the
/// only reader.
pub trait IdempotencyStore: Send + Sync + 'static {
    /// Reserve `scope` for `fingerprint`.
    ///
    /// # Errors
    /// [`IdempotencyError::Unavailable`] when the store cannot decide; the key is then not
    /// reserved and the caller must not run the request.
    fn reserve<'a>(
        &'a self,
        scope: &'a KeyScope,
        fingerprint: &'a str,
    ) -> StoreFuture<'a, Result<Reservation, IdempotencyError>>;

    /// Store the response for a key this caller reserved with `fingerprint`. `class` names a
    /// retention class the store was configured with; the router passes a constant per route
    /// group, never a value taken from a request. A key that is not pending under that
    /// fingerprint is left alone (a no-op).
    ///
    /// # Errors
    /// [`IdempotencyError::Unavailable`] when the response could not be written.
    fn complete<'a>(
        &'a self,
        scope: &'a KeyScope,
        fingerprint: &'a str,
        response: StoredResponse,
        class: &'a str,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>>;

    /// Release a reservation after a failure produced no storable response. Best effort and
    /// never an error: a durable store's lease frees a key whose release did not arrive. Only
    /// a pending entry with this fingerprint is released, never a stored response.
    fn fail<'a>(&'a self, scope: &'a KeyScope, fingerprint: &'a str) -> StoreFuture<'a, ()>;

    /// Wait until the pending entry for `scope` is resolved (completed or released). Returns
    /// immediately when there is no pending entry.
    ///
    /// # Errors
    /// [`IdempotencyError::Unavailable`] when the store cannot be read, or its wait budget ran
    /// out while the entry was still pending. The duplicate is then answered 503 and is never
    /// run a second time.
    fn wait_pending<'a>(
        &'a self,
        scope: &'a KeyScope,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>>;

    /// Delete at most `limit` expired entries and return how many were deleted. The default
    /// does nothing (the in-memory store prunes inside `reserve`); a durable store runs no
    /// timer of its own, the host calls this.
    ///
    /// # Errors
    /// [`IdempotencyError::Unavailable`] when the store cannot be written.
    fn prune<'a>(&'a self, limit: u32) -> StoreFuture<'a, Result<u64, IdempotencyError>> {
        let _ = limit;
        Box::pin(std::future::ready(Ok(0)))
    }

    /// The largest response body this store keeps. The router stores nothing larger (the
    /// reservation is released instead).
    fn max_body_bytes(&self) -> usize {
        DEFAULT_MAX_BODY_BYTES
    }
}

/// The body fingerprint: SHA-256 over method, path and body (ADR-055, the brief).
pub fn fingerprint(method: &str, path: &str, body: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update(b"\n");
    hasher.update(path.as_bytes());
    hasher.update(b"\n");
    hasher.update(body);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        // `write!` into a String cannot fail for a hex pair.
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// The 24-hour contract window.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

enum Entry {
    /// Running. A `Pending` older than the TTL is considered orphaned and pruned.
    Pending {
        fingerprint: String,
        notify: Arc<Notify>,
        started: Duration,
    },
    /// Done; pruned when `expires` (monotonic) passes.
    Done {
        fingerprint: String,
        response: StoredResponse,
        expires: Duration,
        notify: Arc<Notify>,
    },
}

impl Entry {
    fn fingerprint(&self) -> &str {
        match self {
            Entry::Pending { fingerprint, .. } | Entry::Done { fingerprint, .. } => fingerprint,
        }
    }

    fn notify(&self) -> &Arc<Notify> {
        match self {
            Entry::Pending { notify, .. } | Entry::Done { notify, .. } => notify,
        }
    }
}

type EntryId = (String, String, String);

/// The in-memory store: a map behind a mutex, timed by a `Clock`.
pub struct MemoryIdempotencyStore {
    clock: Arc<dyn Clock>,
    ttl: Duration,
    inner: std::sync::Mutex<HashMap<EntryId, Entry>>,
}

impl std::fmt::Debug for MemoryIdempotencyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self.inner.lock().unwrap_or_else(|p| p.into_inner()).len();
        f.debug_struct("MemoryIdempotencyStore")
            .field("ttl_secs", &self.ttl.as_secs())
            .field("entries", &count)
            .finish()
    }
}

impl MemoryIdempotencyStore {
    /// A store whose completed entries live 24 hours, timed by `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_ttl(clock, DEFAULT_TTL)
    }

    /// A store with an explicit TTL (tests shrink it; production uses `new`).
    pub fn with_ttl(clock: Arc<dyn Clock>, ttl: Duration) -> Self {
        MemoryIdempotencyStore {
            clock,
            ttl,
            inner: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EntryId, Entry>> {
        // The mutex guards a plain map; a panic elsewhere cannot leave it inconsistent.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Live (unexpired) entries; tests assert TTL eviction through this.
    pub fn live_entries(&self) -> usize {
        let now = self.clock.monotonic();
        let mut map = self.lock();
        map.retain(|_, entry| match entry {
            Entry::Done { expires, .. } => *expires > now,
            Entry::Pending { started, .. } => now - *started < self.ttl,
        });
        map.len()
    }

    fn reserve_now(&self, scope: &KeyScope, fingerprint: &str) -> Reservation {
        let now = self.clock.monotonic();
        let mut map = self.lock();
        map.retain(|_, entry| match entry {
            Entry::Done { expires, .. } => *expires > now,
            Entry::Pending { started, .. } => now - *started < self.ttl,
        });
        let id = (
            scope.site.clone(),
            scope.credential.clone(),
            scope.key.clone(),
        );
        match map.get(&id) {
            None => {
                map.insert(
                    id,
                    Entry::Pending {
                        fingerprint: fingerprint.to_owned(),
                        notify: Arc::new(Notify::new()),
                        started: now,
                    },
                );
                Reservation::Fresh
            }
            Some(entry) if entry.fingerprint() != fingerprint => Reservation::Conflict,
            Some(entry) => match entry {
                Entry::Pending {
                    notify, started, ..
                } => {
                    let notify = Arc::clone(notify);
                    let started = *started;
                    if now - started >= self.ttl {
                        // Orphaned reservation: take it over.
                        map.insert(
                            id,
                            Entry::Pending {
                                fingerprint: fingerprint.to_owned(),
                                notify,
                                started: now,
                            },
                        );
                        Reservation::Fresh
                    } else {
                        Reservation::Pending
                    }
                }
                Entry::Done { response, .. } => Reservation::Completed(response.clone()),
            },
        }
    }
}

impl IdempotencyStore for MemoryIdempotencyStore {
    fn reserve<'a>(
        &'a self,
        scope: &'a KeyScope,
        fingerprint: &'a str,
    ) -> StoreFuture<'a, Result<Reservation, IdempotencyError>> {
        Box::pin(async move { Ok(self.reserve_now(scope, fingerprint)) })
    }

    /// The memory store has one retention (its TTL) and ignores `class`.
    fn complete<'a>(
        &'a self,
        scope: &'a KeyScope,
        fingerprint: &'a str,
        response: StoredResponse,
        _class: &'a str,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        Box::pin(async move {
            let now = self.clock.monotonic();
            let mut map = self.lock();
            let id = (
                scope.site.clone(),
                scope.credential.clone(),
                scope.key.clone(),
            );
            let notify = match map.get(&id) {
                Some(Entry::Pending {
                    fingerprint: held,
                    notify,
                    ..
                }) if held == fingerprint => Arc::clone(notify),
                // Not pending under this fingerprint: nothing to complete.
                _ => return Ok(()),
            };
            map.insert(
                id,
                Entry::Done {
                    fingerprint: fingerprint.to_owned(),
                    response,
                    expires: now + self.ttl,
                    notify: Arc::clone(&notify),
                },
            );
            notify.notify_waiters();
            Ok(())
        })
    }

    fn fail<'a>(&'a self, scope: &'a KeyScope, fingerprint: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut map = self.lock();
            let id = (
                scope.site.clone(),
                scope.credential.clone(),
                scope.key.clone(),
            );
            let releasable = matches!(
                map.get(&id),
                Some(Entry::Pending { fingerprint: held, .. }) if held == fingerprint
            );
            if releasable && let Some(entry) = map.remove(&id) {
                entry.notify().notify_waiters();
            }
        })
    }

    fn wait_pending<'a>(
        &'a self,
        scope: &'a KeyScope,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        Box::pin(async move {
            let id = (
                scope.site.clone(),
                scope.credential.clone(),
                scope.key.clone(),
            );
            let notify = match self.lock().get(&id) {
                Some(Entry::Pending { notify, .. }) => Arc::clone(notify),
                _ => return Ok(()),
            };
            // Register for wakeups *before* re-checking the entry: `notify_waiters` stores no
            // permit, so a completion between the check and the first poll would otherwise be
            // lost and this waiter would hang.
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !matches!(self.lock().get(&id), Some(Entry::Pending { .. })) {
                return Ok(());
            }
            notified.await;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spot_core::ManualClock;
    use time::OffsetDateTime;

    fn scope(key: &str) -> KeyScope {
        KeyScope {
            site: "acme".to_owned(),
            credential: "tok".to_owned(),
            key: key.to_owned(),
        }
    }

    fn store() -> (Arc<ManualClock>, MemoryIdempotencyStore) {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        let store = MemoryIdempotencyStore::with_ttl(clock.clone(), Duration::from_secs(20));
        (clock, store)
    }

    fn response(status: u16) -> StoredResponse {
        StoredResponse {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    async fn reserve(store: &MemoryIdempotencyStore, s: &KeyScope, fp: &str) -> Reservation {
        store.reserve(s, fp).await.unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn fingerprint_is_deterministic_and_method_bound() {
        let a = fingerprint("POST", "/api/v1/resource/x", b"{\"a\":1}");
        let b = fingerprint("POST", "/api/v1/resource/x", b"{\"a\":1}");
        let c = fingerprint("POST", "/api/v1/resource/y", b"{\"a\":1}");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64, "sha256 hex");
    }

    #[tokio::test]
    async fn fresh_then_complete_then_replay() {
        let (_clock, store) = store();
        let s = scope("k1");
        assert_eq!(reserve(&store, &s, "fp1").await, Reservation::Fresh);
        let stored = StoredResponse {
            status: 201,
            headers: vec![("location".to_owned(), "/api/v1/resource/x/1".to_owned())],
            body: b"{}".to_vec(),
        };
        assert_eq!(store.complete(&s, "fp1", stored, "http_api").await, Ok(()));
        match reserve(&store, &s, "fp1").await {
            Reservation::Completed(r) => {
                assert_eq!(r.status, 201);
                assert_eq!(r.headers[0].1, "/api/v1/resource/x/1");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn different_fingerprint_conflicts() {
        let (_clock, store) = store();
        let s = scope("k2");
        reserve(&store, &s, "fp1").await;
        assert_eq!(reserve(&store, &s, "fp2").await, Reservation::Conflict);
    }

    #[tokio::test]
    async fn scopes_separate_keys() {
        let (_clock, store) = store();
        let a = scope("same");
        let mut b = scope("same");
        b.credential = "other".to_owned();
        let mut c = scope("same");
        c.site = "other-site".to_owned();
        assert_eq!(reserve(&store, &a, "fp1").await, Reservation::Fresh);
        assert_eq!(reserve(&store, &b, "fp1").await, Reservation::Fresh);
        assert_eq!(reserve(&store, &c, "fp1").await, Reservation::Fresh);
        assert_eq!(reserve(&store, &a, "fp2").await, Reservation::Conflict);
    }

    #[tokio::test]
    async fn ttl_expiry_evicts_completed_entries() {
        let (clock, store) = store();
        let s = scope("k3");
        reserve(&store, &s, "fp1").await;
        let _ = store.complete(&s, "fp1", response(200), "http_api").await;
        assert_eq!(store.live_entries(), 1);
        clock.advance(Duration::from_secs(21));
        assert_eq!(store.live_entries(), 0);
        assert_eq!(reserve(&store, &s, "fp1").await, Reservation::Fresh);
    }

    #[tokio::test]
    async fn pending_release_allows_fresh() {
        let (_clock, store) = store();
        let s = scope("k4");
        reserve(&store, &s, "fp1").await;
        assert_eq!(reserve(&store, &s, "fp1").await, Reservation::Pending);
        store.fail(&s, "fp1").await;
        assert_eq!(reserve(&store, &s, "fp1").await, Reservation::Fresh);
    }

    #[tokio::test]
    async fn a_release_or_completion_that_is_not_ours_is_a_no_op() {
        let (_clock, store) = store();
        let s = scope("k7");
        // Nothing pending: both are no-ops.
        store.fail(&s, "fp1").await;
        assert_eq!(
            store.complete(&s, "fp1", response(200), "http_api").await,
            Ok(())
        );
        assert_eq!(reserve(&store, &s, "fp1").await, Reservation::Fresh);
        // Pending under fp1: a release under another fingerprint leaves it pending.
        store.fail(&s, "fp-other").await;
        assert_eq!(reserve(&store, &s, "fp1").await, Reservation::Pending);
        let _ = store.complete(&s, "fp1", response(200), "http_api").await;
        // A done entry is never released by `fail`.
        store.fail(&s, "fp1").await;
        assert!(matches!(
            reserve(&store, &s, "fp1").await,
            Reservation::Completed(_)
        ));
    }

    #[tokio::test]
    async fn waiter_wakes_when_completion_races_the_first_poll() {
        let (_clock, store) = store();
        let s = scope("k5");
        assert_eq!(reserve(&store, &s, "fp1").await, Reservation::Fresh);
        // Created but not yet polled; completion lands before the first poll.
        let wait = store.wait_pending(&s);
        let _ = store.complete(&s, "fp1", response(200), "http_api").await;
        let woke = tokio::time::timeout(Duration::from_secs(1), wait).await;
        assert!(woke.is_ok(), "waiter must not hang");
    }

    #[tokio::test]
    async fn waiter_wakes_on_release() {
        let (_clock, store) = store();
        let store = Arc::new(store);
        let s = scope("k6");
        reserve(&store, &s, "fp1").await;
        let waiter = {
            let store = Arc::clone(&store);
            let s = s.clone();
            tokio::spawn(async move { store.wait_pending(&s).await })
        };
        tokio::task::yield_now().await;
        store.fail(&s, "fp1").await;
        let woke = tokio::time::timeout(Duration::from_secs(1), waiter).await;
        assert!(matches!(woke, Ok(Ok(Ok(())))), "waiter must wake");
    }

    #[test]
    fn the_error_text_is_fixed_and_carries_no_detail() {
        let error = IdempotencyError::Unavailable {
            retry_after_secs: 7,
        };
        assert_eq!(error.to_string(), "the idempotency store is unavailable");
        assert_eq!(format!("{error:?}"), "Unavailable { retry_after_secs: 7 }");
        assert_eq!(error.retry_after_secs(), 7);
    }
}
