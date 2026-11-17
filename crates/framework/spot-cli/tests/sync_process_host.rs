//! Process distribution hosted (WP-SYN-12.T2): the hub publishes, the edge installs beside, holds
//! what its engine cannot run, retires only unpinned versions, refuses what a node pushes, and
//! answers only after a version is durable. Over an in-memory durable store and a fake hub stream
//! and pin source, with no database; the sync wire between a hub and an edge is
//! `harness/rust/l3-journal-sync/tests/process_distribution.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use spot_cli::services::sync_process::{
    BoxFut, DistributionEvent, EventSink, HubStream, Persist, PersistOp, PinSource, ProcessSync,
    ProcessSyncParts,
};
use spot_core::ManualClock;
use spot_sync_process::{Capability, CompatClass, ProcessRecord, Published, Received, record_name};
use time::macros::datetime;

#[derive(Default)]
struct Durable {
    records: Mutex<BTreeMap<(String, u64), ProcessRecord>>,
    fail: AtomicBool,
}

impl Persist for Durable {
    fn load<'a>(&'a self) -> BoxFut<'a, Result<Vec<ProcessRecord>, String>> {
        Box::pin(async move { Ok(self.records.lock().unwrap().values().cloned().collect()) })
    }
    fn apply<'a>(&'a self, ops: &'a [PersistOp]) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err("the table cannot be written".to_owned());
            }
            let mut records = self.records.lock().unwrap();
            for op in ops {
                match op {
                    PersistOp::Put(r) => {
                        records.insert((r.process.clone(), r.version), r.clone());
                    }
                    PersistOp::Remove(p, v) => {
                        records.remove(&(p.clone(), *v));
                    }
                }
            }
            Ok(())
        })
    }
    fn check<'a>(&'a self) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move { Ok(()) })
    }
}

#[derive(Default)]
struct Stream(Mutex<Vec<(String, Vec<String>)>>);
impl HubStream for Stream {
    fn emit(&self, record: &ProcessRecord, scope_keys: &[String]) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .push((record.name(), scope_keys.to_vec()));
        Ok(())
    }
}

#[derive(Default)]
struct Pins(Mutex<BTreeMap<(String, u64), u64>>);
impl PinSource for Pins {
    fn pins(&self, process: &str, version: u64) -> u64 {
        self.0
            .lock()
            .unwrap()
            .get(&(process.to_owned(), version))
            .copied()
            .unwrap_or(0)
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<DistributionEvent>>);
impl EventSink for Events {
    fn record(&self, event: &DistributionEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

struct Node {
    sync: ProcessSync,
    durable: Arc<Durable>,
    stream: Arc<Stream>,
    pins: Arc<Pins>,
    events: Arc<Events>,
}

async fn node(engine_version: u64, durable: Arc<Durable>) -> Node {
    let stream = Arc::new(Stream::default());
    let pins = Arc::new(Pins::default());
    let events = Arc::new(Events::default());
    let sync = ProcessSync::start(ProcessSyncParts {
        node: "n1".to_owned(),
        tenant: "acme".to_owned(),
        capability: Capability {
            engine_version,
            accepts_breaking: false,
        },
        persist: durable.clone(),
        pins: Some(pins.clone()),
        hub: Some(stream.clone()),
        conditions: None,
        events: Some(events.clone()),
        clock: Arc::new(ManualClock::new(datetime!(2026-11-13 08:00:00 UTC))),
    })
    .await
    .unwrap();
    Node {
        sync,
        durable,
        stream,
        pins,
        events,
    }
}

fn record(version: u64, min_engine: u64, class: CompatClass) -> ProcessRecord {
    ProcessRecord::new(
        "acme",
        "order_to_cash",
        version,
        min_engine,
        class,
        &format!("{{\"process\":\"order_to_cash\",\"note\":\"definition of version {version}\"}}"),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn publish_reaches_the_stream_with_its_scope_keys_and_a_republish_is_a_noop() {
    let hub = node(1, Arc::default()).await;
    let v1 = record(1, 1, CompatClass::Compatible);
    let keys = vec!["branch:east".to_owned()];
    assert_eq!(hub.sync.publish(&v1, &keys).unwrap(), Published::Emitted);
    assert_eq!(hub.sync.publish(&v1, &keys).unwrap(), Published::NoOp);
    let emitted = hub.stream.0.lock().unwrap().clone();
    assert_eq!(
        emitted,
        [("process:order_to_cash@1".to_owned(), keys)],
        "scope by subscription keys"
    );
    // A changed definition under the same version is refused, not emitted.
    let mut changed = v1.clone();
    changed.payload = "{\"process\":\"order_to_cash\",\"note\":\"another definition\"}".to_owned();
    changed.definition_hash = spot_sync_process::hash_definition(&changed.payload);
    assert!(hub.sync.publish(&changed, &[]).is_err());
    assert_eq!(hub.stream.0.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_installs_beside_and_a_new_case_starts_on_the_newest_runnable_version() {
    let edge = node(2, Arc::default()).await;
    assert_eq!(
        edge.sync
            .receive(&record(1, 1, CompatClass::Compatible))
            .await
            .unwrap(),
        Received::Installed
    );
    assert_eq!(edge.sync.start_version("order_to_cash"), Some(1));
    assert_eq!(
        edge.sync
            .receive(&record(2, 1, CompatClass::Compatible))
            .await
            .unwrap(),
        Received::Installed
    );
    // v1 is still installed beside v2, so a Case pinned to it keeps running.
    assert_eq!(edge.durable.records.lock().unwrap().len(), 2);
    assert_eq!(edge.sync.start_version("order_to_cash"), Some(2));
    // The same record again is a no-op.
    assert_eq!(
        edge.sync
            .receive(&record(2, 1, CompatClass::Compatible))
            .await
            .unwrap(),
        Received::NoOp
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_one_release_behind_holds_the_new_version_and_an_upgrade_releases_it() {
    let edge = node(2, Arc::default()).await;
    edge.sync
        .receive(&record(2, 1, CompatClass::Compatible))
        .await
        .unwrap();
    assert_eq!(
        edge.sync
            .receive(&record(3, 5, CompatClass::Compatible))
            .await
            .unwrap(),
        Received::Held
    );
    assert_eq!(
        edge.sync.held(),
        ["order_to_cash@3"],
        "visible in the sync health"
    );
    assert_eq!(
        edge.sync.start_version("order_to_cash"),
        Some(2),
        "new Cases keep starting on v2"
    );
    assert_eq!(
        edge.durable.records.lock().unwrap().len(),
        1,
        "a held record is not installed"
    );
    assert_eq!(edge.sync.on_upgrade(5).await.unwrap(), 1);
    assert!(edge.sync.held().is_empty());
    assert_eq!(edge.sync.start_version("order_to_cash"), Some(3));
    let actions: Vec<&str> = edge
        .events
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.action)
        .collect();
    assert_eq!(actions, ["installed", "held", "released"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retired_version_goes_only_when_no_case_is_pinned_to_it() {
    let edge = node(2, Arc::default()).await;
    edge.sync
        .receive(&record(1, 1, CompatClass::Compatible))
        .await
        .unwrap();
    edge.sync
        .receive(&record(2, 1, CompatClass::Compatible))
        .await
        .unwrap();
    edge.pins
        .0
        .lock()
        .unwrap()
        .insert(("order_to_cash".to_owned(), 1), 3);
    // Three Cases run on v1: it is kept, whatever the retirement says.
    assert!(!edge.sync.retire("order_to_cash", 1).await.unwrap());
    assert_eq!(edge.durable.records.lock().unwrap().len(), 2);
    // The last Case ends: the sweep removes it, and v2 stays.
    edge.pins
        .0
        .lock()
        .unwrap()
        .insert(("order_to_cash".to_owned(), 1), 0);
    assert_eq!(edge.sync.sweep().await.unwrap(), 1);
    let left: Vec<u64> = edge
        .durable
        .records
        .lock()
        .unwrap()
        .keys()
        .map(|k| k.1)
        .collect();
    assert_eq!(left, [2]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_acknowledged_before_it_is_durable_and_a_restart_reloads_the_set() {
    let durable = Arc::new(Durable::default());
    durable.fail.store(true, Ordering::SeqCst);
    let edge = node(2, durable.clone()).await;
    let error = edge
        .sync
        .receive(&record(1, 1, CompatClass::Compatible))
        .await
        .unwrap_err();
    assert!(error.code.ends_with("store_failed"), "{}", error.code);
    // The store recovers; a retry installs and persists.
    durable.fail.store(false, Ordering::SeqCst);
    assert!(
        edge.sync
            .receive(&record(1, 1, CompatClass::Compatible))
            .await
            .is_ok()
    );
    // A restart over the same durable set still has the version.
    let restarted = node(2, durable).await;
    assert_eq!(restarted.sync.start_version("order_to_cash"), Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_and_tampered_records_are_refused_and_counted() {
    let edge = node(2, Arc::default()).await;
    let mut foreign = record(1, 1, CompatClass::Compatible);
    foreign.tenant = "beta".to_owned();
    assert!(edge.sync.receive(&foreign).await.is_err());
    let mut tampered = record(1, 1, CompatClass::Compatible);
    tampered.payload =
        "{\"process\":\"order_to_cash\",\"note\":\"not what was hashed\"}".to_owned();
    assert!(edge.sync.receive(&tampered).await.is_err());
    assert_eq!(
        edge.sync.quarantined(),
        1,
        "a hash mismatch is quarantined and counted"
    );
    assert!(
        edge.durable.records.lock().unwrap().is_empty(),
        "nothing was installed"
    );
    let refused = edge
        .events
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.action == "refused")
        .count();
    assert_eq!(refused, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_authored_process_record_is_refused_and_audited() {
    let hub = node(1, Arc::default()).await;
    let error = hub
        .sync
        .refuse_from_node(&record_name("order_to_cash", 9))
        .unwrap_err();
    assert!(error.code.ends_with("edge_authored"), "{}", error.code);
    assert!(
        hub.sync.refuse_from_node("party:123").is_ok(),
        "other names are not this module's"
    );
    let events = hub.events.0.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert_eq!(
        (
            events[0].action,
            events[0].code.map(|c| c.ends_with("edge_authored"))
        ),
        ("refused", Some(true))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_pin_source_nothing_is_ever_retired() {
    // A node whose BPM runtime does not provide pin counts treats every version as pinned.
    let durable = Arc::new(Durable::default());
    let sync = ProcessSync::start(ProcessSyncParts {
        node: "n1".to_owned(),
        tenant: "acme".to_owned(),
        capability: Capability {
            engine_version: 2,
            accepts_breaking: false,
        },
        persist: durable.clone(),
        pins: None,
        hub: None,
        conditions: None,
        events: None,
        clock: Arc::new(ManualClock::new(datetime!(2026-11-13 08:00:00 UTC))),
    })
    .await
    .unwrap();
    sync.receive(&record(1, 1, CompatClass::Compatible))
        .await
        .unwrap();
    assert_eq!(sync.sweep().await.unwrap(), 0);
    // The hub side is off without a stream.
    assert!(
        sync.publish(&record(2, 1, CompatClass::Compatible), &[])
            .is_err()
    );
}
