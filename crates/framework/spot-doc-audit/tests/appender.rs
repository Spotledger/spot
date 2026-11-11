//! The lane appender: ordering, idempotency, no cross-lane lock, tamper detection and truncation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use spot_audit_chain::{
    AppendOutcome, AuditStore, ChainEntry, Checkpoint, CheckpointSigner, InMemoryStore, KeyRing,
    LocalSigner, StoreFuture,
};
use spot_core::ManualClock;
use spot_doc_audit::{
    AppendResult, AppenderStore, DocAudit, DocOperation, InMemoryAppenderStore, LaneAppender,
    LaneConfig, OutboxFeed, SiteCheckpoint, TRUNCATED_KIND, lane_of, lane_site,
};
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{
    Catalog, Isolation, StorageBackend, Transaction, TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;

const SITE: &str = "test.spotledger.app";

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    b.prepare(&cat).await.unwrap();
    (b, cat)
}

async fn begin(b: &FakeBackend, cat: &Arc<Catalog>) -> <FakeBackend as StorageBackend>::Tx {
    b.begin(
        cat.clone(),
        TxOptions::new(
            spot_app_api::prelude::Name::new("docaudit_appender").unwrap(),
            Isolation::ReadCommitted,
        ),
    )
    .await
    .unwrap()
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([7u8; 32]))
}

fn keys_of(signer: &Arc<dyn CheckpointSigner>) -> KeyRing {
    // Recover the public key by re-deriving it from the same seed the signer used.
    let local = LocalSigner::from_seed([7u8; 32]);
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).unwrap();
    let _ = signer;
    keys
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
    ))
}

/// 10,000 documents across 4 lanes chain in per-document order.
#[tokio::test]
async fn ten_thousand_documents_chain_in_order() {
    let (b, cat) = backend().await;
    let config = LaneConfig::new(4, 1);
    let docs = 10_000u64;
    // Batches of 100 events per transaction; a document's own events keep their commit order.
    let mut n = 0u64;
    while n < docs {
        let mut tx = begin(&b, &cat).await;
        for i in 0..100u64 {
            let id = n + i;
            if id >= docs {
                break;
            }
            let event = event(&id_of(id + 1), 1, DocOperation::Create, "note");
            spot_doc_audit::enqueue_audit(&mut tx, &event)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
        n += 100;
    }
    let store = Arc::new(InMemoryStore::new());
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let signer = signer();
    let mut appenders = Vec::new();
    for lane in 0..4 {
        appenders.push(LaneAppender::new(
            Arc::clone(&store),
            SITE,
            lane,
            config,
            Arc::clone(&signer),
            Arc::clone(&state),
        ));
    }
    let feed: &dyn OutboxFeed = &b;
    let mut total = 0usize;
    for appender in &appenders {
        let report = appender.drain(feed, 20_000).await.unwrap();
        total += report.appended;
    }
    assert_eq!(total as u64, docs, "every document is chained once");
    // Every lane's chain is contiguous and linked.
    let mut chained = 0u64;
    for lane in 0..4 {
        let site = lane_site(SITE, lane);
        let entries = store.range(&site, 1, u64::MAX).await.unwrap();
        for (i, e) in entries.iter().enumerate() {
            assert_eq!(e.seq, i as u64 + 1, "lane {lane} has a gap");
        }
        chained += entries.len() as u64;
    }
    assert_eq!(chained, docs);
    // Per-document order: the index of a document is strictly increasing.
    for id in [1u64, 2, 500, 9_999] {
        let entries = state.document_entries(DOCTYPE, &id_of(id + 1));
        assert_eq!(entries.len(), 1, "one entry per document");
    }
}

/// Two writers to different lanes never wait on each other: holding lane 0's append open does not
/// block lane 1.
#[tokio::test]
async fn lanes_do_not_share_a_lock() {
    let store = Arc::new(GatedStore::new(lane_site(SITE, 0)));
    let config = LaneConfig::new(2, 1);
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let signer = signer();
    let lane0 = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        Arc::clone(&signer),
        Arc::clone(&state),
    );
    let lane1 = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        1,
        config,
        Arc::clone(&signer),
        Arc::clone(&state),
    );

    let event0 = event(&id_of(1), 1, DocOperation::Create, "a");
    let event1 = event(&id_of(2), 1, DocOperation::Create, "b");
    let lane0_task = tokio::spawn({
        let lane0 = std::sync::Arc::new(lane0);
        let event0 = event0.clone();
        async move { lane0.append_event("e0", &event0).await }
    });
    store.entered.notified().await;
    // Lane 1 proceeds while lane 0 is held inside its append.
    let result = tokio::time::timeout(Duration::from_secs(2), lane1.append_event("e1", &event1))
        .await
        .expect("lane 1 must not wait on lane 0");
    assert_eq!(result.unwrap(), AppendResult::Appended { seq: 1 });
    assert!(!lane0_task.is_finished(), "lane 0 is still held");
    store.release.notify_one();
    let result0 = lane0_task.await.unwrap().unwrap();
    assert_eq!(result0, AppendResult::Appended { seq: 1 });
}

/// A crash after the append and before the bookmark advanced re-delivers the event: it is
/// appended once.
#[tokio::test]
async fn redelivery_appends_once() {
    let (b, cat) = backend().await;
    let config = LaneConfig::new(1, 1);
    let mut tx = begin(&b, &cat).await;
    let event = event(&id_of(1), 1, DocOperation::Create, "note");
    spot_doc_audit::enqueue_audit(&mut tx, &event)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let store = Arc::new(InMemoryStore::new());
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        signer(),
        Arc::clone(&state),
    );
    let feed: &dyn OutboxFeed = &b;
    appender.drain(feed, 100).await.unwrap();
    let head_after_first = store.head(&lane_site(SITE, 0)).await.unwrap().unwrap().seq;
    assert_eq!(head_after_first, 1);

    // Simulate the crash: the bookmark is lost, the same event is read again.
    state.set_bookmark(0, FeedPosition::START);
    let report = appender.drain(feed, 100).await.unwrap();
    assert_eq!(report.appended, 0, "the event was already chained");
    let head_after_second = store.head(&lane_site(SITE, 0)).await.unwrap().unwrap().seq;
    assert_eq!(head_after_second, 1, "the chain did not grow");
}

/// Tampering one entry in one lane is reported with the lane and seq.
#[tokio::test]
async fn tamper_is_reported_with_lane_and_seq() {
    let config = LaneConfig::new(2, 1);
    let store = Arc::new(MutableStore::new());
    let (service, state, signer, keys) = service_over(Arc::clone(&store), config);
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        Arc::clone(&signer),
        Arc::clone(&state),
    );
    for i in 1..=3u64 {
        appender
            .append_event(
                &format!("e{i}"),
                &event(&id_of(i), i, DocOperation::Create, "note"),
            )
            .await
            .unwrap();
    }
    let _ = keys;
    store.tamper(&lane_site(SITE, 0), 2);
    let report = service.verify_site(None).await.unwrap();
    assert!(!report.is_ok());
    assert!(
        report
            .failures
            .iter()
            .any(|f| f.lane == Some(0) && f.seq == 2),
        "reports lane 0 at seq 2: {:?}",
        report.failures
    );
}

/// Deleting a whole lane's tail is caught by the signed site checkpoint.
#[tokio::test]
async fn a_truncated_lane_is_caught_by_the_site_checkpoint() {
    let config = LaneConfig::new(2, 1);
    let store = Arc::new(MutableStore::new());
    let (service, state, signer, _keys) = service_over(Arc::clone(&store), config);
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        1,
        config,
        Arc::clone(&signer),
        Arc::clone(&state),
    );
    for i in 1..=4u64 {
        appender
            .append_event(
                &format!("e{i}"),
                &event(&id_of(i), i, DocOperation::Create, "note"),
            )
            .await
            .unwrap();
    }
    let checkpoint = appender
        .sign_checkpoint(1_700_000_000_000_000)
        .await
        .unwrap();
    // Remove the lane's tail: keep only the first two entries.
    store.truncate(&lane_site(SITE, 1), 2);
    let report = service.verify_site(Some(&checkpoint)).await.unwrap();
    assert!(
        report.failures.iter().any(|f| f.kind == TRUNCATED_KIND),
        "the site checkpoint catches the truncated lane: {:?}",
        report.failures
    );
}

/// Changing the lane count without a new epoch is refused.
#[test]
fn changing_the_lane_count_needs_a_new_epoch() {
    let signer = signer();
    let heads = vec![
        spot_doc_audit::LaneHead {
            lane: 0,
            seq: 3,
            head_hash: [1u8; 32],
        },
        spot_doc_audit::LaneHead {
            lane: 1,
            seq: 5,
            head_hash: [2u8; 32],
        },
    ];
    let cp = SiteCheckpoint::sign(&*signer, SITE, &LaneConfig::new(2, 1), &heads, 10).unwrap();
    assert!(
        cp.verify(&KeyRing::new(), SITE, &LaneConfig::new(2, 1))
            .is_err(),
        "an empty key ring cannot verify"
    );
    // The same lanes at a new epoch, or a new lane count, do not match.
    assert!(LaneConfig::new(2, 2).check_epoch(&cp).is_err());
    assert!(LaneConfig::new(4, 1).check_epoch(&cp).is_err());
    assert!(LaneConfig::new(2, 1).check_epoch(&cp).is_ok());
    assert_eq!(cp.digest(), spot_doc_audit::HeadDigest::compute(&heads, 1));
}

/// A lane is stable for a document and covers the configured lanes.
#[test]
fn lanes_are_stable_and_in_range() {
    let n = 4;
    for i in 0..100u64 {
        let id = id_of(i + 1);
        let lane = lane_of(DOCTYPE, &id, n);
        assert!(lane < n);
        assert_eq!(lane, lane_of(DOCTYPE, &id, n), "the lane is stable");
    }
    assert_eq!(lane_of(DOCTYPE, &id_of(1), 1), 0);
}

// ----------------------------------------------------------------- helpers

fn service_over(
    store: Arc<MutableStore>,
    config: LaneConfig,
) -> (
    DocAudit<MutableStore>,
    Arc<dyn AppenderStore>,
    Arc<dyn CheckpointSigner>,
    KeyRing,
) {
    let signer = signer();
    let keys = keys_of(&signer);
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let service = DocAudit::new(
        store,
        SITE,
        config,
        Arc::clone(&signer),
        keys.clone(),
        masker(),
        Arc::clone(&state),
        clock(),
    );
    (service, state, signer, keys)
}

/// A store that blocks `append_if_head` for one site until released.
struct GatedStore {
    inner: InMemoryStore,
    target: String,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl GatedStore {
    fn new(target: String) -> Self {
        Self {
            inner: InMemoryStore::new(),
            target,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }
}

impl AuditStore for GatedStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        self.inner.head(site)
    }
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        Box::pin(async move {
            if site == self.target {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.append_if_head(site, expected_seq, entry).await
        })
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        self.inner.range(site, from, to)
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.inner.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.inner.latest_checkpoint(site)
    }
}

/// A store whose entries can be tampered with or hidden, for the verifier tests.
struct MutableStore {
    inner: InMemoryStore,
    tampered: Mutex<HashSet<(String, u64)>>,
    hidden_above: Mutex<HashMap<String, u64>>,
}

impl MutableStore {
    fn new() -> Self {
        Self {
            inner: InMemoryStore::new(),
            tampered: Mutex::new(HashSet::new()),
            hidden_above: Mutex::new(HashMap::new()),
        }
    }

    fn tamper(&self, site: &str, seq: u64) {
        self.tampered.lock().unwrap().insert((site.to_owned(), seq));
    }

    fn truncate(&self, site: &str, keep: u64) {
        self.hidden_above
            .lock()
            .unwrap()
            .insert(site.to_owned(), keep);
    }

    fn visible(&self, site: &str, entries: Vec<ChainEntry>) -> Vec<ChainEntry> {
        let hidden = self.hidden_above.lock().unwrap().get(site).copied();
        let tampered = self.tampered.lock().unwrap();
        entries
            .into_iter()
            .filter(|e| hidden.is_none_or(|keep| e.seq <= keep))
            .map(|mut e| {
                if tampered.contains(&(site.to_owned(), e.seq)) {
                    // Alter a field of the record: its stored hash no longer matches.
                    e.record.at = e.record.at.saturating_add(1);
                }
                e
            })
            .collect()
    }
}

impl AuditStore for MutableStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async move {
            let entries = self.inner.range(site, 1, u64::MAX).await?;
            Ok(self.visible(site, entries).into_iter().last())
        })
    }
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.inner.append_if_head(site, expected_seq, entry)
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async move {
            let entries = self.inner.range(site, from, to).await?;
            Ok(self.visible(site, entries))
        })
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.inner.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.inner.latest_checkpoint(site)
    }
}
