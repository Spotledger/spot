//! [`AuditChain`]: append and read over an [`AuditStore`].

use std::sync::Arc;

use crate::checkpoint::{Checkpoint, CheckpointSigner};
use crate::entry::{ChainEntry, genesis_hash};
use crate::error::AuditError;
use crate::record::AuditRecord;
use crate::store::{AppendOutcome, AuditStore};

/// How often a checkpoint is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointPolicy {
    /// A checkpoint is signed after every entry whose `seq` is a multiple of this; `0` means
    /// never automatically (only [`AuditChain::checkpoint_now`]). It is also the segment length.
    pub every: u64,
}

impl Default for CheckpointPolicy {
    fn default() -> Self {
        Self { every: 1000 }
    }
}

/// The first and last `seq` of segment `index` (0-based) for segments of `len` entries, or `None`
/// when `len` is 0.
#[must_use]
pub fn segment_bounds(index: u64, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let first = index.checked_mul(len)?.checked_add(1)?;
    let last = first.checked_add(len - 1)?;
    Some((first, last))
}

const MAX_APPEND_ATTEMPTS: usize = 32;

/// The writer of one site's chain.
pub struct AuditChain<S: AuditStore> {
    store: S,
    site: String,
    signer: Arc<dyn CheckpointSigner>,
    policy: CheckpointPolicy,
}

impl<S: AuditStore> AuditChain<S> {
    /// A chain for `site` over `store`; checkpoints are signed with `signer`.
    pub fn new(
        store: S,
        site: impl Into<String>,
        signer: Arc<dyn CheckpointSigner>,
        policy: CheckpointPolicy,
    ) -> Self {
        Self {
            store,
            site: site.into(),
            signer,
            policy,
        }
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The store, for the verifier and for tests.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// Validates `record` and appends it: assigns `seq`, links and hashes it. If another writer
    /// extends the head first, the append is retried on the new head.
    ///
    /// # Errors
    /// The record's validation errors, [`AuditError::HeadConflict`] after too many lost races, or
    /// a store or signer error. A signer error leaves the entry appended.
    pub async fn append(&self, record: AuditRecord) -> Result<ChainEntry, AuditError> {
        record.validate()?;
        for _ in 0..MAX_APPEND_ATTEMPTS {
            let head = self.store.head(&self.site).await?;
            let (head_seq, prev_hash) = match &head {
                Some(h) => (h.seq, h.hash),
                None => (0, genesis_hash(&self.site)),
            };
            let entry = ChainEntry::new(head_seq.saturating_add(1), prev_hash, record.clone());
            match self
                .store
                .append_if_head(&self.site, head_seq, entry.clone())
                .await?
            {
                AppendOutcome::Appended => {
                    if self.policy.every > 0 && entry.seq.is_multiple_of(self.policy.every) {
                        self.sign_and_store(&entry, entry.record.at).await?;
                    }
                    return Ok(entry);
                }
                AppendOutcome::HeadMoved => {}
            }
        }
        Err(AuditError::HeadConflict)
    }

    /// The newest entry.
    ///
    /// # Errors
    /// A store error.
    pub async fn head(&self) -> Result<Option<ChainEntry>, AuditError> {
        self.store.head(&self.site).await
    }

    /// The entries with `from <= seq <= to`.
    ///
    /// # Errors
    /// A store error.
    pub async fn read_range(&self, from: u64, to: u64) -> Result<Vec<ChainEntry>, AuditError> {
        self.store.range(&self.site, from, to).await
    }

    /// Signs and stores a checkpoint of the current head at time `at`; `None` for an empty chain.
    ///
    /// # Errors
    /// A store or signer error.
    pub async fn checkpoint_now(&self, at: i64) -> Result<Option<Checkpoint>, AuditError> {
        match self.store.head(&self.site).await? {
            None => Ok(None),
            Some(head) => Ok(Some(self.sign_and_store(&head, at).await?)),
        }
    }

    async fn sign_and_store(&self, head: &ChainEntry, at: i64) -> Result<Checkpoint, AuditError> {
        let cp = Checkpoint::sign(&*self.signer, &self.site, head.seq, head.hash, at)?;
        self.store.put_checkpoint(cp.clone()).await?;
        Ok(cp)
    }
}
