//! The service ports of the jobs module: job submit, outbox lease and system reader.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use spot_app_api::context::Invocation;
use spot_app_api::document::Document;
use spot_app_api::prelude::{Name, RecordId};
use spot_cli::services::SiteCtx;
use spot_cli::services::ports::{
    DocumentLoader, JobSpec, JobSubmit, OutboxLease, PortError, PortFuture, PortParts,
    SystemReader, provide,
};
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::{ManualClock, SiteId};
use spot_jobs::{InMemoryJobStore, JobStore};
use spot_storage_contract::{Change, ChangeFeed, FeedPosition, OutboxRecord, StorageResult};
use time::macros::datetime;

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-06 10:00:00 UTC)))
}

/// A feed with nothing in it that counts the reads it served.
#[derive(Clone, Default)]
struct EmptyFeed(Arc<Mutex<u32>>);

impl ChangeFeed for EmptyFeed {
    async fn read_outbox(
        &self,
        _after: FeedPosition,
        _limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        *self.0.lock().unwrap() += 1;
        Ok(Vec::new())
    }

    async fn read_changes(&self, _after: FeedPosition, _limit: u32) -> StorageResult<Vec<Change>> {
        Ok(Vec::new())
    }
}

/// Documents of every site in one place, found only by the invocation's site.
#[derive(Default)]
struct Docs(Mutex<HashMap<(String, String, RecordId), Document>>);

impl Docs {
    fn put(&self, site: &str, doc: Document) {
        self.0.lock().unwrap().insert(
            (site.to_owned(), doc.doctype().as_str().to_owned(), doc.id()),
            doc,
        );
    }
}

impl DocumentLoader for Docs {
    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> PortFuture<'a, Option<Document>> {
        Box::pin(async move {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&(inv.site.to_string(), doctype.to_owned(), id))
                .cloned())
        })
    }
}

struct Site {
    ctx: SiteCtx<'static>,
    store: Arc<InMemoryJobStore>,
    feed: EmptyFeed,
}

async fn site(name: &str, docs: Arc<Docs>, node: &str) -> Site {
    let clock = clock();
    let mut ctx = SiteCtx::new(
        name,
        Duration::from_secs(1),
        clock.clone(),
        BTreeMap::new(),
        true,
    );
    let store = Arc::new(InMemoryJobStore::new());
    let coordinator = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new(node).unwrap(),
        clock.clone(),
    )
    .await
    .unwrap();
    let feed = EmptyFeed::default();
    ctx.as_module("jobs", 600);
    provide(
        &mut ctx,
        PortParts {
            site: SiteId::new(name).unwrap(),
            store: store.clone() as Arc<dyn JobStore>,
            queues: vec!["default".to_owned()],
            clock,
            coordinator,
            feed: feed.clone(),
            node: node.to_owned(),
            ttl: Duration::from_secs(30),
            loader: docs,
        },
    )
    .unwrap();
    Site { ctx, store, feed }
}

fn spec(key: &str) -> JobSpec {
    JobSpec {
        name: "platform:deliver_webhook".to_owned(),
        queue: "default".to_owned(),
        args: serde_json::json!({"n": 1}),
        idempotency_key: key.to_owned(),
        not_before: None,
    }
}

#[tokio::test]
async fn a_later_module_takes_each_port_and_an_earlier_one_gets_the_missing_port_error() {
    let mut s = site("acme", Arc::new(Docs::default()), "n1").await;
    s.ctx.as_module("webhooks", 700);
    assert!(s.ctx.require_port::<dyn JobSubmit>().is_ok());
    assert!(s.ctx.require_port::<dyn OutboxLease>().is_ok());
    assert!(s.ctx.require_port::<dyn SystemReader>().is_ok());

    let mut early = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        true,
    );
    early.as_module("early", 500);
    let err = early.require_port::<dyn JobSubmit>().err().unwrap();
    assert!(err.0.contains("module `early` requires the port"), "{err}");
}

#[tokio::test]
async fn the_same_idempotency_key_creates_one_job() {
    let s = site("acme", Arc::new(Docs::default()), "n1").await;
    let submit = s.ctx.require_port::<dyn JobSubmit>().unwrap();
    assert!(submit.submit(spec("k1")).await.unwrap());
    assert!(!submit.submit(spec("k1")).await.unwrap());
    assert!(submit.submit(spec("k2")).await.unwrap());
    let listed = s
        .store
        .list(spot_jobs::store::JobFilter::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
async fn submit_refuses_a_bad_request_without_echoing_it() {
    let s = site("acme", Arc::new(Docs::default()), "n1").await;
    let submit = s.ctx.require_port::<dyn JobSubmit>().unwrap();
    let mut other_queue = spec("k");
    other_queue.queue = "elsewhere".to_owned();
    assert!(matches!(
        submit.submit(other_queue).await,
        Err(PortError::Invalid(_))
    ));
    assert!(matches!(
        submit.submit(spec("")).await,
        Err(PortError::Invalid(_))
    ));
    let mut name = spec("secret-key-123");
    name.name = "no spaces allowed".to_owned();
    let err = submit.submit(name).await.unwrap_err();
    assert!(!err.to_string().contains("secret-key-123"));
}

#[tokio::test]
async fn the_outbox_lease_is_exclusive_and_reads_from_a_position() {
    let s = site("acme", Arc::new(Docs::default()), "n1").await;
    let outbox = s.ctx.require_port::<dyn OutboxLease>().unwrap();
    let lease = outbox.acquire("webhooks").await.unwrap();
    assert_eq!(lease.consumer(), "webhooks");
    assert_eq!(
        outbox.acquire("webhooks").await.unwrap_err(),
        PortError::LeaseUnavailable
    );
    assert!(outbox.still_held(&lease).await.unwrap());
    let lease = outbox.renew(&lease).await.unwrap();
    assert!(
        outbox
            .read(FeedPosition::START, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(*s.feed.0.lock().unwrap(), 1);
    outbox.release(&lease).await.unwrap();
    assert!(outbox.acquire("webhooks").await.is_ok());
    assert!(matches!(
        outbox.acquire("Bad Name").await,
        Err(PortError::Invalid(_))
    ));
}

#[tokio::test]
async fn a_lease_of_another_site_is_not_accepted() {
    let a = site("acme", Arc::new(Docs::default()), "n1").await;
    let b = site("globex", Arc::new(Docs::default()), "n1").await;
    let lease = a
        .ctx
        .require_port::<dyn OutboxLease>()
        .unwrap()
        .acquire("webhooks")
        .await
        .unwrap();
    let outbox_b = b.ctx.require_port::<dyn OutboxLease>().unwrap();
    assert!(matches!(
        outbox_b.renew(&lease).await,
        Err(PortError::Invalid(_))
    ));
}

#[tokio::test]
async fn two_sites_system_readers_are_independent() {
    let docs = Arc::new(Docs::default());
    let id = RecordId::new_v7();
    let mut doc = Document::new(Name::new("note").unwrap(), id);
    doc.set(Name::new("title").unwrap(), "hello".to_owned());
    docs.put("acme", doc);
    let a = site("acme", docs.clone(), "n1").await;
    let b = site("globex", docs, "n2").await;
    let name = id.to_string();
    let read_a = a.ctx.require_port::<dyn SystemReader>().unwrap();
    let read_b = b.ctx.require_port::<dyn SystemReader>().unwrap();
    assert!(read_a.read("note", &name).await.unwrap().is_some());
    assert!(read_b.read("note", &name).await.unwrap().is_none());
    assert!(matches!(
        read_a.read("note", "not-an-id").await,
        Err(PortError::Invalid(_))
    ));
}

#[tokio::test]
async fn ports_answer_stopped_after_the_stop_signal() {
    let s = site("acme", Arc::new(Docs::default()), "n1").await;
    let submit = s.ctx.require_port::<dyn JobSubmit>().unwrap();
    let outbox = s.ctx.require_port::<dyn OutboxLease>().unwrap();
    let reader = s.ctx.require_port::<dyn SystemReader>().unwrap();
    let held = outbox.acquire("webhooks").await.unwrap();
    let mut assembly = s.ctx.finish(Router::new()).assembly.unwrap();
    assembly.start().await.unwrap();
    assert!(submit.submit(spec("before")).await.unwrap());
    assembly.shutdown().await;
    let stopped = tokio::time::timeout(Duration::from_secs(5), async {
        (
            submit.submit(spec("after")).await.unwrap_err(),
            outbox.read(FeedPosition::START, 1).await.unwrap_err(),
            outbox.acquire("other").await.unwrap_err(),
            reader
                .read("note", &RecordId::new_v7().to_string())
                .await
                .unwrap_err(),
        )
    })
    .await
    .expect("a stopped port answers, it does not hang");
    assert_eq!(
        stopped,
        (
            PortError::Stopped,
            PortError::Stopped,
            PortError::Stopped,
            PortError::Stopped
        )
    );
    // Giving a lease up stays possible after the stop signal.
    assert!(outbox.release(&held).await.is_ok());
}
