//! The sync and rebuild wiring (WP-RTB-01.T10): the pin source never believes an old count, the
//! hub stream queues and refuses at its capacity, the invariant suite fails closed with no
//! checks, the site guard refuses while the shipper's lease is live and takes a maintenance lease,
//! and the journal side reads verified manifests only. No database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::journal::{FolderStore, JournalKeys, JournalStore};
use spot_cli::services::sync_process::{HubStream, PinSource};
use spot_cli::services::sync_rebuild_wiring::{
    CachedPins, InvariantCheck, InvariantSuite, LeaseGuard, QueuedHubStream, Settings, StoreJournal,
};
use spot_cli::services::{node_role, sync_process, sync_rebuild_wiring};
use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::{Clock, ManualClock};
use spot_journal_manifest::{Manifest, SegmentSummary};
use spot_journal_rebuild_host::{
    Fut, HostError, InvariantResult, Invariants, JournalSide, SiteGuard,
};
use spot_journal_snapshot_pg::SnapshotKeyring;
use spot_journal_store::Layout;
use spot_secrets::{KeyProvider, MemoryKeyring};
use spot_sync_process::{CompatClass, ProcessRecord};
use time::macros::datetime;

const SITE: &str = "acme";

fn record(version: u64) -> ProcessRecord {
    ProcessRecord::new(
        SITE,
        "order_to_cash",
        version,
        1,
        CompatClass::Compatible,
        // The payload names the process it defines (a record whose payload does not is refused).
        &format!(r#"{{"process":"order_to_cash","version":{version}}}"#),
    )
    .unwrap()
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn the_order_is_before_process_distribution_and_after_the_role() {
    assert!(node_role::ORDER < sync_rebuild_wiring::ORDER);
    assert!(sync_rebuild_wiring::ORDER < sync_process::ORDER);
    assert!(spot_cli::services::registered_names().contains(&"sync_rebuild_wiring"));
}

#[test]
fn settings_default_on_and_refuse_unknown_keys() {
    let defaults = Settings::default();
    assert!(defaults.enabled);
    assert_eq!(defaults.pin_refresh_seconds, 5);
    assert_eq!(defaults.pin_max_age_seconds, 30);
    assert_eq!(defaults.queue_capacity, 1000);
    let parsed: Settings = toml::from_str("queue_capacity = 10").unwrap();
    assert_eq!(parsed.queue_capacity, 10);
    assert!(toml::from_str::<Settings>("bogus = 1").is_err());
}

#[test]
fn the_pin_source_honours_the_pin_and_never_believes_an_old_count() {
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-13 08:00:00 UTC)));
    let pins = CachedPins::new(
        Arc::clone(&clock) as Arc<dyn Clock>,
        Duration::from_secs(30),
    );
    // Never read: one Case counts as pinned, so nothing is retired.
    assert_eq!(pins.pins("order_to_cash", 1), 1);
    let now = i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap();
    let mut counts = HashMap::new();
    counts.insert(("order_to_cash".to_owned(), 1_u64), 3_u64);
    pins.store(now, counts);
    assert_eq!(pins.pins("order_to_cash", 1), 3);
    // A version with no running Case is free to retire.
    assert_eq!(pins.pins("order_to_cash", 2), 0);
    clock.advance(Duration::from_secs(31));
    // The count is stale: every version reads as pinned again.
    assert_eq!(pins.pins("order_to_cash", 2), 1);
    assert_eq!(pins.pins("order_to_cash", 1), 1);
}

#[test]
fn the_hub_stream_queues_and_refuses_at_its_capacity() {
    let stream = QueuedHubStream::new(2);
    let keys = vec!["branch:lahore".to_owned()];
    assert!(stream.emit(&record(1), &keys).is_ok());
    assert!(stream.emit(&record(2), &[]).is_ok());
    assert_eq!(stream.waiting(), 2);
    let refused = stream.emit(&record(3), &[]).unwrap_err();
    assert_eq!(refused, "sync_rebuild_wiring.hub_queue_full");
    assert_eq!(stream.waiting(), 2);
}

struct Held;

impl InvariantCheck for Held {
    fn run(&self) -> Fut<'_, InvariantResult> {
        Box::pin(async {
            Ok(InvariantResult {
                name: "document_counts".to_owned(),
                held: true,
                checked: 7,
                violations: 0,
            })
        })
    }
}

#[tokio::test]
async fn the_invariant_suite_fails_closed_with_no_checks() {
    let empty = InvariantSuite::new(Vec::new()).run().await.unwrap();
    assert_eq!(empty.len(), 1);
    assert!(!empty[0].held);
    assert_eq!(empty[0].name, "sync_rebuild_wiring.no_invariant_registered");
    let suite = InvariantSuite::new(vec![Arc::new(Held)]);
    let results = suite.run().await.unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].held);
    assert_eq!(results[0].checked, 7);
}

async fn coordinator(clock: Arc<dyn Clock>) -> Arc<dyn Coordinator> {
    let node = NodeId::new("rebuild-test".to_owned()).unwrap();
    InProcessCoordinator::start(Arc::new(InMemoryPresence::new()), node, clock)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_rebuild_refuses_while_the_site_is_live_and_holds_the_maintenance_lease() {
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(datetime!(2026-11-13 08:00:00 UTC)));
    let coordinator = coordinator(clock).await;
    // A running node holds the shipper's lease.
    let shipper = coordinator
        .acquire("journal-ship:acme", "node-1", Duration::from_secs(30))
        .await
        .unwrap();
    let guard = LeaseGuard::new(
        Arc::clone(&coordinator),
        "rebuild".to_owned(),
        Duration::from_secs(600),
    );
    assert_eq!(
        guard.ensure_stopped(SITE).await,
        Err(HostError::SiteRunning)
    );
    // The node stops: the rebuild may start and holds the maintenance lease.
    coordinator.release(&shipper).await.unwrap();
    guard.ensure_stopped(SITE).await.unwrap();
    // A second rebuild is refused while the first holds it.
    let other = LeaseGuard::new(
        Arc::clone(&coordinator),
        "other".to_owned(),
        Duration::from_secs(600),
    );
    assert_eq!(
        other.ensure_stopped(SITE).await,
        Err(HostError::SiteRunning)
    );
    guard.renew().await.unwrap();
    guard.release().await;
    other.ensure_stopped(SITE).await.unwrap();
}

async fn journal(dir: &std::path::Path, signer_key: &str, verifier_key: &str) -> StoreJournal {
    let sign: Arc<dyn KeyProvider> = Arc::new(MemoryKeyring::new(signer_key).unwrap());
    let signing = JournalKeys::derive(Arc::clone(&sign)).await.unwrap();
    let store = FolderStore::new(dir).unwrap();
    let first = SegmentSummary::new(1, 1, 5, 5, [1; 32], [0; 32]);
    let manifest = Manifest::first(SITE, 1_000)
        .unwrap()
        .append_segments(&[first])
        .unwrap();
    let signed = signing.signer().sign(&manifest);
    let key = Layout::manifest(SITE, 1).unwrap().to_key();
    store.put_new(&key, signed.to_bytes()).await.unwrap();
    let verify: Arc<dyn KeyProvider> = if signer_key == verifier_key {
        sign
    } else {
        Arc::new(MemoryKeyring::new(verifier_key).unwrap())
    };
    let keys = Arc::new(JournalKeys::derive(Arc::clone(&verify)).await.unwrap());
    let snapshot_keys = SnapshotKeyring::derive(&verify).await.unwrap().keys();
    StoreJournal::new(
        SITE.to_owned(),
        dir.to_path_buf(),
        Vec::new(),
        keys,
        Arc::new(snapshot_keys),
        Arc::new(ManualClock::new(datetime!(2026-11-13 08:00:00 UTC))),
    )
}

#[tokio::test]
async fn the_journal_side_reads_verified_manifests_and_no_others() {
    let dir = tempfile::tempdir().unwrap();
    let side = journal(dir.path(), "site-key", "site-key").await;
    assert_eq!(side.tenant(), SITE);
    let catalog = side.catalog().await.unwrap();
    assert_eq!(catalog.segments.len(), 1);
    assert!(catalog.snapshots.is_empty());
    assert!(catalog.forks.is_empty());
    // `before <timestamp>` has no time index to resolve against.
    assert_eq!(side.resolve_before(1_000).await.unwrap(), None);

    // Keys of another tenant refuse the same journal.
    let other_dir = tempfile::tempdir().unwrap();
    let stranger = journal(other_dir.path(), "site-key", "another-key").await;
    assert_eq!(stranger.catalog().await, Err(HostError::JournalFailed));
}
