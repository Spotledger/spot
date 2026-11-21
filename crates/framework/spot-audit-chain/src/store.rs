//! The [`AuditStore`] trait and the [`InMemoryStore`].
//!
//! The trait has no method that edits or deletes an entry: the chain is append-only.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use crate::checkpoint::Checkpoint;
use crate::entry::ChainEntry;
use crate::error::AuditError;

/// The boxed future of every store method (the trait stays object-safe).
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, AuditError>> + Send + 'a>>;

/// What [`AuditStore::append_if_head`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendOutcome {
    /// The entry is now the head.
    Appended,
    /// The head was not the one expected (another writer won); nothing was written.
    HeadMoved,
}

/// Storage of one or more sites' chains. Entries are keyed by `site` and `seq`.
///
/// A backend must make [`AuditStore::append_if_head`] atomic (a compare-and-set on the head), so
/// two writers cannot both extend the same head. [`crate::contract::run`] is the backend-
/// independent suite every implementation must pass.
pub trait AuditStore: Send + Sync {
    /// The newest entry of `site`'s chain, if any.
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>>;

    /// Appends `entry` if the chain's head has sequence number `expected_seq` (`0` for an empty
    /// chain). Returns [`AppendOutcome::HeadMoved`] when it does not.
    ///
    /// # Errors
    /// [`AuditError::Store`] when the head matches but `entry` does not extend it (its `seq` is
    /// not `expected_seq + 1`, or its `prev_hash` is not the head's hash), or the backend fails.
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome>;

    /// Appends `entries`, which must follow one another (each `seq` one more than the last,
    /// each `prev_hash` the hash before it), if the chain's head has sequence number
    /// `expected_seq`: all of them or none. [`AppendOutcome::HeadMoved`] writes nothing.
    ///
    /// The provided implementation appends one by one and is **not** atomic: it exists so a
    /// store that only knows [`append_if_head`](Self::append_if_head) still answers. A store
    /// that makes the whole call one atomic write returns `true` from
    /// [`batches_appends`](Self::batches_appends), and only then does a caller prefer this
    /// method.
    ///
    /// # Errors
    /// [`AuditError::Store`] when the head matches but the entries do not extend it, or the
    /// backend fails (or, for the provided implementation, when the head moves after the first
    /// entry).
    fn append_many_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entries: Vec<ChainEntry>,
    ) -> StoreFuture<'a, AppendOutcome> {
        Box::pin(async move {
            let mut expected = expected_seq;
            for (index, entry) in entries.into_iter().enumerate() {
                let seq = entry.seq;
                match self.append_if_head(site, expected, entry).await? {
                    AppendOutcome::Appended => expected = seq,
                    AppendOutcome::HeadMoved if index == 0 => return Ok(AppendOutcome::HeadMoved),
                    AppendOutcome::HeadMoved => {
                        return Err(AuditError::Store(
                            "the head moved in the middle of a batch append".to_owned(),
                        ));
                    }
                }
            }
            Ok(AppendOutcome::Appended)
        })
    }

    /// Whether [`append_many_if_head`](Self::append_many_if_head) is one atomic write (cheaper
    /// than a loop of single appends).
    fn batches_appends(&self) -> bool {
        false
    }

    /// The stored entries with `from <= seq <= to`, in stored order.
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>>;

    /// Stores a checkpoint; an older one (lower `seq`) than the stored one is ignored.
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()>;

    /// The newest checkpoint of `site`.
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>>;
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) entries: HashMap<String, Vec<ChainEntry>>,
    pub(crate) checkpoints: HashMap<String, Checkpoint>,
}

/// An in-memory store, the reference implementation of the contract.
#[derive(Default)]
pub struct InMemoryStore {
    pub(crate) state: Mutex<State>,
}

impl InMemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, State>, AuditError> {
        self.state
            .lock()
            .map_err(|_| AuditError::Store("the in-memory store lock is poisoned".to_owned()))
    }
}

impl AuditStore for InMemoryStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async move {
            let state = self.lock()?;
            Ok(state.entries.get(site).and_then(|v| v.last()).cloned())
        })
    }

    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        Box::pin(async move {
            let mut state = self.lock()?;
            let chain = state.entries.entry(site.to_owned()).or_default();
            let head_seq = chain.last().map_or(0, |e| e.seq);
            if head_seq != expected_seq {
                return Ok(AppendOutcome::HeadMoved);
            }
            let prev_ok = chain.last().is_none_or(|h| h.hash == entry.prev_hash);
            if entry.seq != expected_seq.saturating_add(1) || !prev_ok {
                return Err(AuditError::Store(
                    "the entry does not extend the head".to_owned(),
                ));
            }
            chain.push(entry);
            Ok(AppendOutcome::Appended)
        })
    }

    fn append_many_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entries: Vec<ChainEntry>,
    ) -> StoreFuture<'a, AppendOutcome> {
        Box::pin(async move {
            let mut state = self.lock()?;
            let chain = state.entries.entry(site.to_owned()).or_default();
            let head_seq = chain.last().map_or(0, |e| e.seq);
            if head_seq != expected_seq {
                return Ok(AppendOutcome::HeadMoved);
            }
            let mut seq = expected_seq;
            let mut hash = chain.last().map(|h| h.hash);
            for entry in &entries {
                let linked = hash.is_none_or(|h| h == entry.prev_hash);
                if entry.seq != seq.saturating_add(1) || !linked {
                    return Err(AuditError::Store(
                        "the entry does not extend the head".to_owned(),
                    ));
                }
                seq = entry.seq;
                hash = Some(entry.hash);
            }
            chain.extend(entries);
            Ok(AppendOutcome::Appended)
        })
    }

    fn batches_appends(&self) -> bool {
        true
    }

    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async move {
            let state = self.lock()?;
            Ok(state
                .entries
                .get(site)
                .map(|v| {
                    v.iter()
                        .filter(|e| e.seq >= from && e.seq <= to)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let mut state = self.lock()?;
            let newer = state
                .checkpoints
                .get(&checkpoint.site)
                .is_none_or(|c| checkpoint.seq >= c.seq);
            if newer {
                state
                    .checkpoints
                    .insert(checkpoint.site.clone(), checkpoint);
            }
            Ok(())
        })
    }

    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        Box::pin(async move { Ok(self.lock()?.checkpoints.get(site).cloned()) })
    }
}
