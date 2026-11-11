//! Throttle decorators: lockout transitions are audited, and the in-process counters have a
//! ceiling.
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use spot_core::Clock;
use spot_sessions::throttle::{FailureRecord, ThrottleError, ThrottleStore};

use super::{AuditRecord, Recorder, lock, redact_name};

/// The most throttle keys (accounts and sources) the in-process counters track.
pub const THROTTLE_KEY_CAP: usize = 50_000;

// ---- lockout transitions -------------------------------------------------------------------

/// A throttle store that audits a lockout starting and ending. The service does not announce
/// either, so this decorator watches the account counter: a record whose `locked_until` moves
/// from nothing (or a past instant) to the future is a lockout start; a counter that is cleared
/// or rewritten unlocked while it held a lock is a lockout end.
pub struct AuditedThrottleStore<S> {
    inner: Arc<S>,
    recorder: Arc<Recorder>,
}

impl<S: ThrottleStore> AuditedThrottleStore<S> {
    /// Audits the transitions of `inner`.
    #[must_use]
    pub fn new(inner: Arc<S>, recorder: Arc<Recorder>) -> Self {
        Self { inner, recorder }
    }

    fn named(&self, kind: &str, key: &[u8]) -> AuditRecord {
        let mut record = self.recorder.record(kind);
        let text = String::from_utf8_lossy(key);
        match text.strip_prefix(crate::password_throttle::KEY_PREFIX) {
            // A password-change counter is keyed by the user's record id, which names the user
            // in the record; no sign-in name was presented.
            Some(user) => record.user = Some(user.to_owned()),
            None => record.attempted = Some(redact_name(&text)),
        }
        record
    }

    fn ended(&self, key: &[u8], previous: &FailureRecord, why: &str) {
        let mut record = self.named("lockout_ended", key);
        record.reason = Some(why.to_owned());
        record.until = Some(previous.locked_until);
        self.recorder.lockout(&record);
    }
}

type ThrottleFuture<'a, T> =
    Pin<Box<dyn Future<Output = std::result::Result<T, ThrottleError>> + Send + 'a>>;

impl<S: ThrottleStore> ThrottleStore for AuditedThrottleStore<S> {
    fn get<'a>(&'a self, account: bool, key: &'a [u8]) -> ThrottleFuture<'a, FailureRecord> {
        self.inner.get(account, key)
    }

    fn put<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
        record: FailureRecord,
    ) -> ThrottleFuture<'a, ()> {
        Box::pin(async move {
            if !account {
                return self.inner.put(account, key, record).await;
            }
            let previous = self.inner.get(true, key).await?;
            // The counter is written first: a lock is in force whatever happens to its audit.
            self.inner.put(true, key, record).await?;
            let now = self.recorder.now();
            if record.locked_until > now && record.locked_until > previous.locked_until {
                let mut started = self.named("lockout_started", key);
                started.count = Some(u64::from(record.failures));
                started.until = Some(record.locked_until);
                self.recorder.lockout(&started);
            } else if previous.locked_until != 0 && record.locked_until == 0 {
                self.ended(key, &previous, "reset");
            }
            Ok(())
        })
    }

    fn clear<'a>(&'a self, account: bool, key: &'a [u8]) -> ThrottleFuture<'a, ()> {
        Box::pin(async move {
            if !account {
                return self.inner.clear(account, key).await;
            }
            let previous = self.inner.get(true, key).await?;
            self.inner.clear(true, key).await?;
            if previous.locked_until != 0 {
                let why = if previous.locked_until <= self.recorder.now() {
                    "expired"
                } else {
                    "cleared"
                };
                self.ended(key, &previous, why);
            }
            Ok(())
        })
    }
}

// ---- the bounded in-process counters ------------------------------------------------------

/// The in-process throttle counters with a hard ceiling on tracked keys.
///
/// Counters are **per process**: a restart forgets them (documented in the README and logged at
/// start). Inside a process the number of keys is bounded because an attacker chooses the names:
/// when the ceiling is reached the least recently written key that is *not currently locked* is
/// evicted. If every key is a live lockout there is nothing safe to evict and the write fails,
/// which refuses the sign-in (fail closed); a flood of new names can therefore never wash a
/// lockout out.
pub struct BoundedThrottleStore {
    cap: usize,
    clock: Arc<dyn Clock>,
    inner: Mutex<Bounded>,
}

#[derive(Default)]
struct Bounded {
    map: BTreeMap<(bool, Vec<u8>), (FailureRecord, u64)>,
    order: BTreeMap<u64, (bool, Vec<u8>)>,
    next: u64,
}

impl BoundedThrottleStore {
    /// A store with the default ceiling.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self::with_cap(clock, THROTTLE_KEY_CAP)
    }

    /// A store tracking at most `cap` keys.
    #[must_use]
    pub fn with_cap(clock: Arc<dyn Clock>, cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            clock,
            inner: Mutex::new(Bounded::default()),
        }
    }

    /// How many keys are tracked now.
    pub fn len(&self) -> usize {
        lock(&self.inner).map.len()
    }

    /// Whether no key is tracked.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ThrottleStore for BoundedThrottleStore {
    fn get<'a>(&'a self, account: bool, key: &'a [u8]) -> ThrottleFuture<'a, FailureRecord> {
        Box::pin(async move {
            Ok(lock(&self.inner)
                .map
                .get(&(account, key.to_vec()))
                .map(|(record, _)| *record)
                .unwrap_or_default())
        })
    }

    fn put<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
        record: FailureRecord,
    ) -> ThrottleFuture<'a, ()> {
        Box::pin(async move {
            let now = self.clock.now_utc().unix_timestamp();
            let mut inner = lock(&self.inner);
            let slot = (account, key.to_vec());
            if let Some((_, seq)) = inner.map.remove(&slot) {
                // An existing key is replaced in place: no eviction, and it becomes the newest.
                inner.order.remove(&seq);
            }
            while inner.map.len() >= self.cap {
                let victim = inner
                    .order
                    .iter()
                    .find(|(_, k)| {
                        inner
                            .map
                            .get(*k)
                            .is_some_and(|(r, _)| r.locked_until <= now)
                    })
                    .map(|(seq, k)| (*seq, k.clone()));
                let Some((seq, victim)) = victim else {
                    return Err(ThrottleError::StoreFailed(
                        "the throttle table is full of live lockouts".into(),
                    ));
                };
                inner.order.remove(&seq);
                inner.map.remove(&victim);
            }
            let seq = inner.next;
            inner.next += 1;
            inner.order.insert(seq, slot.clone());
            inner.map.insert(slot, (record, seq));
            Ok(())
        })
    }

    fn clear<'a>(&'a self, account: bool, key: &'a [u8]) -> ThrottleFuture<'a, ()> {
        Box::pin(async move {
            let mut inner = lock(&self.inner);
            if let Some((_, seq)) = inner.map.remove(&(account, key.to_vec())) {
                inner.order.remove(&seq);
            }
            Ok(())
        })
    }
}
