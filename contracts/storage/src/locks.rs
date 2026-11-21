//! Row locks and guard locks for hot rows and multi-row rules (ADR-46).
//!
//! No correctness may depend on the database preventing write skew by default. A business
//! rule that spans several rows either locks a guard ([`Locks::lock_key`]) or runs through a
//! single writer. All locks are released when the transaction ends; there are no session locks.

use std::future::Future;

use crate::error::StorageResult;
use crate::id::Name;

/// Whether to wait for a lock held by another transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// Wait up to the transaction's lock timeout, then fail with `LockTimeout`.
    Block,
    /// Fail at once with `LockNotAvailable`.
    NoWait,
    /// Leave locked rows out of the result instead of waiting (queue consumption). Only valid
    /// on queries; elsewhere it fails with `InvalidQuery`.
    SkipLocked,
}

/// A lock on rows read by a load or query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RowLock {
    /// Plain read; no lock.
    #[default]
    None,
    /// Others may read and share-lock, but not write or exclusively lock, until this
    /// transaction ends.
    Share(Wait),
    /// Nobody else may write or lock the rows until this transaction ends (`FOR UPDATE`).
    Update(Wait),
}

/// Shared or exclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// Compatible with other shared holders.
    Shared,
    /// Excludes every other holder.
    Exclusive,
}

/// A named guard: any application-level resource, for example "stock of item X in warehouse Y".
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LockKey {
    /// The kind of resource, for example `stock_balance`.
    pub namespace: Name,
    /// The resource within the namespace, for example the item and warehouse ids.
    pub key: String,
}

/// Transaction-scoped guard locks.
pub trait Locks: Send {
    /// Takes a guard lock until the transaction ends. Taking a lock this transaction already
    /// holds succeeds; a shared lock is upgraded when exclusive is requested.
    fn lock_key(
        &mut self,
        key: &LockKey,
        mode: LockMode,
        wait: Wait,
    ) -> impl Future<Output = StorageResult<()>> + Send;
}
