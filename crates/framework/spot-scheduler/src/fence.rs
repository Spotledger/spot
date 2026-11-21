//! [`FencedWrite`]: the check that keeps an old lease holder's write out of the store, and the
//! tiny [`FenceGuard`]/counter the tests demonstrate it with.

use crate::error::{Error, FENCE_MISMATCH, Result};
use crate::lease::{FencingToken, Lease};

/// The check every storage-backed write on behalf of a lease runs first.
///
/// `check(store, &lease)` compares the lease the caller holds against the store's current
/// record for the same name: same record id *and* same token, else
/// `scheduler.fence_mismatch`/`scheduler.lease_lost`. A node paused past its TTL renews, gets
/// refused, and must drop everything it planned to write — its tokens are old and the new
/// holder has taken over.
pub struct FencedWrite;

impl FencedWrite {
    /// Whether the caller's `lease` is still the store's current one: the write may proceed.
    /// `Ok(())` when it is; `Err` with `fence_mismatch` (same record, different token — the
    /// storage-side token check failed) or `lease_lost` (no record, or a different record
    /// holds the name) otherwise.
    pub fn check(store: &dyn crate::lease::LeaseStore, lease: &Lease) -> Result<()> {
        match store.current(&lease.name) {
            Some(current) if current.id == lease.id && current.token == lease.token => Ok(()),
            Some(current) if current.id == lease.id => Err(Error::new(
                FENCE_MISMATCH,
                format!(
                    "lease {}: write carries token {}, the record holds token {}",
                    lease.name, lease.token.0, current.token.0
                ),
            )),
            Some(current) => Err(Error::lease_lost(
                &lease.name,
                &lease.holder,
                lease.token.0,
                &format!("token {} holds it now", current.token.0),
            )),
            None => Err(Error::lease_lost(
                &lease.name,
                &lease.holder,
                lease.token.0,
                "the lease has no current record",
            )),
        }
    }
}

/// A one-cell guarded store: the smallest thing a fencing token actually protects.
///
/// [`FencedCounter::bump`] mimics a real write: it checks the lease first
/// ([`FencedWrite::check`]), and only a current lease may move the counter — a stale holder's
/// bump is refused, leaving the value untouched. The tests use it to show a crashed-and-
/// resurrected node cannot corrupt the counter the new holder is advancing.
#[derive(Debug, Default)]
pub struct FenceGuard {
    value: std::sync::Mutex<Option<GuardedCell>>,
}

#[derive(Debug)]
struct GuardedCell {
    value: u64,
    token: FencingToken,
}

impl FenceGuard {
    /// An empty guard (value 0, no lease has written yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the value and the token of the lease that last wrote it.
    pub fn get(&self) -> (u64, Option<FencingToken>) {
        let cell = self.lock();
        match &*cell {
            Some(cell) => (cell.value, Some(cell.token)),
            None => (0, None),
        }
    }

    /// Adds one, when `lease` still holds the name: the guarded check runs before the write.
    /// Fails with the same codes [`FencedWrite::check`] produces, leaving the value unchanged.
    pub fn bump(&self, store: &dyn crate::lease::LeaseStore, lease: &Lease) -> Result<u64> {
        FencedWrite::check(store, lease)?;
        let mut cell = self.lock();
        let next = match &*cell {
            Some(cell) => cell.value + 1,
            None => 1,
        };
        *cell = Some(GuardedCell {
            value: next,
            token: lease.token,
        });
        Ok(next)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<GuardedCell>> {
        // One plain cell; a panic elsewhere cannot leave it inconsistent.
        self.lock_inner()
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Option<GuardedCell>> {
        self.value
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
