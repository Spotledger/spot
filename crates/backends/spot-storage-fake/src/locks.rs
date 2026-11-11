//! Row locks and guard locks: one lock table, shared by all transactions.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use spot_storage_contract::{
    ErrorKind, LockMode, Name, RecordId, RowLock, StorageError, StorageResult, Wait,
};

use crate::Shared;

/// What a lock protects.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LockTarget {
    /// A record of an entity.
    Row(Name, RecordId),
    /// A named guard (`Locks::lock_key`, and sequences under namespace `sequence`).
    Guard(Name, String),
}

/// Holders of every lock, by target. A holder is a transaction id.
#[derive(Debug, Default)]
pub(crate) struct LockTable {
    held: HashMap<LockTarget, BTreeMap<RecordId, LockMode>>,
}

impl LockTable {
    /// Grants `mode` on `target` to `tx` if no other holder conflicts. A holder's own shared
    /// lock never blocks its upgrade.
    pub fn try_acquire(&mut self, tx: RecordId, target: &LockTarget, mode: LockMode) -> bool {
        let holders = self.held.entry(target.clone()).or_default();
        let conflict = holders
            .iter()
            .any(|(h, m)| *h != tx && (mode == LockMode::Exclusive || *m == LockMode::Exclusive));
        if conflict {
            return false;
        }
        let held = holders.entry(tx).or_insert(mode);
        if mode == LockMode::Exclusive {
            *held = LockMode::Exclusive;
        }
        true
    }

    /// Releases every lock `tx` holds. Returns whether it held any.
    pub fn release_all(&mut self, tx: RecordId) -> bool {
        let mut released = false;
        self.held.retain(|_, holders| {
            released |= holders.remove(&tx).is_some();
            !holders.is_empty()
        });
        released
    }
}

/// The lock mode a row lock asks for, or `None` for a plain read.
pub(crate) fn row_lock_mode(lock: RowLock) -> Option<(LockMode, Wait)> {
    match lock {
        RowLock::None => None,
        RowLock::Share(w) => Some((LockMode::Shared, w)),
        RowLock::Update(w) => Some((LockMode::Exclusive, w)),
    }
}

/// Takes a lock for `tx`, waiting as `wait` says. `SkipLocked` is only valid on queries, so it
/// fails here with `InvalidQuery`.
pub(crate) async fn acquire(
    shared: &Shared,
    tx: RecordId,
    target: &LockTarget,
    mode: LockMode,
    wait: Wait,
    timeout: Duration,
) -> StorageResult<()> {
    if wait == Wait::SkipLocked {
        return Err(StorageError::new(
            ErrorKind::InvalidQuery,
            "SKIP LOCKED is only valid on queries",
        ));
    }
    let mut deadline = None;
    loop {
        let notified = shared.released.notified();
        let mut notified = std::pin::pin!(notified);
        {
            let mut state = shared.state();
            if state.locks.try_acquire(tx, target, mode) {
                return Ok(());
            }
            if wait == Wait::NoWait {
                return Err(not_available(target));
            }
            // Registered while the state is locked, so a release after this point wakes us.
            notified.as_mut().enable();
        }
        let until = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + timeout);
        if tokio::time::timeout_at(until, notified).await.is_err() {
            return Err(timed_out(target, timeout));
        }
    }
}

/// Waits until `target` could be granted to `tx` in `mode` (without taking it), or fails with
/// `LockTimeout` at `until`. Used by queries, which retry after the wait.
pub(crate) async fn wait_until_free(
    shared: &Shared,
    tx: RecordId,
    target: &LockTarget,
    mode: LockMode,
    until: tokio::time::Instant,
    timeout: Duration,
) -> StorageResult<()> {
    loop {
        let notified = shared.released.notified();
        let mut notified = std::pin::pin!(notified);
        {
            let mut state = shared.state();
            // Taking the lock now is harmless: the query takes it again on retry.
            if state.locks.try_acquire(tx, target, mode) {
                return Ok(());
            }
            notified.as_mut().enable();
        }
        if tokio::time::timeout_at(until, notified).await.is_err() {
            return Err(timed_out(target, timeout));
        }
    }
}

/// Locks the `from` rows of a query as the query's [`RowLock`] asks. The query engine calls
/// [`RowLocker::lock_row`] for each candidate row, under the state mutex.
#[allow(dead_code, reason = "called by the query engine of WP-CON-01.T3")]
pub(crate) trait RowLocker {
    /// Locks one row. `Ok(true)`: locked (or no lock asked), keep the row. `Ok(false)`: the
    /// row is locked by another transaction and the query skips locked rows. An error ends
    /// the query; after a blocked [`Wait::Block`] the caller waits and runs the query again.
    fn lock_row(&mut self, entity: &Name, id: RecordId) -> StorageResult<bool>;
}

/// The locker for reads outside a transaction, where `Query::lock` must be `RowLock::None`.
pub(crate) struct NoRowLocks;

impl RowLocker for NoRowLocks {
    fn lock_row(&mut self, _: &Name, _: RecordId) -> StorageResult<bool> {
        Ok(true)
    }
}

/// The locker of a transaction's query.
#[allow(
    dead_code,
    reason = "read by `lock_row`, which the query engine of WP-CON-01.T3 calls"
)]
pub(crate) struct TxRowLocker<'a> {
    /// The shared lock table.
    pub locks: &'a mut LockTable,
    /// Commit number of the last write to each record.
    pub written_at: &'a HashMap<(Name, RecordId), u64>,
    /// The locking transaction.
    pub tx: RecordId,
    /// The query's lock.
    pub lock: RowLock,
    /// The snapshot's commit number, for repeatable-read transactions.
    pub snapshot: Option<u64>,
    /// Set when a `Wait::Block` lock is held by another transaction.
    pub blocked: Option<(LockTarget, LockMode)>,
}

impl RowLocker for TxRowLocker<'_> {
    fn lock_row(&mut self, entity: &Name, id: RecordId) -> StorageResult<bool> {
        let Some((mode, wait)) = row_lock_mode(self.lock) else {
            return Ok(true);
        };
        let target = LockTarget::Row(entity.clone(), id);
        if !self.locks.try_acquire(self.tx, &target, mode) {
            return match wait {
                Wait::SkipLocked => Ok(false),
                Wait::NoWait => Err(not_available(&target)),
                Wait::Block => {
                    let e = not_available(&target);
                    self.blocked = Some((target, mode));
                    Err(e)
                }
            };
        }
        if let Some(snapshot) = self.snapshot
            && self
                .written_at
                .get(&(entity.clone(), id))
                .is_some_and(|c| *c > snapshot)
        {
            return Err(crate::concurrent_update(entity));
        }
        Ok(true)
    }
}

fn describe(target: &LockTarget) -> String {
    match target {
        LockTarget::Row(entity, id) => format!("row {id} of `{entity}`"),
        LockTarget::Guard(ns, key) => format!("guard `{ns}`/`{key}`"),
    }
}

fn with_entity(e: StorageError, target: &LockTarget) -> StorageError {
    match target {
        LockTarget::Row(entity, _) => e.with_entity(entity),
        LockTarget::Guard(..) => e,
    }
}

/// `LockNotAvailable` for `target`.
pub(crate) fn not_available(target: &LockTarget) -> StorageError {
    with_entity(
        StorageError::new(
            ErrorKind::LockNotAvailable,
            format!(
                "{} is locked by another transaction; retry later or wait for it",
                describe(target)
            ),
        ),
        target,
    )
}

fn timed_out(target: &LockTarget, timeout: Duration) -> StorageError {
    with_entity(
        StorageError::new(
            ErrorKind::LockTimeout,
            format!(
                "waited {} ms for {} without getting it; retry the transaction",
                timeout.as_millis(),
                describe(target)
            ),
        ),
        target,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> LockTarget {
        LockTarget::Guard(
            Name::new("stock").unwrap_or_else(|e| panic!("{e}")),
            "a".into(),
        )
    }

    #[test]
    fn shared_holders_coexist_and_block_exclusive() {
        let (t1, t2) = (RecordId::new_v7(), RecordId::new_v7());
        let mut locks = LockTable::default();
        assert!(locks.try_acquire(t1, &target(), LockMode::Shared));
        assert!(locks.try_acquire(t2, &target(), LockMode::Shared));
        assert!(!locks.try_acquire(t1, &target(), LockMode::Exclusive));
        assert!(locks.release_all(t2));
        assert!(locks.try_acquire(t1, &target(), LockMode::Exclusive));
        // Re-requesting shared keeps the exclusive lock.
        assert!(locks.try_acquire(t1, &target(), LockMode::Shared));
        assert!(!locks.try_acquire(t2, &target(), LockMode::Shared));
        assert!(locks.release_all(t1));
        assert!(!locks.release_all(t1));
        assert!(locks.held.is_empty());
    }
}
