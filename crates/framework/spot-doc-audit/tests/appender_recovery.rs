//! Exactly-once audit appends: the chain entry carries the outbox event id, and an appender that
//! restarts after a crash between the chain commit and an index write finds its own entry at the
//! lane head instead of chaining the event a second time.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::*;
use spot_app_api::prelude::{Name, OffsetDateTime};
use spot_audit_chain::{
    AppendOutcome, AuditStore, ChainEntry, Checkpoint, CheckpointSigner, InMemoryStore, KeyRing,
    LocalSigner, PayloadValue, StoreFuture,
};
use spot_core::ManualClock;
use spot_doc_audit::{
    ActorRef, AppendResult, AppenderStore, DiffChange, DiffValue, DocAudit, DocAuditError,
    DocOperation, EVENT_ID_KEY, InMemoryAppenderStore, LaneAppender, LaneConfig, OutboxFeed,
    RECOVERY_WINDOW, SecretAuditEvent, SiteCheckpoint, StateFuture, enqueue_audit, lane_site,
    record_of, record_of_with_id,
};
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{
    Catalog, Isolation, StorageBackend, Transaction, TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;

const SITE: &str = "test.spotledger.app";

/// An [`AppenderStore`] that fails the Nth call of a named operation once (a crash at that point:
/// the test then builds a new appender over the same stores, as a restart does).
struct CrashingAppenderStore {
    inner: InMemoryAppenderStore,
    calls: Mutex<HashMap<&'static str, u64>>,
    crash_at: Mutex<Option<(&'static str, u64)>>,
}

impl CrashingAppenderStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryAppenderStore::new(),
            calls: Mutex::new(HashMap::new()),
            crash_at: Mutex::new(None),
        })
    }

    /// Fails the `nth` call (1 is the next) of `op`.
    fn crash(&self, op: &'static str, nth: u64) {
        let made = self.calls.lock().unwrap().get(op).copied().unwrap_or(0);
        *self.crash_at.lock().unwrap() = Some((op, made + nth));
    }

    fn gate(&self, op: &'static str) -> Result<(), DocAuditError> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            let n = calls.entry(op).or_insert(0);
            *n += 1;
            *n
        };
        if *self.crash_at.lock().unwrap() == Some((op, n)) {
            return Err(DocAuditError::appender_state(op, "auditstore.unavailable"));
        }
        Ok(())
    }
}

impl AppenderStore for CrashingAppenderStore {
    fn bookmark(&self, lane: u32) -> StateFuture<'_, FeedPosition> {
        Box::pin(async move {
            self.gate("bookmark")?;
            self.inner.bookmark(lane).await
        })
    }
    fn set_bookmark(&self, lane: u32, position: FeedPosition) -> StateFuture<'_, ()> {
        Box::pin(async move {
            self.gate("set_bookmark")?;
            self.inner.set_bookmark(lane, position).await
        })
    }
    fn appended<'a>(&'a self, event_id: &'a str) -> StateFuture<'a, Option<(u32, u64)>> {
        Box::pin(async move {
            self.gate("appended")?;
            self.inner.appended(event_id).await
        })
    }
    fn record_appended<'a>(
        &'a self,
        event_id: &'a str,
        lane: u32,
        seq: u64,
    ) -> StateFuture<'a, ()> {
        Box::pin(async move {
            self.gate("record_appended")?;
            self.inner.record_appended(event_id, lane, seq).await
        })
    }
    fn document_entries<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> StateFuture<'a, Vec<(u32, u64)>> {
        Box::pin(async move {
            self.gate("document_entries")?;
            self.inner.document_entries(doctype, id).await
        })
    }
    fn record_document_entry<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
        lane: u32,
        seq: u64,
    ) -> StateFuture<'a, ()> {
        Box::pin(async move {
            self.gate("record_document_entry")?;
            self.inner
                .record_document_entry(doctype, id, lane, seq)
                .await
        })
    }
    fn site_checkpoint(&self) -> StateFuture<'_, Option<SiteCheckpoint>> {
        Box::pin(async move { self.inner.site_checkpoint().await })
    }
    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint) -> StateFuture<'_, ()> {
        Box::pin(async move { self.inner.set_site_checkpoint(checkpoint).await })
    }
}

/// A chain store that counts `head` and `range` calls and can fail them.
struct SpyStore {
    inner: InMemoryStore,
    reads: AtomicUsize,
    fail_reads: AtomicBool,
}

impl SpyStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryStore::new(),
            reads: AtomicUsize::new(0),
            fail_reads: AtomicBool::new(false),
        })
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    fn read_gate(&self) -> Result<(), spot_audit_chain::AuditError> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(spot_audit_chain::AuditError::Store(
                "injected read failure".to_owned(),
            ));
        }
        Ok(())
    }
}

impl AuditStore for SpyStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async move {
            self.read_gate()?;
            self.inner.head(site).await
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
            self.read_gate()?;
            self.inner.range(site, from, to).await
        })
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.inner.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.inner.latest_checkpoint(site)
    }
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([3u8; 32]))
}

fn keys() -> KeyRing {
    let local = LocalSigner::from_seed([3u8; 32]);
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).unwrap();
    keys
}

struct Rig {
    store: Arc<SpyStore>,
    state: Arc<CrashingAppenderStore>,
    config: LaneConfig,
}

impl Rig {
    fn new() -> Rig {
        Rig {
            store: SpyStore::new(),
            state: CrashingAppenderStore::new(),
            config: LaneConfig::new(1, 1),
        }
    }

    /// A fresh appender over the same stores: the process after a restart.
    fn appender(&self) -> LaneAppender<SpyStore> {
        LaneAppender::new(
            Arc::clone(&self.store),
            SITE,
            0,
            self.config,
            signer(),
            Arc::clone(&self.state) as Arc<dyn AppenderStore>,
        )
    }

    fn service(&self) -> DocAudit<SpyStore> {
        DocAudit::new(
            Arc::clone(&self.store),
            SITE,
            self.config,
            signer(),
            keys(),
            masker(),
            Arc::clone(&self.state) as Arc<dyn AppenderStore>,
            Arc::new(ManualClock::new(
                OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap(),
            )),
        )
    }

    async fn entries(&self) -> Vec<ChainEntry> {
        self.store
            .inner
            .range(&lane_site(SITE, 0), 1, 10_000)
            .await
            .unwrap()
    }

    async fn assert_clean(&self) {
        let report = self.service().verify_site(None).await.unwrap();
        assert!(report.failures.is_empty(), "the chain verifies");
    }
}

fn doc_event(n: u64) -> spot_doc_audit::DocAuditEvent {
    event(&id_of(n), 1, DocOperation::Create, "note")
}

fn secret_event(n: u64) -> SecretAuditEvent {
    SecretAuditEvent {
        op: "set".to_owned(),
        doctype: DOCTYPE.to_owned(),
        field: "api_key".to_owned(),
        record: Some(id_of(n)),
        caller: "login_service".to_owned(),
        site: SITE.to_owned(),
        outcome: "ok".to_owned(),
        error: None,
        state: "<set>".to_owned(),
        actor: ActorRef {
            actor: "user:alice".to_owned(),
            on_behalf_of: None,
            component: None,
        },
        at: 1_700_000_000_000_000 + i64::try_from(n).unwrap(),
        subject_doctype: DOCTYPE.to_owned(),
        subject_id: id_of(n),
    }
}

fn event_id_of(entry: &ChainEntry) -> Option<String> {
    match entry.record.payload.get(EVENT_ID_KEY) {
        Some(PayloadValue::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

/// (a) The reported defect: the chain append succeeds, `record_appended` fails, the process
/// restarts and drains again. One entry, the index answers, the document index holds one entry.
#[tokio::test]
async fn a_crash_before_the_event_index_write_does_not_chain_the_event_twice() {
    let rig = Rig::new();
    rig.state.crash("record_appended", 1);
    let error = rig
        .appender()
        .append_event("ev-1", &doc_event(1))
        .await
        .expect_err("the index write fails");
    assert_eq!(error.code(), "docaudit.appender_state");
    assert_eq!(rig.entries().await.len(), 1, "the entry is in the chain");

    let result = rig
        .appender()
        .append_event("ev-1", &doc_event(1))
        .await
        .unwrap();
    assert_eq!(result, AppendResult::AlreadyChained { lane: 0, seq: 1 });
    assert_eq!(rig.entries().await.len(), 1, "no second entry");
    assert_eq!(rig.state.appended("ev-1").await.unwrap(), Some((0, 1)));
    assert_eq!(
        rig.state
            .document_entries(DOCTYPE, &id_of(1))
            .await
            .unwrap(),
        vec![(0, 1)]
    );
    rig.assert_clean().await;
}

/// (b) The second window: the document-index write fails once.
#[tokio::test]
async fn a_crash_before_the_document_index_write_leaves_the_entry_exactly_once_in_the_history() {
    let rig = Rig::new();
    rig.state.crash("record_document_entry", 1);
    rig.appender()
        .append_event("ev-1", &doc_event(1))
        .await
        .expect_err("the document index write fails");

    let result = rig
        .appender()
        .append_event("ev-1", &doc_event(1))
        .await
        .unwrap();
    assert_eq!(result, AppendResult::AlreadyChained { lane: 0, seq: 1 });
    assert_eq!(rig.entries().await.len(), 1);
    assert_eq!(
        rig.state
            .document_entries(DOCTYPE, &id_of(1))
            .await
            .unwrap(),
        vec![(0, 1)],
        "the document history has the entry exactly once"
    );
    let history = rig
        .service()
        .history(&principal("reader"), DOCTYPE, &id_of(1))
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    rig.assert_clean().await;
}

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    b.prepare(&cat).await.unwrap();
    (b, cat)
}

/// (c) A drain whose bookmark write fails: the redelivery is `AlreadyChained`, never a second
/// entry.
#[tokio::test]
async fn a_crash_before_the_bookmark_write_redelivers_without_a_second_entry() {
    let (b, cat) = backend().await;
    let mut tx = b
        .begin(
            cat.clone(),
            TxOptions::new(Name::new("docaudit_rec").unwrap(), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    enqueue_audit(&mut tx, &doc_event(1)).await.unwrap();
    tx.commit().await.unwrap();

    let rig = Rig::new();
    let feed: &dyn OutboxFeed = &b;
    rig.state.crash("set_bookmark", 1);
    rig.appender()
        .drain(feed, 100)
        .await
        .expect_err("the bookmark write fails");
    assert_eq!(rig.state.bookmark(0).await.unwrap(), FeedPosition::START);

    let report = rig.appender().drain(feed, 100).await.unwrap();
    assert_eq!(report.appended, 0, "redelivery is already chained");
    assert_eq!(rig.entries().await.len(), 1);
    rig.assert_clean().await;
}

/// (d) The same windows for a secret event (no document-index row): the index write fails after
/// the chain commit, and the restart adopts the entry.
#[tokio::test]
async fn secret_events_recover_the_same_way() {
    let rig = Rig::new();
    rig.state.crash("record_appended", 1);
    rig.appender()
        .append_secret_event("sev-1", &secret_event(1))
        .await
        .expect_err("the index write fails");
    assert_eq!(rig.entries().await.len(), 1);

    let result = rig
        .appender()
        .append_secret_event("sev-1", &secret_event(1))
        .await
        .unwrap();
    assert_eq!(result, AppendResult::AlreadyChained { lane: 0, seq: 1 });
    assert_eq!(rig.entries().await.len(), 1, "one entry");
    assert_eq!(rig.state.appended("sev-1").await.unwrap(), Some((0, 1)));
    assert!(
        rig.state
            .document_entries(DOCTYPE, &id_of(1))
            .await
            .unwrap()
            .is_empty(),
        "a secret event is not in the document index"
    );
    rig.assert_clean().await;
}

/// (e) A redelivered event the index knows is answered from the index: the chain is not read.
#[tokio::test]
async fn an_indexed_event_costs_no_chain_read() {
    let rig = Rig::new();
    let appender = rig.appender();
    for n in 1..=3 {
        appender
            .append_event(&format!("ev-{n}"), &doc_event(n))
            .await
            .unwrap();
    }
    let before = rig.store.reads();
    let result = appender.append_event("ev-1", &doc_event(1)).await.unwrap();
    assert_eq!(result, AppendResult::AlreadyChained { lane: 0, seq: 1 });
    assert_eq!(rig.store.reads(), before, "no head or range call");
}

/// (f) Recovery matches by id, never by content.
#[tokio::test]
async fn two_events_with_equal_content_and_different_ids_are_both_chained() {
    let rig = Rig::new();
    let appender = rig.appender();
    let event = doc_event(1);
    assert_eq!(
        appender.append_event("ev-a", &event).await.unwrap(),
        AppendResult::Appended { seq: 1 }
    );
    assert_eq!(
        appender.append_event("ev-b", &event).await.unwrap(),
        AppendResult::Appended { seq: 2 }
    );
    let ids: Vec<Option<String>> = rig.entries().await.iter().map(event_id_of).collect();
    assert_eq!(ids, vec![Some("ev-a".to_owned()), Some("ev-b".to_owned())]);
    rig.assert_clean().await;
}

/// (g) An entry written before this change has no id and is never matched.
#[tokio::test]
async fn an_entry_without_an_event_id_is_never_matched_and_still_verifies() {
    let rig = Rig::new();
    // An old entry: appended through the chain with the id-less record.
    let old = rig.appender();
    old.chain()
        .append(record_of(&doc_event(1)).unwrap())
        .await
        .unwrap();
    assert_eq!(event_id_of(&rig.entries().await[0]), None);

    let result = rig
        .appender()
        .append_event("ev-1", &doc_event(1))
        .await
        .unwrap();
    assert_eq!(
        result,
        AppendResult::Appended { seq: 2 },
        "not matched by content"
    );
    rig.assert_clean().await;
}

/// (h) A chain read that fails during recovery fails the call and chains nothing.
#[tokio::test]
async fn a_chain_read_failure_during_recovery_fails_and_chains_nothing() {
    let rig = Rig::new();
    rig.store.fail_reads.store(true, Ordering::SeqCst);
    let error = rig
        .appender()
        .append_event("ev-1", &doc_event(1))
        .await
        .expect_err("the head cannot be read");
    assert_eq!(error.code(), "docaudit.chain");
    assert!(!error.to_string().contains("ev-1"), "{error}");
    rig.store.fail_reads.store(false, Ordering::SeqCst);
    assert!(rig.entries().await.is_empty(), "nothing was chained");
    assert_eq!(rig.state.appended("ev-1").await.unwrap(), None);

    // Healthy again: the event is chained once.
    assert_eq!(
        rig.appender()
            .append_event("ev-1", &doc_event(1))
            .await
            .unwrap(),
        AppendResult::Appended { seq: 1 }
    );
}

/// (i) A payload near the size limit still validates with the id (the compact fallback applies).
#[test]
fn a_payload_near_the_size_limit_validates_with_the_id() {
    let limit = spot_audit_chain::MAX_PAYLOAD_BYTES;
    let id = "0198d3a0-0000-7000-8000-000000000001";
    for size in (limit.saturating_sub(1_500)..limit + 1_500).step_by(37) {
        let mut event = doc_event(1);
        event.diff = vec![DiffChange {
            path: "title".to_owned(),
            before: None,
            after: Some(DiffValue::Text("x".repeat(size))),
        }];
        let with_id = record_of_with_id(&event, id);
        assert!(with_id.is_ok(), "size {size}: {:?}", with_id.err());
        let record = with_id.unwrap();
        assert_eq!(
            record.payload.get(EVENT_ID_KEY),
            Some(&PayloadValue::Text(id.to_owned()))
        );
    }
}

/// (j) The id in the payload is the outbox event id as text, and it appears in no error message.
#[tokio::test]
async fn the_payload_carries_the_outbox_id_as_text_and_no_error_names_it() {
    let (b, cat) = backend().await;
    let mut tx = b
        .begin(
            cat.clone(),
            TxOptions::new(
                Name::new("docaudit_rec2").unwrap(),
                Isolation::ReadCommitted,
            ),
        )
        .await
        .unwrap();
    enqueue_audit(&mut tx, &doc_event(1)).await.unwrap();
    tx.commit().await.unwrap();
    let feed: &dyn OutboxFeed = &b;
    let outbox = feed.read(FeedPosition::START, 10).await.unwrap();
    let outbox_id = outbox[0].event.id.to_string();

    let rig = Rig::new();
    rig.state.crash("record_appended", 1);
    let error = rig.appender().drain(feed, 100).await.expect_err("fails");
    assert!(!error.to_string().contains(&outbox_id), "{error}");
    let entries = rig.entries().await;
    assert_eq!(event_id_of(&entries[0]), Some(outbox_id));
}

/// The recovery window is 32 and the constant is public.
#[test]
fn the_recovery_window_is_bounded() {
    assert_eq!(RECOVERY_WINDOW, 32);
}
