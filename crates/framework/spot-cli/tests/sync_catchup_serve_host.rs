//! Sync catch-up served and applied (WP-RTB-02.T14): the module's order, settings and limits, and
//! the snapshot provider's refusal while no snapshot is verified. The serving and applying logic
//! is proved over fakes in `crates/framework/spot-sync-catchup-host/tests/serve.rs`; a hub and an
//! edge on real databases are the end-to-end scenarios (WP-RTB-02.T9, WP-RTB-03.T4).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_cli::services::journal::{FolderStore, JournalKeys};
use spot_cli::services::sync_catchup_serve::{ORDER, Settings, StoreSnapshots};
use spot_cli::services::{journal_wiring, sync_catchup};
use spot_journal_snapshot_pg::SnapshotKeyring;
use spot_secrets::{KeyProvider, MemoryKeyring};
use spot_sync_catchup_host::{ServeError, SnapshotProvider};

#[test]
#[allow(clippy::assertions_on_constants)]
fn the_order_is_after_the_director_and_the_journal_wiring() {
    assert!(sync_catchup::ORDER < ORDER);
    assert!(journal_wiring::ORDER < ORDER);
    assert!(spot_cli::services::registered_names().contains(&"sync_catchup_serve"));
}

#[test]
fn the_settings_default_to_the_closure_limits_and_refuse_nonsense() {
    let defaults = Settings::default();
    assert!(defaults.enabled);
    let limits = defaults.limits().unwrap();
    assert_eq!(limits.max_records, 500);
    let parsed: Settings = toml::from_str("chunk_max_records = 100").unwrap();
    assert_eq!(parsed.limits().unwrap().max_records, 100);
    assert!(toml::from_str::<Settings>("bogus = 1").is_err());
    let zero: Settings = toml::from_str("chunk_max_records = 0").unwrap();
    assert!(zero.limits().is_err());
    // The hard limit may not be smaller than a chunk.
    let small: Settings = toml::from_str("hard_limit_records = 10").unwrap();
    assert!(small.limits().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_gets_no_base_while_no_snapshot_is_verified() {
    let dir = tempfile::tempdir().unwrap();
    let provider: Arc<dyn KeyProvider> = Arc::new(MemoryKeyring::new("site-key").unwrap());
    let _journal = JournalKeys::derive(Arc::clone(&provider)).await.unwrap();
    let keys = SnapshotKeyring::derive(&provider).await.unwrap().keys();
    let store = Arc::new(FolderStore::new(dir.path()).unwrap());
    let snapshots = StoreSnapshots::new(
        store,
        "acme".to_owned(),
        Arc::new(keys),
        tokio::runtime::Handle::current(),
    );
    // The provider blocks on the runtime, as it does on the blocking pool.
    let opened = tokio::task::spawn_blocking(move || snapshots.open().map(|_| ()))
        .await
        .unwrap();
    assert_eq!(opened, Err(ServeError::NoVerifiedSnapshot));
}
