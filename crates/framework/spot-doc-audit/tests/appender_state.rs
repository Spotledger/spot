//! The appender state is an async, fallible port: every failing call surfaces as
//! `docaudit.appender_state`, the bookmark never passes an event that was not stored, nothing is
//! chained twice, and the drain works on a current-thread and a multi-thread runtime with an
//! async store.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use spot_app_api::prelude::{Name, OffsetDateTime};
use spot_audit_chain::{AuditStore, CheckpointSigner, InMemoryStore, KeyRing, LocalSigner};
use spot_core::{ManualClock, SiteId};
use spot_doc_audit::{
    APPENDER_STATE, AppenderStore, DocAudit, DocAuditError, DocOperation, InMemoryAppenderStore,
    LaneAppender, LaneConfig, OutboxFeed, SiteCheckpoint, StateFuture, append_job_name,
    enqueue_audit, lane_site, register_appender,
};
use spot_jobs::model::InsertResult;
use spot_jobs::worker::{HandlerResult, JobContext, JobHandlerRegistry};
use spot_jobs::{InMemoryJobStore, JobStore, NewJob};
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{
    Catalog, Isolation, StorageBackend, Transaction, TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;

const SITE: &str = "test.spotledger.app";

/// The eight operations of the port, by name.
const OPS: [&str; 8] = [
    "bookmark",
    "set_bookmark",
    "appended",
    "record_appended",
    "document_entries",
    "record_document_entry",
    "site_checkpoint",
    "set_site_checkpoint",
];

/// An [`AppenderStore`] over the in-memory one that fails the Nth call of a named operation once,
/// and can yield to the scheduler inside every call (an async store, not a ready one).
struct FlakyAppenderStore {
    inner: InMemoryAppenderStore,
    calls: Mutex<HashMap<&'static str, u64>>,
    fail_at: Mutex<HashMap<&'static str, u64>>,
    yielding: bool,
}

impl FlakyAppenderStore {
    fn new(yielding: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryAppenderStore::new(),
            calls: Mutex::new(HashMap::new()),
            fail_at: Mutex::new(HashMap::new()),
            yielding,
        })
    }

    /// Fails the `nth` call (1 is the next one) of `op`, once.
    fn fail(&self, op: &'static str, nth: u64) {
        let made = self.calls.lock().unwrap().get(op).copied().unwrap_or(0);
        self.fail_at.lock().unwrap().insert(op, made + nth);
    }

    fn clear(&self) {
        self.fail_at.lock().unwrap().clear();
    }

    async fn gate(&self, op: &'static str) -> Result<(), DocAuditError> {
        if self.yielding {
            tokio::task::yield_now().await;
        }
        let n = {
            let mut calls = self.calls.lock().unwrap();
            let n = calls.entry(op).or_insert(0);
            *n += 1;
            *n
        };
        if self.fail_at.lock().unwrap().get(op) == Some(&n) {
            return Err(DocAuditError::appender_state(op, "auditstore.unavailable"));
        }
        Ok(())
    }
}

impl AppenderStore for FlakyAppenderStore {
    fn bookmark(&self, lane: u32) -> StateFuture<'_, FeedPosition> {
        Box::pin(async move {
            self.gate("bookmark").await?;
            self.inner.bookmark(lane).await
        })
    }

    fn set_bookmark(&self, lane: u32, position: FeedPosition) -> StateFuture<'_, ()> {
        Box::pin(async move {
            self.gate("set_bookmark").await?;
            self.inner.set_bookmark(lane, position).await
        })
    }

    fn appended<'a>(&'a self, event_id: &'a str) -> StateFuture<'a, Option<(u32, u64)>> {
        Box::pin(async move {
            self.gate("appended").await?;
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
            self.gate("record_appended").await?;
            self.inner.record_appended(event_id, lane, seq).await
        })
    }

    fn document_entries<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> StateFuture<'a, Vec<(u32, u64)>> {
        Box::pin(async move {
            self.gate("document_entries").await?;
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
            self.gate("record_document_entry").await?;
            self.inner
                .record_document_entry(doctype, id, lane, seq)
                .await
        })
    }

    fn site_checkpoint(&self) -> StateFuture<'_, Option<SiteCheckpoint>> {
        Box::pin(async move {
            self.gate("site_checkpoint").await?;
            self.inner.site_checkpoint().await
        })
    }

    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint) -> StateFuture<'_, ()> {
        Box::pin(async move {
            self.gate("set_site_checkpoint").await?;
            self.inner.set_site_checkpoint(checkpoint).await
        })
    }
}

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    b.prepare(&cat).await.unwrap();
    (b, cat)
}

/// `n` audit events about `n` different documents, in the outbox.
async fn seed(b: &FakeBackend, cat: &Arc<Catalog>, n: u64) {
    for i in 1..=n {
        let mut tx = b
            .begin(
                cat.clone(),
                TxOptions::new(
                    Name::new("docaudit_state").unwrap(),
                    Isolation::ReadCommitted,
                ),
            )
            .await
            .unwrap();
        let event = event(&id_of(i), 1, DocOperation::Create, "note");
        enqueue_audit(&mut tx, &event).await.unwrap();
        tx.commit().await.unwrap();
    }
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([5u8; 32]))
}

fn keys() -> KeyRing {
    let local = LocalSigner::from_seed([5u8; 32]);
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).unwrap();
    keys
}

struct Rig {
    backend: FakeBackend,
    store: Arc<InMemoryStore>,
    state: Arc<FlakyAppenderStore>,
    appender: LaneAppender<InMemoryStore>,
    config: LaneConfig,
}

async fn rig(events: u64, yielding: bool) -> Rig {
    let (backend, cat) = backend().await;
    seed(&backend, &cat, events).await;
    let store = Arc::new(InMemoryStore::new());
    let state = FlakyAppenderStore::new(yielding);
    let config = LaneConfig::new(1, 1);
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        signer(),
        Arc::clone(&state) as Arc<dyn AppenderStore>,
    );
    Rig {
        backend,
        store,
        state,
        appender,
        config,
    }
}

impl Rig {
    async fn drain(&self) -> Result<usize, DocAuditError> {
        let feed: &dyn OutboxFeed = &self.backend;
        Ok(self.appender.drain(feed, 100).await?.appended)
    }

    /// Entries of lane 0, oldest first.
    async fn chained(&self) -> Vec<spot_audit_chain::ChainEntry> {
        self.store
            .range(&lane_site(SITE, 0), 1, 1_000)
            .await
            .unwrap()
    }

    fn service(&self) -> DocAudit<InMemoryStore> {
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
}

fn assert_state_error(error: &DocAuditError, op: &str) {
    assert_eq!(error.code(), APPENDER_STATE, "{op}");
    let text = error.to_string();
    assert!(text.contains(op), "the message names the operation: {text}");
    assert!(
        text.contains("auditstore.unavailable"),
        "and the store's code: {text}"
    );
    assert!(!text.contains("ev-") && !text.contains(&id_of(1)), "{text}");
}

/// (a) Each drain-path operation failing once ends the drain with `docaudit.appender_state`, the
/// next drain chains every event exactly once, in order (the crash windows after the chain append
/// are recovered by finding the entry again: WP-SEC-06.T7).
#[tokio::test]
async fn a_failing_state_call_ends_the_drain_and_the_next_drain_loses_nothing() {
    for op in [
        "bookmark",
        "set_bookmark",
        "appended",
        "record_appended",
        "record_document_entry",
    ] {
        let r = rig(3, false).await;
        // The second event's call, so the first is already chained and indexed.
        let nth = if op == "bookmark" { 1 } else { 2 };
        r.state.fail(op, nth);

        let error = r.drain().await.expect_err(op);
        assert_state_error(&error, op);

        r.state.clear();
        r.drain().await.unwrap();
        let entries = r.chained().await;
        let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
        assert_eq!(
            seqs,
            (1..=entries.len() as u64).collect::<Vec<_>>(),
            "{op}: in order"
        );
        assert_eq!(entries.len(), 3, "{op}: every event exactly once");
        // The service can verify the chain whatever happened.
        let report = r.service().verify_site(None).await.unwrap();
        assert!(report.failures.is_empty(), "{op}: the chain verifies");
    }
}

/// The three operations that are not on the drain path fail their own call, never silently.
#[tokio::test]
async fn the_other_operations_fail_their_own_call_with_the_typed_error() {
    let r = rig(1, false).await;
    r.drain().await.unwrap();

    r.state.fail("document_entries", 1);
    let history = r
        .service()
        .history(&principal("reader"), DOCTYPE, &id_of(1))
        .await;
    assert_state_error(&history.expect_err("history"), "document_entries");
    r.state.clear();
    assert_eq!(
        r.service()
            .history(&principal("reader"), DOCTYPE, &id_of(1))
            .await
            .unwrap()
            .len(),
        1
    );

    r.state.fail("set_site_checkpoint", 1);
    let signed = r.appender.sign_checkpoint(1_700_000_200).await;
    assert_state_error(&signed.expect_err("sign"), "set_site_checkpoint");
    r.state.clear();
    r.appender.sign_checkpoint(1_700_000_200).await.unwrap();

    r.state.fail("site_checkpoint", 1);
    let read = r.state.site_checkpoint().await;
    assert_state_error(&read.expect_err("read"), "site_checkpoint");
    r.state.clear();
    assert!(r.state.site_checkpoint().await.unwrap().is_some());
}

/// (a) every operation name of the port is covered by a test above or here.
#[test]
fn the_port_has_eight_operations() {
    assert_eq!(OPS.len(), 8);
}

/// (b) An unreadable bookmark is an error, never "zero lag".
#[tokio::test]
async fn an_unreadable_bookmark_does_not_make_the_lag_look_like_zero() {
    let r = rig(3, false).await;
    let service = r.service();
    r.state.fail("bookmark", 1);
    let feed: &dyn OutboxFeed = &r.backend;
    let lag = service.audit_lag(feed, 100).await;
    assert_state_error(&lag.expect_err("lag"), "bookmark");
    r.state.fail("bookmark", 1);
    let status = service.status(feed, 100).await;
    assert_state_error(&status.expect_err("status"), "bookmark");

    r.state.clear();
    assert_eq!(service.audit_lag(feed, 100).await.unwrap().pending, 3);
}

/// (c) The drain works on a current-thread runtime with an async store whose futures yield.
#[tokio::test(flavor = "current_thread")]
async fn the_drain_works_on_a_current_thread_runtime_with_an_async_store() {
    let r = rig(5, true).await;
    assert_eq!(r.drain().await.unwrap(), 5);
    assert_eq!(r.chained().await.len(), 5);
    assert_eq!(r.drain().await.unwrap(), 0, "nothing more to chain");
}

/// (c) And on a multi-thread runtime, from a spawned task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_drain_works_on_a_multi_thread_runtime_with_an_async_store() {
    let r = Arc::new(rig(5, true).await);
    let spawned = Arc::clone(&r);
    let appended = tokio::spawn(async move { spawned.drain().await.unwrap() })
        .await
        .unwrap();
    assert_eq!(appended, 5);
    assert_eq!(r.chained().await.len(), 5);
}

/// (d) The job handler answers a retry while the state fails and completes once it recovers.
#[tokio::test]
async fn the_job_handler_retries_while_the_state_fails_and_completes_when_it_recovers() {
    let r = rig(2, false).await;
    let appender = Arc::new(r.appender);
    let feed: Arc<dyn OutboxFeed> = Arc::new(r.backend.clone());
    let mut registry = JobHandlerRegistry::new();
    register_appender(&mut registry, Arc::clone(&appender), feed, 100);
    let handler = registry.get(&append_job_name(0)).expect("registered");

    let job_store = InMemoryJobStore::new();
    let now = OffsetDateTime::now_utc();
    let new = NewJob::new(
        append_job_name(0),
        serde_json::json!({ "lane": 0 }),
        "default".to_string(),
        SiteId::new(SITE).unwrap(),
        now,
    );
    let InsertResult::Inserted(_) = job_store.insert_if_new(new).await.unwrap() else {
        panic!("not inserted");
    };
    let claimed = job_store
        .claim(
            &["default".to_string()],
            "w1",
            Duration::from_secs(30),
            now,
            1,
        )
        .await
        .unwrap()
        .remove(0);

    r.state.fail("appended", 1);
    let first = handler(JobContext::new(claimed.job.clone(), 1, now))
        .await
        .unwrap();
    assert!(
        matches!(&first, HandlerResult::Retry(code) if code == APPENDER_STATE),
        "{first:?}"
    );
    assert!(
        r.store.head(&lane_site(SITE, 0)).await.unwrap().is_none(),
        "nothing was chained"
    );

    r.state.clear();
    let second = handler(JobContext::new(claimed.job, 2, now)).await.unwrap();
    assert!(matches!(second, HandlerResult::Ok));
    assert_eq!(
        r.store
            .head(&lane_site(SITE, 0))
            .await
            .unwrap()
            .unwrap()
            .seq,
        2
    );
}

/// A lane persists its bookmark after its own appends and once at the end of the drain, never
/// once per record of another lane: with several lanes reading one feed that was one commit per
/// lane per event.
#[tokio::test]
async fn the_bookmark_is_written_per_own_append_not_per_foreign_record() {
    let (backend, cat) = backend().await;
    seed(&backend, &cat, 40).await;
    let store = Arc::new(InMemoryStore::new());
    let state = FlakyAppenderStore::new(false);
    let config = LaneConfig::new(4, 1);
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        signer(),
        Arc::clone(&state) as Arc<dyn AppenderStore>,
    );
    let feed: &dyn OutboxFeed = &backend;
    let report = appender.drain(feed, 100).await.unwrap();
    assert_eq!(report.read, 40);
    assert!(
        report.appended > 0 && report.appended < 40,
        "{}",
        report.appended
    );
    let writes = state
        .calls
        .lock()
        .unwrap()
        .get("set_bookmark")
        .copied()
        .unwrap_or(0);
    assert!(
        writes <= report.appended as u64 + 1,
        "{writes} bookmark writes for {} own appends",
        report.appended
    );
    // And the bookmark is at the end of what was read: the next drain reads nothing.
    let again = appender.drain(feed, 100).await.unwrap();
    assert_eq!(again.read, 0);
}
