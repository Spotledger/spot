//! [`FlakyStore`]: a [`JournalStore`] wrapper that fails on command, for the later journal/sync
//! tasks' crash and retry tests.
//!
//! Every rule is deterministic: a call count, a key prefix, or a byte threshold, never the wall
//! clock or an OS random source. `seed` drives [`FlakyStore::fail_rate`] (a seeded, reproducible
//! "sometimes fails" rule) through a fixed splitmix64 step, so two runs with the same seed and
//! the same call sequence fail on exactly the same calls.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use spot_journal_store::{
    JournalStore, ObjectEntry, ObjectMeta, StoreError, StoreFuture, StoredObject,
};

/// Wraps a [`JournalStore`] and fails calls according to its configuration.
pub struct FlakyStore<S> {
    inner: S,
    seed: u64,
    calls: AtomicU64,
    fail_next: AtomicUsize,
    fail_prefix: Option<String>,
    latency: Duration,
    fail_after_bytes: Option<u64>,
    fail_rate: Option<(u32, u32)>,
}

impl<S: JournalStore> FlakyStore<S> {
    /// Wraps `inner` with no failure configured yet: every call succeeds until a `with_*`
    /// builder is applied. `seed` is the only input [`FlakyStore::fail_rate`] reads; it is never
    /// combined with the time or an OS random source.
    #[must_use]
    pub fn new(inner: S, seed: u64) -> Self {
        Self {
            inner,
            seed,
            calls: AtomicU64::new(0),
            fail_next: AtomicUsize::new(0),
            fail_prefix: None,
            latency: Duration::ZERO,
            fail_after_bytes: None,
            fail_rate: None,
        }
    }

    /// Fails the next `n` calls (of any method), then returns to normal.
    #[must_use]
    pub fn fail_next(self, n: usize) -> Self {
        self.fail_next.store(n, Ordering::SeqCst);
        self
    }

    /// Fails every call whose key starts with `prefix`.
    #[must_use]
    pub fn fail_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.fail_prefix = Some(prefix.into());
        self
    }

    /// Adds a fixed delay before every call (a timer wait, not a wall-clock read).
    #[must_use]
    pub fn latency(mut self, delay: Duration) -> Self {
        self.latency = delay;
        self
    }

    /// Fails a `put_new` or `put_replace` whose payload is larger than `bytes`, modelling a
    /// backend that cannot take an object past a size limit.
    #[must_use]
    pub fn fail_after_bytes(mut self, bytes: u64) -> Self {
        self.fail_after_bytes = Some(bytes);
        self
    }

    /// Fails roughly `numerator` calls out of every `denominator`, deterministically from
    /// `seed` and the call's sequence number: the same seed and the same call sequence always
    /// fail on exactly the same calls.
    #[must_use]
    pub fn fail_rate(mut self, numerator: u32, denominator: u32) -> Self {
        self.fail_rate = Some((numerator, denominator.max(1)));
        self
    }

    /// Whether this call should fail, given its key and payload length; advances the "fail the
    /// next n" counter when it is the reason.
    fn should_fail(&self, key: &str, payload_len: Option<u64>) -> bool {
        let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
        // "Fail the next n": a counter that counts down to zero and stays there.
        loop {
            let remaining = self.fail_next.load(Ordering::SeqCst);
            if remaining == 0 {
                break;
            }
            if self
                .fail_next
                .compare_exchange(remaining, remaining - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return true;
            }
        }
        if let Some(prefix) = &self.fail_prefix
            && key.starts_with(prefix.as_str())
        {
            return true;
        }
        if let (Some(limit), Some(len)) = (self.fail_after_bytes, payload_len)
            && len > limit
        {
            return true;
        }
        if let Some((numerator, denominator)) = self.fail_rate {
            let roll = splitmix64(self.seed ^ call_index) % u64::from(denominator);
            if roll < u64::from(numerator) {
                return true;
            }
        }
        false
    }

    fn refusal(key: &str) -> StoreError {
        StoreError::Backend {
            message: format!("flaky store: configured failure for `{key}`"),
        }
    }
}

/// A fixed, deterministic mixing step (Steele, Lea & Flood's splitmix64): the only source of
/// variability [`FlakyStore::fail_rate`] uses, and it is a pure function of its input, never a
/// clock or an OS random source.
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl<S: JournalStore> JournalStore for FlakyStore<S> {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            if !self.latency.is_zero() {
                tokio::time::sleep(self.latency).await;
            }
            let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if self.should_fail(key, Some(len)) {
                return Err(Self::refusal(key));
            }
            self.inner.put_new(key, bytes).await
        })
    }

    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            if !self.latency.is_zero() {
                tokio::time::sleep(self.latency).await;
            }
            let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if self.should_fail(key, Some(len)) {
                return Err(Self::refusal(key));
            }
            self.inner.put_replace(key, bytes, expected_etag).await
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        Box::pin(async move {
            if !self.latency.is_zero() {
                tokio::time::sleep(self.latency).await;
            }
            if self.should_fail(key, None) {
                return Err(Self::refusal(key));
            }
            self.inner.get(key).await
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        Box::pin(async move {
            if !self.latency.is_zero() {
                tokio::time::sleep(self.latency).await;
            }
            if self.should_fail(key, None) {
                return Err(Self::refusal(key));
            }
            self.inner.head(key).await
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        Box::pin(async move {
            if !self.latency.is_zero() {
                tokio::time::sleep(self.latency).await;
            }
            if self.should_fail(prefix, None) {
                return Err(Self::refusal(prefix));
            }
            self.inner.list(prefix, after, limit).await
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            if !self.latency.is_zero() {
                tokio::time::sleep(self.latency).await;
            }
            if self.should_fail(key, None) {
                return Err(Self::refusal(key));
            }
            self.inner.delete(key).await
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_journal_store::{Layout, MemoryStore};

    /// A canonical tail-chunk key the inner store accepts, for test keys only.
    fn tail_key(batch_id: u64) -> String {
        Layout::tail_chunk("acme", 1, batch_id)
            .expect("a valid tail key")
            .to_key()
    }

    #[tokio::test]
    async fn fail_next_fails_exactly_that_many_calls() {
        let store = FlakyStore::new(MemoryStore::new(), 1).fail_next(2);
        assert!(store.put_new(&tail_key(1), b"a".to_vec()).await.is_err());
        assert!(store.put_new(&tail_key(2), b"b".to_vec()).await.is_err());
        let ok = store.put_new(&tail_key(3), b"c".to_vec()).await;
        assert!(ok.is_ok(), "the third call should have succeeded: {ok:?}");
    }

    #[tokio::test]
    async fn fail_prefix_fails_only_matching_keys() {
        let store = FlakyStore::new(MemoryStore::new(), 1).fail_prefix("acme/tail/");
        assert!(store.put_new(&tail_key(1), b"x".to_vec()).await.is_err());
        let other_key = Layout::tail_chunk("other", 1, 1)
            .expect("a valid tail key")
            .to_key();
        let other = store.put_new(&other_key, b"y".to_vec()).await;
        assert!(other.is_ok(), "{other:?}");
    }

    #[tokio::test]
    async fn fail_after_bytes_fails_a_put_past_the_limit() {
        let store = FlakyStore::new(MemoryStore::new(), 1).fail_after_bytes(4);
        let small = store.put_new(&tail_key(1), b"ab".to_vec()).await;
        assert!(small.is_ok(), "{small:?}");
        let big = store.put_new(&tail_key(2), b"abcdef".to_vec()).await;
        assert!(big.is_err(), "a payload past the byte limit should fail");
    }

    #[tokio::test]
    async fn fail_rate_is_deterministic_for_the_same_seed_and_call_sequence() {
        let first = FlakyStore::new(MemoryStore::new(), 42).fail_rate(1, 3);
        let second = FlakyStore::new(MemoryStore::new(), 42).fail_rate(1, 3);
        for i in 0..20 {
            let key = tail_key(i);
            let a = first.put_new(&key, b"x".to_vec()).await.is_ok();
            let b = second.put_new(&key, b"x".to_vec()).await.is_ok();
            assert_eq!(
                a, b,
                "call {i} diverged between two stores with the same seed"
            );
        }
    }

    #[tokio::test]
    async fn no_configuration_never_fails() {
        let store = FlakyStore::new(MemoryStore::new(), 7);
        for i in 0..5 {
            let key = tail_key(i);
            assert!(store.put_new(&key, b"ok".to_vec()).await.is_ok());
        }
    }
}
