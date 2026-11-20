//! Snapshot retention applied (WP-RTB-01.T11): only what the plan names goes, never the newest
//! verified snapshot, never one a reader holds, never an unlisted key, nothing in a dry run, and a
//! store that refuses to delete snapshots is reported once and not worked around. No database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use spot_cli::services::journal_snapshots::Settings;
use spot_journal_snapshot::{INDEX_NAME, SnapshotInfo};
use spot_journal_snapshot_pg::retention::{Retention, SnapshotId, plan};
use spot_journal_snapshot_pg::{
    PruneError, SnapshotPruner, StorePruner, apply_retention, prune::PruneFuture,
};
use spot_journal_store::{FolderStore, JournalStore, Layout};

const DAY: i64 = 86_400_000;
const SITE: &str = "acme";

fn snap(generation: u64, position: u64, day: i64, verified: bool) -> SnapshotInfo {
    SnapshotInfo {
        position,
        generation,
        verified,
        taken_at_millis: day * DAY + 3_600_000,
    }
}

/// Forty daily snapshots, one per day, the newest verified one on day 10 (long out of the
/// retained days).
fn forty() -> Vec<SnapshotInfo> {
    (0..40)
        .map(|d| snap(1, 100 + u64::try_from(d).unwrap(), d, d == 10))
        .collect()
}

#[derive(Default)]
struct Recording {
    removed: Mutex<Vec<SnapshotId>>,
    refuse: bool,
    fail_at: Option<SnapshotId>,
}

impl SnapshotPruner for Recording {
    fn remove<'a>(&'a self, id: SnapshotId) -> PruneFuture<'a> {
        Box::pin(async move {
            if self.refuse {
                return Err(PruneError::StoreRefuses);
            }
            if self.fail_at == Some(id) {
                return Err(PruneError::Failed("journal_store.io"));
            }
            self.removed.lock().unwrap().push(id);
            Ok(3)
        })
    }
}

#[test]
fn the_settings_take_the_prune_switches_and_refuse_unknown_keys() {
    let defaults = Settings::default();
    assert!(defaults.prune);
    assert!(!defaults.prune_dry_run);
    let parsed: Settings = toml::from_str("prune = false\nprune_dry_run = true").unwrap();
    assert!(!parsed.prune);
    assert!(parsed.prune_dry_run);
    assert!(toml::from_str::<Settings>("prune_all = true").is_err());
}

#[tokio::test]
async fn the_plan_is_applied_and_only_the_plan() {
    let listed = forty();
    let planned = plan(&listed, Retention::default());
    let pruner = Recording::default();
    let outcome = apply_retention(&planned, &listed, &[], &pruner, false).await;
    let removed = pruner.removed.lock().unwrap().clone();
    // Everything removed was named by the plan, and every removal is reported.
    assert!(removed.iter().all(|id| planned.prune.contains(id)));
    assert_eq!(removed, outcome.deleted);
    assert!(!removed.is_empty());
    // What the plan keeps is never touched.
    assert!(removed.iter().all(|id| !planned.keep.contains(id)));
    assert!(outcome.failed.is_empty());
    assert!(!outcome.store_refused);
}

#[tokio::test]
async fn the_newest_verified_snapshot_is_kept_whatever_the_plan_says() {
    let listed = forty();
    let verified = (1, 110);
    // A plan that (wrongly) names the verified snapshot for removal.
    let mut planned = plan(&listed, Retention::default());
    planned.keep.retain(|id| *id != verified);
    planned.prune.push(verified);
    let pruner = Recording::default();
    let outcome = apply_retention(&planned, &listed, &[], &pruner, false).await;
    assert!(!pruner.removed.lock().unwrap().contains(&verified));
    assert_eq!(outcome.kept_latest_verified, vec![verified]);
}

#[tokio::test]
async fn a_snapshot_a_reader_holds_is_kept() {
    let listed = forty();
    let planned = plan(&listed, Retention::default());
    let held = planned.prune[0];
    let pruner = Recording::default();
    let outcome = apply_retention(&planned, &listed, &[held], &pruner, false).await;
    assert!(!pruner.removed.lock().unwrap().contains(&held));
    assert_eq!(outcome.kept_in_use, vec![held]);
    // Once the hold is dropped the same plan removes it.
    let again = Recording::default();
    apply_retention(&planned, &listed, &[], &again, false).await;
    assert!(again.removed.lock().unwrap().contains(&held));
}

#[tokio::test]
async fn a_dry_run_deletes_nothing_and_says_what_would_go() {
    let listed = forty();
    let planned = plan(&listed, Retention::default());
    let pruner = Recording::default();
    let outcome = apply_retention(&planned, &listed, &[], &pruner, true).await;
    assert!(outcome.dry_run);
    assert!(pruner.removed.lock().unwrap().is_empty());
    assert!(outcome.deleted.is_empty());
    assert!(!outcome.would_delete.is_empty());
}

#[tokio::test]
async fn an_unlisted_key_is_never_deleted() {
    let listed = forty();
    let mut planned = plan(&listed, Retention::default());
    let stranger = (9, 999);
    planned.prune.push(stranger);
    let pruner = Recording::default();
    let outcome = apply_retention(&planned, &listed, &[], &pruner, false).await;
    assert!(!pruner.removed.lock().unwrap().contains(&stranger));
    assert_eq!(outcome.skipped_unlisted, vec![stranger]);
}

#[tokio::test]
async fn a_failed_removal_is_counted_and_the_rest_go_on() {
    let listed = forty();
    let planned = plan(&listed, Retention::default());
    let first = planned.prune[0];
    let pruner = Recording {
        fail_at: Some(first),
        ..Recording::default()
    };
    let outcome = apply_retention(&planned, &listed, &[], &pruner, false).await;
    assert_eq!(
        outcome.failed,
        vec![(first, "journal_snapshot_pg.prune_failed")]
    );
    assert_eq!(outcome.deleted.len() + 1, planned.prune.len());
}

#[tokio::test]
async fn a_store_that_refuses_is_reported_once_and_the_rest_are_not_tried() {
    let listed = forty();
    let planned = plan(&listed, Retention::default());
    let pruner = Recording {
        refuse: true,
        ..Recording::default()
    };
    let outcome = apply_retention(&planned, &listed, &[], &pruner, false).await;
    assert!(outcome.store_refused);
    assert!(outcome.deleted.is_empty());
    assert!(outcome.failed.is_empty());
}

#[tokio::test]
async fn the_real_journal_store_keeps_its_snapshots_and_the_pruner_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let store = FolderStore::new(dir.path()).unwrap();
    let index = Layout::snapshot_object(SITE, 1, 5, INDEX_NAME)
        .unwrap()
        .to_key();
    let part = Layout::snapshot_object(SITE, 1, 5, "part-0001.bin")
        .unwrap()
        .to_key();
    store.put_new(&index, b"index".to_vec()).await.unwrap();
    store.put_new(&part, b"part".to_vec()).await.unwrap();
    let pruner = StorePruner::new(FolderStore::new(dir.path()).unwrap(), SITE);
    // The store's rule stands: no deletion under `snapshots/`, and the pruner reports it.
    assert_eq!(pruner.remove((1, 5)).await, Err(PruneError::StoreRefuses));
    assert!(store.get(&index).await.unwrap().is_some());
    assert!(store.get(&part).await.unwrap().is_some());
}
