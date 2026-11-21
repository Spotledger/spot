//! The grouped drain (a store that batches): the same chain as the single-event path, in order and
//! exactly once, whichever call of a group fails and however often the drain is repeated.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::*;
use spot_app_api::prelude::Name;
use spot_audit_chain::{AuditStore, CheckpointSigner, InMemoryStore, LocalSigner};
use spot_doc_audit::{
    AppenderStore, DocAuditError, DocOperation, DocumentEntry, InMemoryAppenderStore, LaneAppender,
    LaneConfig, OutboxFeed, SiteCheckpoint, StateFuture, enqueue_audit, lane_site,
};
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{
    Catalog, Isolation, StorageBackend, Transaction, TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;

const SITE: &str = "test.spotledger.app";

/// A batching [`AppenderStore`] over the in-memory one that fails a named call once.
struct Flaky {
    inner: InMemoryAppenderStore,
    fail: Mutex<HashMap<&'static str, u64>>,
    calls: Mutex<HashMap<&'static str, u64>>,
}

impl Flaky {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryAppenderStore::new(),
            fail: Mutex::new(HashMap::new()),
            calls: Mutex::new(HashMap::new()),
        })
    }

    fn fail_nth(&self, op: &'static str, nth: u64) {
        let made = self.calls.lock().unwrap().get(op).copied().unwrap_or(0);
        self.fail.lock().unwrap().insert(op, made + nth);
    }

    fn gate(&self, op: &'static str) -> Result<(), DocAuditError> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            let n = calls.entry(op).or_insert(0);
            *n += 1;
            *n
        };
        if self.fail.lock().unwrap().get(op) == Some(&n) {
            return Err(DocAuditError::appender_state(op, "auditstore.unavailable"));
        }
        Ok(())
    }
}

impl AppenderStore for Flaky {
    fn bookmark(&self, lane: u32) -> StateFuture<'_, FeedPosition> {
        self.inner.bookmark(lane)
    }
    fn set_bookmark(&self, lane: u32, position: FeedPosition) -> StateFuture<'_, ()> {
        Box::pin(async move {
            self.gate("set_bookmark")?;
            self.inner.set_bookmark(lane, position).await
        })
    }
    fn appended<'a>(&'a self, event_id: &'a str) -> StateFuture<'a, Option<(u32, u64)>> {
        self.inner.appended(event_id)
    }
    fn record_appended<'a>(&'a self, id: &'a str, lane: u32, seq: u64) -> StateFuture<'a, ()> {
        self.inner.record_appended(id, lane, seq)
    }
    fn document_entries<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> StateFuture<'a, Vec<(u32, u64)>> {
        self.inner.document_entries(doctype, id)
    }
    fn record_document_entry<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
        lane: u32,
        seq: u64,
    ) -> StateFuture<'a, ()> {
        self.inner.record_document_entry(doctype, id, lane, seq)
    }
    fn site_checkpoint(&self) -> StateFuture<'_, Option<SiteCheckpoint>> {
        self.inner.site_checkpoint()
    }
    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint) -> StateFuture<'_, ()> {
        self.inner.set_site_checkpoint(checkpoint)
    }
    fn batches(&self) -> bool {
        true
    }
    fn appended_many<'a>(
        &'a self,
        event_ids: &'a [String],
    ) -> StateFuture<'a, Vec<Option<(u32, u64)>>> {
        Box::pin(async move {
            self.gate("appended_many")?;
            self.inner.appended_many(event_ids).await
        })
    }
    fn record_many<'a>(
        &'a self,
        documents: &'a [DocumentEntry],
        events: &'a [(String, u32, u64)],
    ) -> StateFuture<'a, ()> {
        Box::pin(async move {
            self.gate("record_many")?;
            self.inner.record_many(documents, events).await
        })
    }
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([5u8; 32]))
}

async fn seeded(n: u64) -> (FakeBackend, Arc<Catalog>) {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    b.prepare(&cat).await.unwrap();
    for i in 1..=n {
        let mut tx = b
            .begin(
                cat.clone(),
                TxOptions::new(Name::new("batched").unwrap(), Isolation::ReadCommitted),
            )
            .await
            .unwrap();
        let event = event(&id_of(i), 1, DocOperation::Create, "note");
        enqueue_audit(&mut tx, &event).await.unwrap();
        tx.commit().await.unwrap();
    }
    (b, cat)
}

struct Rig {
    backend: FakeBackend,
    store: Arc<InMemoryStore>,
    state: Arc<Flaky>,
    appender: LaneAppender<InMemoryStore>,
}

async fn rig(events: u64) -> Rig {
    let (backend, _) = seeded(events).await;
    let store = Arc::new(InMemoryStore::new());
    let state = Flaky::new();
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        LaneConfig::new(1, 1),
        signer(),
        Arc::clone(&state) as Arc<dyn AppenderStore>,
    );
    Rig {
        backend,
        store,
        state,
        appender,
    }
}

impl Rig {
    async fn drain(&self) -> Result<usize, DocAuditError> {
        let feed: &dyn OutboxFeed = &self.backend;
        Ok(self.appender.drain(feed, 1000).await?.appended)
    }

    async fn seqs(&self) -> Vec<u64> {
        self.store
            .range(&lane_site(SITE, 0), 1, 10_000)
            .await
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect()
    }
}

#[tokio::test]
async fn forty_events_are_chained_once_each_in_order_in_groups() {
    let r = rig(40).await;
    assert_eq!(r.drain().await.unwrap(), 40);
    assert_eq!(r.seqs().await, (1..=40).collect::<Vec<_>>());
    // A second drain finds nothing new and appends nothing.
    assert_eq!(r.drain().await.unwrap(), 0);
    // Every document has exactly one entry.
    for i in 1..=40 {
        let entries = r.state.document_entries(DOCTYPE, &id_of(i)).await.unwrap();
        assert_eq!(entries.len(), 1);
    }
}

#[tokio::test]
async fn a_failed_index_write_of_a_group_is_recovered_without_chaining_twice() {
    for (op, nth) in [
        ("record_many", 1),
        ("record_many", 2),
        ("appended_many", 2),
        ("set_bookmark", 1),
    ] {
        let r = rig(40).await;
        r.state.fail_nth(op, nth);
        r.drain().await.expect_err(op);
        // The restart: a new appender over the same stores finds the unindexed entries in the
        // window and adopts them.
        let again = LaneAppender::new(
            Arc::clone(&r.store),
            SITE,
            0,
            LaneConfig::new(1, 1),
            signer(),
            Arc::clone(&r.state) as Arc<dyn AppenderStore>,
        );
        let feed: &dyn OutboxFeed = &r.backend;
        again.drain(feed, 1000).await.unwrap();
        assert_eq!(r.seqs().await, (1..=40).collect::<Vec<_>>(), "{op} {nth}");
        for i in 1..=40 {
            let entries = r.state.document_entries(DOCTYPE, &id_of(i)).await.unwrap();
            assert_eq!(entries.len(), 1, "{op} {nth}: document {i}");
        }
    }
}
