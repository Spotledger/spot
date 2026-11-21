//! The [`ReplayStore`] contract suite (feature `testing`).
//!
//! Every replay store, the in-memory one here and the PostgreSQL one (WP-IAM-05.T6), must pass
//! [`run_all`]. The suite is synchronous: it drives the store's futures with [`block_on`], so
//! it needs no async runtime. [`FailingStore`] is a store that is always down, for proving a
//! verifier fails closed.

use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

use crate::replay::{Claim, ReplayError, ReplayStore, StoreFuture, namespaced};

struct ThreadWaker(thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs a future to completion on the calling thread. For tests: it parks the thread between
/// polls, so a store that really waits on I/O still works.
pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}

/// A store that is always down: every claim is a [`ReplayError`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FailingStore;

impl ReplayStore for FailingStore {
    fn claim<'a>(
        &'a self,
        _id: &'a str,
        _now: i64,
        _expires_at: i64,
    ) -> StoreFuture<'a, Result<Claim, ReplayError>> {
        Box::pin(std::future::ready(Err(ReplayError::unavailable())))
    }
}

/// Runs every contract case against stores built by `make`, which must return a fresh, empty
/// store (sharing nothing with earlier ones) on each call.
///
/// # Panics
///
/// When the store breaks the contract, naming the case.
pub fn run_all<S, F>(make: F)
where
    S: ReplayStore + 'static,
    F: Fn() -> S,
{
    fresh_then_replayed(&make());
    replay_after_expiry_is_fresh_again(&make());
    a_replay_does_not_extend_the_record(&make());
    concurrent_claims_give_one_fresh(Arc::new(make()));
    namespaces_never_collide(&make());
}

fn claim(store: &impl ReplayStore, id: &str, now: i64, expires_at: i64) -> Claim {
    match block_on(store.claim(id, now, expires_at)) {
        Ok(claim) => claim,
        Err(error) => panic!("the store failed a claim: {error}"),
    }
}

/// An id is fresh once, then a replay.
fn fresh_then_replayed(store: &impl ReplayStore) {
    assert_eq!(claim(store, "id-1", 100, 200), Claim::Fresh, "first claim");
    assert_eq!(
        claim(store, "id-1", 100, 200),
        Claim::Replayed,
        "same instant"
    );
    assert_eq!(
        claim(store, "id-1", 199, 400),
        Claim::Replayed,
        "just before expiry"
    );
    assert_eq!(
        claim(store, "id-2", 100, 200),
        Claim::Fresh,
        "another id is independent"
    );
}

/// At and after `expires_at` the id is forgotten and fresh again.
fn replay_after_expiry_is_fresh_again(store: &impl ReplayStore) {
    assert_eq!(claim(store, "id-1", 100, 200), Claim::Fresh);
    assert_eq!(
        claim(store, "id-1", 200, 300),
        Claim::Fresh,
        "at expires_at"
    );
    assert_eq!(
        claim(store, "id-1", 250, 300),
        Claim::Replayed,
        "the new record is live"
    );
    assert_eq!(
        claim(store, "id-1", 1_000, 1_100),
        Claim::Fresh,
        "long after"
    );
}

/// A refused replay must not push the record's expiry out (it would keep an id alive forever).
fn a_replay_does_not_extend_the_record(store: &impl ReplayStore) {
    assert_eq!(claim(store, "id-1", 100, 200), Claim::Fresh);
    assert_eq!(claim(store, "id-1", 150, 10_000), Claim::Replayed);
    assert_eq!(
        claim(store, "id-1", 200, 300),
        Claim::Fresh,
        "expiry stayed at 200"
    );
}

/// Many threads claim one id at once: exactly one wins.
fn concurrent_claims_give_one_fresh<S: ReplayStore + 'static>(store: Arc<S>) {
    const THREADS: usize = 16;
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let store = Arc::clone(&store);
            thread::spawn(move || block_on(store.claim("contended", 100, 200)))
        })
        .collect();
    let mut fresh = 0;
    for handle in handles {
        match handle.join() {
            Ok(Ok(Claim::Fresh)) => fresh += 1,
            Ok(Ok(Claim::Replayed)) => {}
            Ok(Err(error)) => panic!("the store failed a claim: {error}"),
            Err(_) => panic!("a claiming thread panicked"),
        }
    }
    assert_eq!(fresh, 1, "exactly one concurrent claim is fresh");
}

/// The same raw id under another kind or provider is a different key.
fn namespaces_never_collide(store: &impl ReplayStore) {
    let oidc = namespaced("oidc-logout", "okta", "abc");
    let saml = namespaced("saml-assertion", "okta", "abc");
    let other = namespaced("oidc-logout", "entra", "abc");
    assert_eq!(claim(store, &oidc, 100, 200), Claim::Fresh);
    assert_eq!(claim(store, &saml, 100, 200), Claim::Fresh, "another kind");
    assert_eq!(
        claim(store, &other, 100, 200),
        Claim::Fresh,
        "another provider"
    );
    assert_eq!(
        claim(store, &oidc, 100, 200),
        Claim::Replayed,
        "the first is still held"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::MemoryReplayStore;

    #[test]
    fn the_memory_store_passes_the_contract() {
        run_all(MemoryReplayStore::new);
    }

    #[test]
    fn a_failing_store_reports_the_stable_code() {
        let error = block_on(FailingStore.claim("x", 0, 1));
        assert_eq!(
            error.map_err(|e| e.code()),
            Err("sso.replay_store_unavailable")
        );
    }
}
