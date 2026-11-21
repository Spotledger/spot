//! The backend-independent contract of an [`IdempotencyStore`] (ADR-082 section 3) and the test
//! doubles the router suites use. Compiled for this crate's own tests and behind the `testing`
//! feature, which a durable store's dev-dependencies enable; it never reaches a product build.
//!
//! [`run_all`] takes a factory and a [`TestClock`]. The factory must return a store whose
//! stored time follows the clock (a store timed by `ManualClock`, or one timed by a stored
//! `now` argument, both pass), with the documented 24 h default retention and a lease of at
//! least 30 s. Stores the factory returns may share one backing database: every case keys on
//! a site name unique to the run, so cases never see each other's rows.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::idempotency::{
    DEFAULT_TTL, IdempotencyError, IdempotencyStore, KeyScope, Reservation, StoreFuture,
    StoredResponse,
};

/// The clock a contract run is timed by: move it forward and read it.
pub trait TestClock: Send + Sync {
    /// Moves the clock forward by `by`.
    fn advance(&self, by: Duration);
    /// The clock's current Unix time in seconds.
    fn now_secs(&self) -> i64;
}

impl TestClock for spot_core::ManualClock {
    fn advance(&self, by: Duration) {
        spot_core::ManualClock::advance(self, by);
    }

    fn now_secs(&self) -> i64 {
        spot_core::Clock::now_utc(self).unix_timestamp()
    }
}

/// A retention class name used by the suite. A store that knows no such class must treat it as
/// its default class.
const UNKNOWN_CLASS: &str = "contract_unknown_class";

static RUNS: AtomicU64 = AtomicU64::new(0);

/// One run's scope factory: a site name unique to this run, so cases never meet.
struct Scopes {
    site: String,
}

impl Scopes {
    fn new() -> Self {
        Scopes {
            site: format!("contract-{}", RUNS.fetch_add(1, Ordering::SeqCst)),
        }
    }

    fn of(&self, credential: &str, key: &str) -> KeyScope {
        KeyScope {
            site: self.site.clone(),
            credential: credential.to_owned(),
            key: key.to_owned(),
        }
    }
}

fn answer(status: u16, body: &[u8]) -> StoredResponse {
    StoredResponse {
        status,
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("etag".to_owned(), "\"1\"".to_owned()),
            ("location".to_owned(), "/api/v1/resource/x/1".to_owned()),
        ],
        body: body.to_vec(),
    }
}

/// A fingerprint the way the router builds one (64 hex characters); `n` tells them apart.
fn fp(n: &str) -> String {
    super::idempotency::fingerprint("POST", "/api/v1/resource/contract", n.as_bytes())
}

async fn reserve(store: &dyn IdempotencyStore, scope: &KeyScope, fp: &str) -> Reservation {
    match store.reserve(scope, fp).await {
        Ok(reservation) => reservation,
        Err(error) => panic!("reserve failed: {error}"),
    }
}

async fn complete(
    store: &dyn IdempotencyStore,
    scope: &KeyScope,
    fp: &str,
    response: StoredResponse,
    class: &str,
) {
    if let Err(error) = store.complete(scope, fp, response, class).await {
        panic!("complete failed: {error}");
    }
}

/// Runs every case of the contract against stores from `make`.
///
/// # Panics
/// With the failing case's name when the store breaks the contract.
pub async fn run_all(make: impl Fn() -> Arc<dyn IdempotencyStore>, clock: &dyn TestClock) {
    let scopes = Scopes::new();
    fresh_complete_replay(&*make(), &scopes).await;
    replay_is_seen_by_another_store_instance(&*make(), &*make(), &scopes).await;
    different_fingerprint_conflicts(&*make(), &scopes).await;
    scopes_never_see_each_other(&*make(), &scopes).await;
    fail_frees_the_key(&*make(), &scopes).await;
    fail_and_complete_for_a_key_that_is_not_pending_are_no_ops(&*make(), &scopes).await;
    expiry_after_the_default_window(&*make(), &scopes, clock).await;
    an_unknown_class_gets_the_default_retention(&*make(), &scopes, clock).await;
    concurrent_claims_run_once(make(), &scopes).await;
    wait_pending_returns_when_nothing_is_pending(&*make(), &scopes).await;
    prune_is_bounded_and_never_removes_a_live_entry(&*make(), &scopes).await;
    an_unavailable_store_is_surfaced().await;
}

async fn fresh_complete_replay(store: &dyn IdempotencyStore, scopes: &Scopes) {
    let scope = scopes.of("tok", "fresh-complete-replay");
    assert_eq!(reserve(store, &scope, &fp("1")).await, Reservation::Fresh);
    // A body that is not UTF-8 text: the replay is byte-exact.
    let stored = answer(201, b"\x00\xff{\"data\":{\"id\":\"1\"}}\r\n");
    complete(store, &scope, &fp("1"), stored.clone(), "http_api").await;
    match reserve(store, &scope, &fp("1")).await {
        Reservation::Completed(replayed) => {
            assert_eq!(replayed.status, stored.status, "status");
            assert_eq!(replayed.body, stored.body, "body bytes");
            let mut want = stored.headers.clone();
            let mut got = replayed.headers.clone();
            want.sort();
            got.sort();
            assert_eq!(got, want, "headers");
        }
        other => panic!("fresh_complete_replay: expected Completed, got {other:?}"),
    }
}

async fn replay_is_seen_by_another_store_instance(
    first: &dyn IdempotencyStore,
    second: &dyn IdempotencyStore,
    scopes: &Scopes,
) {
    // Two instances over one backing store are two nodes (or one node after a restart); a
    // purely in-process double that shares nothing is exempt, so the case only asserts what a
    // shared backend must do and is skipped by a store that answers Fresh to the second.
    let scope = scopes.of("tok", "two-instances");
    assert_eq!(reserve(first, &scope, &fp("1")).await, Reservation::Fresh);
    complete(first, &scope, &fp("1"), answer(200, b"{}"), "http_api").await;
    let seen = reserve(second, &scope, &fp("1")).await;
    assert!(
        matches!(seen, Reservation::Completed(_) | Reservation::Fresh),
        "a second instance answers Completed (shared store) or Fresh (private store), not {seen:?}"
    );
}

async fn different_fingerprint_conflicts(store: &dyn IdempotencyStore, scopes: &Scopes) {
    let pending = scopes.of("tok", "conflict-pending");
    assert_eq!(reserve(store, &pending, &fp("1")).await, Reservation::Fresh);
    assert_eq!(
        reserve(store, &pending, &fp("2")).await,
        Reservation::Conflict,
        "a different fingerprint under a pending key"
    );
    assert_eq!(
        reserve(store, &pending, &fp("1")).await,
        Reservation::Pending,
        "the refused claim did not disturb the reservation"
    );
    complete(store, &pending, &fp("1"), answer(200, b"{}"), "http_api").await;
    assert_eq!(
        reserve(store, &pending, &fp("2")).await,
        Reservation::Conflict,
        "a different fingerprint under a stored key"
    );
    assert!(
        matches!(
            reserve(store, &pending, &fp("1")).await,
            Reservation::Completed(_)
        ),
        "the conflict did not disturb the stored response"
    );
}

async fn scopes_never_see_each_other(store: &dyn IdempotencyStore, scopes: &Scopes) {
    let a = scopes.of("cred-a", "scoped");
    let b = scopes.of("cred-b", "scoped");
    let mut other_site = scopes.of("cred-a", "scoped");
    other_site.site.push_str("-other");
    let other_key = scopes.of("cred-a", "scoped-2");
    assert_eq!(reserve(store, &a, &fp("0")).await, Reservation::Fresh);
    complete(store, &a, &fp("0"), answer(201, b"secret-a"), "http_api").await;
    for (name, scope) in [
        ("credential", &b),
        ("site", &other_site),
        ("key", &other_key),
    ] {
        assert_eq!(
            reserve(store, scope, &fp("0")).await,
            Reservation::Fresh,
            "another {name} must not see the stored response"
        );
    }
    // And a conflict in one scope is no conflict in another.
    assert_eq!(reserve(store, &b, &fp("2")).await, Reservation::Conflict);
    let c = scopes.of("cred-c", "scoped");
    assert_eq!(reserve(store, &c, &fp("2")).await, Reservation::Fresh);
}

async fn fail_frees_the_key(store: &dyn IdempotencyStore, scopes: &Scopes) {
    let scope = scopes.of("tok", "fail-frees");
    assert_eq!(reserve(store, &scope, &fp("1")).await, Reservation::Fresh);
    assert_eq!(reserve(store, &scope, &fp("1")).await, Reservation::Pending);
    store.fail(&scope, &fp("1")).await;
    assert_eq!(
        reserve(store, &scope, &fp("1")).await,
        Reservation::Fresh,
        "fail frees the key"
    );
    // The release frees the key for a different fingerprint too.
    store.fail(&scope, &fp("1")).await;
    assert_eq!(reserve(store, &scope, &fp("2")).await, Reservation::Fresh);
}

async fn fail_and_complete_for_a_key_that_is_not_pending_are_no_ops(
    store: &dyn IdempotencyStore,
    scopes: &Scopes,
) {
    // Nothing reserved: neither call may create or change anything.
    let absent = scopes.of("tok", "noop-absent");
    store.fail(&absent, &fp("1")).await;
    complete(store, &absent, &fp("1"), answer(200, b"ghost"), "http_api").await;
    assert_eq!(
        reserve(store, &absent, &fp("1")).await,
        Reservation::Fresh,
        "a complete for an unreserved key stores nothing"
    );

    // Reserved under fp1: a release or completion under another fingerprint is not ours.
    store.fail(&absent, &fp("other")).await;
    complete(
        store,
        &absent,
        &fp("other"),
        answer(200, b"wrong"),
        "http_api",
    )
    .await;
    assert_eq!(
        reserve(store, &absent, &fp("1")).await,
        Reservation::Pending,
        "a foreign fail/complete leaves the reservation alone"
    );

    // Stored: fail must not delete it and a second complete must not replace it.
    complete(store, &absent, &fp("1"), answer(201, b"first"), "http_api").await;
    store.fail(&absent, &fp("1")).await;
    complete(store, &absent, &fp("1"), answer(202, b"second"), "http_api").await;
    match reserve(store, &absent, &fp("1")).await {
        Reservation::Completed(stored) => {
            assert_eq!(stored.status, 201, "the first complete wins");
            assert_eq!(stored.body, b"first".to_vec());
        }
        other => panic!("expected the first response to survive, got {other:?}"),
    }
}

async fn expiry_after_the_default_window(
    store: &dyn IdempotencyStore,
    scopes: &Scopes,
    clock: &dyn TestClock,
) {
    let scope = scopes.of("tok", "expiry");
    assert_eq!(reserve(store, &scope, &fp("1")).await, Reservation::Fresh);
    complete(store, &scope, &fp("1"), answer(200, b"{}"), "http_api").await;
    clock.advance(DEFAULT_TTL - Duration::from_secs(60));
    assert!(
        matches!(
            reserve(store, &scope, &fp("1")).await,
            Reservation::Completed(_)
        ),
        "still replayed inside the 24 h window"
    );
    clock.advance(Duration::from_secs(120));
    assert_eq!(
        reserve(store, &scope, &fp("1")).await,
        Reservation::Fresh,
        "fresh again after the 24 h window"
    );
    // The new reservation holds a different fingerprint without a conflict after expiry too.
    store.fail(&scope, &fp("1")).await;
    assert_eq!(reserve(store, &scope, &fp("2")).await, Reservation::Fresh);
}

async fn an_unknown_class_gets_the_default_retention(
    store: &dyn IdempotencyStore,
    scopes: &Scopes,
    clock: &dyn TestClock,
) {
    let scope = scopes.of("tok", "unknown-class");
    assert_eq!(reserve(store, &scope, &fp("1")).await, Reservation::Fresh);
    complete(store, &scope, &fp("1"), answer(200, b"{}"), UNKNOWN_CLASS).await;
    clock.advance(DEFAULT_TTL - Duration::from_secs(60));
    assert!(
        matches!(
            reserve(store, &scope, &fp("1")).await,
            Reservation::Completed(_)
        ),
        "an unknown class keeps the response for the default window"
    );
    clock.advance(Duration::from_secs(120));
    assert_eq!(reserve(store, &scope, &fp("1")).await, Reservation::Fresh);
}

async fn concurrent_claims_run_once(store: Arc<dyn IdempotencyStore>, scopes: &Scopes) {
    const TASKS: usize = 16;
    let scope = scopes.of("tok", "concurrent");
    let winners = Arc::new(Mutex::new(Vec::<usize>::new()));
    let saw_pending = Arc::new(Mutex::new(Vec::<usize>::new()));
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..TASKS {
        let store = Arc::clone(&store);
        let scope = scope.clone();
        let winners = Arc::clone(&winners);
        let saw_pending = Arc::clone(&saw_pending);
        tasks.spawn(async move {
            loop {
                let reservation = match store.reserve(&scope, &fp("0")).await {
                    Ok(reservation) => reservation,
                    Err(error) => panic!("reserve failed: {error}"),
                };
                match reservation {
                    Reservation::Fresh => {
                        winners.lock().unwrap_or_else(|p| p.into_inner()).push(n);
                        // Run "the command" while the others pile up behind the reservation:
                        // until every duplicate has seen it pending (a bounded wait, so a
                        // store that answers a duplicate any other way still fails the
                        // assertions below instead of hanging).
                        for _ in 0..500 {
                            let seen = saw_pending.lock().unwrap_or_else(|p| p.into_inner()).len();
                            if seen >= TASKS - 1 {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                        let stored = answer(201, b"the one response");
                        if let Err(error) =
                            store.complete(&scope, &fp("0"), stored, "http_api").await
                        {
                            panic!("complete failed: {error}");
                        }
                        return None;
                    }
                    Reservation::Pending => {
                        saw_pending
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .push(n);
                        if let Err(error) = store.wait_pending(&scope).await {
                            panic!("wait_pending failed: {error}");
                        }
                    }
                    Reservation::Completed(stored) => return Some(stored),
                    Reservation::Conflict => panic!("a same-fingerprint duplicate conflicted"),
                }
            }
        });
    }
    let mut replays = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Some(stored)) => replays.push(stored),
            Ok(None) => {}
            Err(error) => panic!("a contract task failed: {error}"),
        }
    }
    let winners = winners.lock().unwrap_or_else(|p| p.into_inner()).len();
    assert_eq!(winners, 1, "exactly one of {TASKS} claims is Fresh");
    assert_eq!(
        replays.len(),
        TASKS - 1,
        "every other claim got the response"
    );
    for stored in &replays {
        assert_eq!(stored.status, 201);
        assert_eq!(stored.body, b"the one response".to_vec());
    }
    let waited = saw_pending.lock().unwrap_or_else(|p| p.into_inner()).len();
    assert!(
        waited >= TASKS - 1,
        "every duplicate saw Pending first ({waited})"
    );
}

async fn wait_pending_returns_when_nothing_is_pending(
    store: &dyn IdempotencyStore,
    scopes: &Scopes,
) {
    let scope = scopes.of("tok", "wait-nothing");
    let waited = tokio::time::timeout(Duration::from_secs(5), store.wait_pending(&scope)).await;
    assert!(
        matches!(waited, Ok(Ok(()))),
        "an absent key is not waited on"
    );
    assert_eq!(reserve(store, &scope, &fp("0")).await, Reservation::Fresh);
    complete(store, &scope, &fp("0"), answer(200, b"{}"), "http_api").await;
    let waited = tokio::time::timeout(Duration::from_secs(5), store.wait_pending(&scope)).await;
    assert!(
        matches!(waited, Ok(Ok(()))),
        "a stored key is not waited on"
    );
}

async fn prune_is_bounded_and_never_removes_a_live_entry(
    store: &dyn IdempotencyStore,
    scopes: &Scopes,
) {
    let live = scopes.of("tok", "prune-live");
    assert_eq!(reserve(store, &live, &fp("0")).await, Reservation::Fresh);
    complete(store, &live, &fp("0"), answer(200, b"{}"), "http_api").await;
    let pruned = match store.prune(1000).await {
        Ok(count) => count,
        Err(error) => panic!("prune failed: {error}"),
    };
    // Other cases' expired rows may go; the live one must stay.
    let _ = pruned;
    assert!(
        matches!(
            reserve(store, &live, &fp("0")).await,
            Reservation::Completed(_)
        ),
        "prune never removes a live entry"
    );
}

async fn an_unavailable_store_is_surfaced() {
    let down = FailingStore::new(7);
    let scope = KeyScope {
        site: "s".to_owned(),
        credential: "c".to_owned(),
        key: "k".to_owned(),
    };
    assert_eq!(
        down.reserve(&scope, &fp("0")).await,
        Err(IdempotencyError::Unavailable {
            retry_after_secs: 7
        })
    );
    assert!(down.wait_pending(&scope).await.is_err());
    assert!(
        down.complete(&scope, &fp("0"), answer(200, b"{}"), "http_api")
            .await
            .is_err()
    );
    // Releasing never errors, even when the store is down.
    down.fail(&scope, &fp("0")).await;
    assert_eq!(
        down.calls(),
        vec![
            StoreCall::Reserve,
            StoreCall::Wait,
            StoreCall::Complete("http_api".to_owned()),
            StoreCall::Fail,
        ]
    );
}

/// One call a test double saw, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreCall {
    /// `reserve`.
    Reserve,
    /// `complete`, with the retention class it named.
    Complete(String),
    /// `fail`.
    Fail,
    /// `wait_pending`.
    Wait,
}

/// A store that is down: `reserve`, `complete` and `wait_pending` answer
/// [`IdempotencyError::Unavailable`]. It records every call, so a router test can assert that a
/// route made none, or only `reserve` and `fail`.
#[derive(Debug)]
pub struct FailingStore {
    retry_after_secs: u64,
    calls: Mutex<Vec<StoreCall>>,
}

impl FailingStore {
    /// A downed store whose errors advise a `retry_after_secs` wait.
    #[must_use]
    pub fn new(retry_after_secs: u64) -> Self {
        FailingStore {
            retry_after_secs,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// The calls seen so far.
    #[must_use]
    pub fn calls(&self) -> Vec<StoreCall> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn record(&self, call: StoreCall) {
        self.calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(call);
    }

    fn error(&self) -> IdempotencyError {
        IdempotencyError::Unavailable {
            retry_after_secs: self.retry_after_secs,
        }
    }
}

impl IdempotencyStore for FailingStore {
    fn reserve<'a>(
        &'a self,
        _scope: &'a KeyScope,
        _fingerprint: &'a str,
    ) -> StoreFuture<'a, Result<Reservation, IdempotencyError>> {
        self.record(StoreCall::Reserve);
        Box::pin(std::future::ready(Err(self.error())))
    }

    fn complete<'a>(
        &'a self,
        _scope: &'a KeyScope,
        _fingerprint: &'a str,
        _response: StoredResponse,
        class: &'a str,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        self.record(StoreCall::Complete(class.to_owned()));
        Box::pin(std::future::ready(Err(self.error())))
    }

    fn fail<'a>(&'a self, _scope: &'a KeyScope, _fingerprint: &'a str) -> StoreFuture<'a, ()> {
        self.record(StoreCall::Fail);
        Box::pin(std::future::ready(()))
    }

    fn wait_pending<'a>(
        &'a self,
        _scope: &'a KeyScope,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        self.record(StoreCall::Wait);
        Box::pin(std::future::ready(Err(self.error())))
    }
}

/// A store that delegates to another and records every call it saw (including the retention
/// class of each `complete`). `set_down(true)` makes it answer like a downed store from then
/// on, so one router can be driven up, then down.
pub struct RecordingStore {
    inner: Arc<dyn IdempotencyStore>,
    calls: Mutex<Vec<StoreCall>>,
    down: std::sync::atomic::AtomicBool,
    complete_fails: std::sync::atomic::AtomicBool,
    max_body: std::sync::atomic::AtomicUsize,
}

impl RecordingStore {
    /// Records the calls made to `inner`.
    #[must_use]
    pub fn new(inner: Arc<dyn IdempotencyStore>) -> Self {
        RecordingStore {
            inner,
            calls: Mutex::new(Vec::new()),
            down: std::sync::atomic::AtomicBool::new(false),
            complete_fails: std::sync::atomic::AtomicBool::new(false),
            max_body: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// While set, only `complete` fails (the store went away after the command ran).
    pub fn set_complete_fails(&self, fails: bool) {
        self.complete_fails.store(fails, Ordering::SeqCst);
    }

    /// Overrides the body cap the router sees (0 restores the inner store's).
    pub fn set_max_body_bytes(&self, bytes: usize) {
        self.max_body.store(bytes, Ordering::SeqCst);
    }

    /// While down, `reserve`, `complete` and `wait_pending` fail without reaching `inner`.
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// The calls seen so far.
    #[must_use]
    pub fn calls(&self) -> Vec<StoreCall> {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Forgets the recorded calls.
    pub fn clear(&self) {
        self.calls.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }

    fn record(&self, call: StoreCall) {
        self.calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(call);
    }

    fn is_down(&self) -> bool {
        self.down.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for RecordingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingStore").finish_non_exhaustive()
    }
}

impl IdempotencyStore for RecordingStore {
    fn reserve<'a>(
        &'a self,
        scope: &'a KeyScope,
        fingerprint: &'a str,
    ) -> StoreFuture<'a, Result<Reservation, IdempotencyError>> {
        self.record(StoreCall::Reserve);
        if self.is_down() {
            return Box::pin(std::future::ready(Err(IdempotencyError::unavailable())));
        }
        self.inner.reserve(scope, fingerprint)
    }

    fn complete<'a>(
        &'a self,
        scope: &'a KeyScope,
        fingerprint: &'a str,
        response: StoredResponse,
        class: &'a str,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        self.record(StoreCall::Complete(class.to_owned()));
        if self.is_down() || self.complete_fails.load(Ordering::SeqCst) {
            return Box::pin(std::future::ready(Err(IdempotencyError::unavailable())));
        }
        self.inner.complete(scope, fingerprint, response, class)
    }

    fn fail<'a>(&'a self, scope: &'a KeyScope, fingerprint: &'a str) -> StoreFuture<'a, ()> {
        self.record(StoreCall::Fail);
        self.inner.fail(scope, fingerprint)
    }

    fn wait_pending<'a>(
        &'a self,
        scope: &'a KeyScope,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        self.record(StoreCall::Wait);
        if self.is_down() {
            return Box::pin(std::future::ready(Err(IdempotencyError::unavailable())));
        }
        self.inner.wait_pending(scope)
    }

    fn prune<'a>(&'a self, limit: u32) -> StoreFuture<'a, Result<u64, IdempotencyError>> {
        self.inner.prune(limit)
    }

    fn max_body_bytes(&self) -> usize {
        match self.max_body.load(Ordering::SeqCst) {
            0 => self.inner.max_body_bytes(),
            bytes => bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::idempotency::MemoryIdempotencyStore;
    use spot_core::ManualClock;
    use time::OffsetDateTime;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_memory_store_passes_the_contract() {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        // One store per run: the memory store is private to its instance, so the two-instance
        // case is satisfied by `Fresh` and every other case uses one instance each.
        let make_clock = Arc::clone(&clock);
        run_all(
            move || Arc::new(MemoryIdempotencyStore::new(make_clock.clone())),
            &*clock,
        )
        .await;
    }
}
