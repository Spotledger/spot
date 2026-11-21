//! Applying the retention plan (WP-RTB-01.T11, ADR-061 section 6): the deletions the plan names,
//! and only those.
//!
//! [`apply`] never deletes more than [`crate::retention::plan`] named. On top of the plan it
//! refuses three things whatever the plan says: a snapshot that is not in the listing it was
//! given (an unlisted key is never deleted), the newest verified snapshot (the base a rebuild
//! trusts) and any snapshot a reader holds (a rebuild or an audit reading it). A dry run reports
//! what would go and deletes nothing.
//!
//! The deletion itself is a [`SnapshotPruner`]. [`StorePruner`] carries it out through
//! `JournalStore::delete`, which today **refuses everything under `snapshots/`** (the store keeps
//! the recoverable record forever: `StoreError::DeleteRefused`). The pruner reports that as
//! [`PruneError::StoreRefuses`] and [`apply`] stops at the first one, so a store that does not
//! allow it costs one refusal, not one per snapshot. Allowing it is a change in the store crate
//! (see the task report); nothing here works around the refusal.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;

use spot_journal_rebuild_plan::SnapshotInfo;
use spot_journal_snapshot::INDEX_NAME;
use spot_journal_store::{JournalStore, Layout, StoreError};

use crate::retention::{RetentionPlan, SnapshotId};

/// Why a snapshot could not be removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PruneError {
    /// The store refuses to delete anything under `snapshots/`.
    #[error("the journal store refuses to delete a snapshot")]
    StoreRefuses,
    /// The store failed (code of the store's error).
    #[error("the snapshot could not be removed: {0}")]
    Failed(&'static str),
}

impl PruneError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::StoreRefuses => "journal_snapshot_pg.prune_refused",
            Self::Failed(_) => "journal_snapshot_pg.prune_failed",
        }
    }
}

/// A boxed future of one removal: how many objects went.
pub type PruneFuture<'a> = Pin<Box<dyn Future<Output = Result<u64, PruneError>> + Send + 'a>>;

/// Removes one whole snapshot.
pub trait SnapshotPruner: Send + Sync {
    /// Removes every object of the snapshot `id`.
    fn remove<'a>(&'a self, id: SnapshotId) -> PruneFuture<'a>;
}

/// Removes a snapshot through the journal store, index first: a snapshot without its index is
/// not listed, so a removal that stops half way leaves nothing a rebuild could trust.
pub struct StorePruner<S> {
    store: S,
    tenant: String,
}

impl<S: JournalStore> StorePruner<S> {
    /// A pruner over `store` for `tenant`'s snapshots.
    #[must_use]
    pub fn new(store: S, tenant: impl Into<String>) -> Self {
        Self {
            store,
            tenant: tenant.into(),
        }
    }
}

fn failed(error: &StoreError) -> PruneError {
    match error {
        StoreError::DeleteRefused { .. } => PruneError::StoreRefuses,
        other => PruneError::Failed(other.code()),
    }
}

impl<S: JournalStore> SnapshotPruner for StorePruner<S> {
    fn remove<'a>(&'a self, id: SnapshotId) -> PruneFuture<'a> {
        Box::pin(async move {
            let (generation, position) = id;
            let prefix = Layout::snapshot(&self.tenant, generation, position)
                .map_err(|e| failed(&e))?
                .to_key();
            let listed = self
                .store
                .list(&prefix, None, 10_000)
                .await
                .map_err(|e| failed(&e))?;
            let mut keys: Vec<String> = listed.into_iter().map(|e| e.key).collect();
            // The index first, then the rest in listing order.
            let index_suffix = format!("/{INDEX_NAME}");
            keys.sort_by_key(|key| !key.ends_with(&index_suffix));
            let mut removed = 0_u64;
            for key in keys {
                self.store.delete(&key).await.map_err(|e| failed(&e))?;
                removed += 1;
            }
            Ok(removed)
        })
    }
}

/// What one application of the plan did. Identities and counts only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    /// A dry run: `would_delete` is what a real run would remove, nothing was removed.
    pub dry_run: bool,
    /// How many snapshots the plan named.
    pub planned: usize,
    /// Removed.
    pub deleted: Vec<SnapshotId>,
    /// Named by the plan and not removed because a dry run.
    pub would_delete: Vec<SnapshotId>,
    /// Named by the plan, kept because a reader holds them.
    pub kept_in_use: Vec<SnapshotId>,
    /// Named by the plan, kept because it is the newest verified snapshot.
    pub kept_latest_verified: Vec<SnapshotId>,
    /// Named by the plan, never touched because they are not in the listing.
    pub skipped_unlisted: Vec<SnapshotId>,
    /// Removal failed for these, with the code.
    pub failed: Vec<(SnapshotId, &'static str)>,
    /// The store refused to delete snapshots at all; the rest were not attempted.
    pub store_refused: bool,
}

/// The newest verified snapshot of `listed`, in the order the plan uses.
fn latest_verified(listed: &[SnapshotInfo]) -> Option<SnapshotId> {
    listed
        .iter()
        .filter(|info| info.verified)
        .max_by_key(|info| (info.taken_at_millis, info.generation, info.position))
        .map(|info| (info.generation, info.position))
}

/// Carries out `plan` over `listed` (the listing the plan was made from), deleting through
/// `pruner`. `in_use` are the snapshots a reader holds.
pub async fn apply(
    plan: &RetentionPlan,
    listed: &[SnapshotInfo],
    in_use: &[SnapshotId],
    pruner: &dyn SnapshotPruner,
    dry_run: bool,
) -> PruneOutcome {
    let known: BTreeSet<SnapshotId> = listed.iter().map(|i| (i.generation, i.position)).collect();
    let held: BTreeSet<SnapshotId> = in_use.iter().copied().collect();
    let newest_verified = latest_verified(listed);
    let mut outcome = PruneOutcome {
        dry_run,
        planned: plan.prune.len(),
        ..PruneOutcome::default()
    };
    let mut seen = BTreeSet::new();
    for id in &plan.prune {
        if !seen.insert(*id) {
            continue;
        }
        if !known.contains(id) {
            outcome.skipped_unlisted.push(*id);
        } else if Some(*id) == newest_verified {
            outcome.kept_latest_verified.push(*id);
        } else if held.contains(id) {
            outcome.kept_in_use.push(*id);
        } else if dry_run {
            outcome.would_delete.push(*id);
        } else if outcome.store_refused {
            // The store said no once; the others are not tried.
        } else {
            match pruner.remove(*id).await {
                Ok(_) => outcome.deleted.push(*id),
                Err(PruneError::StoreRefuses) => outcome.store_refused = true,
                Err(error) => outcome.failed.push((*id, error.code())),
            }
        }
    }
    outcome
}
