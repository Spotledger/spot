//! `ReaderSource` over a real snapshot written by `SnapshotWriter` into a `MemoryStore`, driven by
//! the real `Bootstrap` (WP-SYN-07.T7).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use serde_json::{Value, json};
use spot_journal_snapshot::{
    ImageRecord, SnapshotInfo, SnapshotKeys, SnapshotParams, SnapshotReader, SnapshotSigner,
    SnapshotWriter,
};
use spot_journal_store::{
    JournalStore, MemoryStore, ObjectEntry, ObjectMeta, StoreFuture, StoredObject,
};
use spot_sync_bootstrap::{
    Bootstrap, BootstrapError, Budget, HubHead, NodeState, ScopeView, Seams, SnapshotSource,
    StartOptions,
};
use spot_sync_bootstrap_reader::{Blocker, OpenError, ReaderSource};
use spot_sync_closure::{ClosureError, Limits, NoRedaction, RecordKey, References};

const TENANT: &str = "acme";
const GENERATION: u64 = 2;
const POSITION: u64 = 500;
const PER_ENTITY: u64 = 1_000;

// ----------------------------------------------------------------- doubles

/// Signs by reversing the message: proves the reader checks the signature at all.
struct Toy;

impl SnapshotSigner for Toy {
    fn key_id(&self) -> &str {
        "toy"
    }
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        message.iter().rev().copied().collect()
    }
}

impl SnapshotKeys for Toy {
    fn verify(&self, key_id: &str, _generation: u64, message: &[u8], signature: &[u8]) -> bool {
        key_id == "toy" && Toy.sign(message) == signature
    }
}

/// Runs a future to completion on the calling thread; the in-memory store never waits.
struct PollBlocker;

impl Blocker for PollBlocker {
    fn block_on<T>(&self, future: Pin<Box<dyn Future<Output = T> + '_>>) -> T {
        let mut future = future;
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
            std::thread::yield_now();
        }
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    PollBlocker.block_on(Box::pin(future))
}

/// A store that logs every `get`, and lets a test add its own events to the same log.
#[derive(Clone, Default)]
struct LoggedStore {
    inner: Arc<MemoryStore>,
    log: Arc<Mutex<Vec<String>>>,
}

impl LoggedStore {
    fn events(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn note(&self, event: &str) {
        self.log.lock().unwrap().push(event.to_owned());
    }
    fn clear(&self) {
        self.log.lock().unwrap().clear();
    }
}

impl JournalStore for LoggedStore {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        self.inner.put_new(key, bytes)
    }
    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        self.inner.put_replace(key, bytes, expected_etag)
    }
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        self.log.lock().unwrap().push(format!("get:{key}"));
        self.inner.get(key)
    }
    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        self.inner.head(key)
    }
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        self.inner.list(prefix, after, limit)
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.inner.delete(key)
    }
}

struct NoLinks;

impl References for NoLinks {
    fn references(&self, _entity: &str, _image: &Value) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(Vec::new())
    }
}

/// Covers a fixed list of entities.
struct Entities(&'static [&'static str]);

impl ScopeView for Entities {
    fn covers(&self, key: &RecordKey, _image: &Value) -> bool {
        self.0.contains(&key.entity.as_str())
    }
    fn fingerprint(&self) -> String {
        self.0.join(",")
    }
}

// ----------------------------------------------------------------- the snapshot

fn image(entity: &str, n: u64) -> ImageRecord {
    ImageRecord {
        entity: entity.to_owned(),
        id: format!("{n:06}"),
        version: n + 1,
        entity_schema: 1,
        image: json!({ "n": n, "note": format!("record {entity} {n}"), "pad": "x".repeat(24) }),
    }
}

fn all_images() -> Vec<ImageRecord> {
    ["a", "b"]
        .iter()
        .flat_map(|entity| (0..PER_ENTITY).map(move |n| image(entity, n)))
        .collect()
}

fn info() -> SnapshotInfo {
    SnapshotInfo {
        position: POSITION,
        generation: GENERATION,
        verified: true,
        taken_at_millis: 0,
    }
}

/// 2,000 images in key order across several parts.
fn snapshot() -> LoggedStore {
    let store = LoggedStore::default();
    let params = SnapshotParams {
        tenant: TENANT.to_owned(),
        generation: GENERATION,
        position: POSITION,
        manifest_hash: "ab".repeat(32),
        taken_at_millis: 0,
    };
    block_on(async {
        let mut writer = SnapshotWriter::new(&store, params, &Toy)
            .and_then(|w| w.with_part_bytes(30_000))
            .unwrap();
        writer.write_part(all_images()).await.unwrap();
        writer.finish().await.unwrap();
    });
    store
}

fn reader(store: &LoggedStore) -> SnapshotReader<'_> {
    block_on(SnapshotReader::open(
        store, TENANT, GENERATION, POSITION, &Toy,
    ))
    .unwrap()
}

fn source(store: &LoggedStore) -> ReaderSource<'_, PollBlocker> {
    ReaderSource::open(reader(store), info(), TENANT, PollBlocker).expect("the snapshot opens")
}

fn part_name(ordinal: usize) -> String {
    format!("part-{:08}.jsonl.zst", ordinal + 1)
}

fn node() -> NodeState {
    NodeState {
        node_id: "n1".to_owned(),
        tenant: TENANT.to_owned(),
        generation: 0,
        applied_position: None,
    }
}

fn hub() -> HubHead {
    HubHead {
        generation: GENERATION,
        head_position: POSITION + 50,
    }
}

fn served(source: &dyn SnapshotSource, scope: &dyn ScopeView) -> BTreeSet<(String, String)> {
    let limits = Limits::default();
    let seams = Seams {
        scope,
        references: &NoLinks,
        redactor: &NoRedaction,
    };
    let mut session = Bootstrap::start(
        source,
        seams,
        limits,
        &node(),
        hub(),
        StartOptions::default(),
    )
    .unwrap_or_else(|e| panic!("bootstrap start: {e}"));
    let mut out = BTreeSet::new();
    while let Some(chunk) = session.next_chunk(Budget::of(&limits)).unwrap() {
        for bundle in &chunk.bundles {
            for record in &bundle.records {
                out.insert((record.key.entity.clone(), record.key.id.clone()));
            }
        }
    }
    out
}

// ----------------------------------------------------------------- tests

#[test]
fn a_real_bootstrap_serves_every_image_that_was_written() {
    let store = snapshot();
    let src = source(&store);
    assert!(src.part_count() >= 5, "parts: {}", src.part_count());
    let everything = Entities(&["a", "b"]);
    let got = served(&src, &everything);
    let want: BTreeSet<(String, String)> =
        all_images().into_iter().map(|i| (i.entity, i.id)).collect();
    assert_eq!(got.len(), 2_000);
    assert_eq!(got, want);
}

#[test]
fn a_scope_that_covers_half_the_records_serves_exactly_that_half() {
    let store = snapshot();
    let src = source(&store);
    let got = served(&src, &Entities(&["a"]));
    assert_eq!(got.len(), 1_000);
    assert!(got.iter().all(|(entity, _)| entity == "a"));
}

#[test]
fn a_fetch_loads_only_the_parts_that_hold_its_keys() {
    let store = snapshot();
    let probe = reader(&store);
    let in_part_1 = block_on(probe.part(1)).unwrap().remove(3);
    let in_part_3 = block_on(probe.part(3)).unwrap().pop().unwrap();
    let src = source(&store);
    src.scan(&mut |_| Ok(())).unwrap();
    store.clear();

    let wanted = [
        RecordKey::new(&in_part_1.entity, &in_part_1.id),
        RecordKey::new(&in_part_3.entity, &in_part_3.id),
        // Held by no part: between two real ids of part 1, so no other part is read for it.
        RecordKey::new(&in_part_1.entity, format!("{}x", in_part_1.id)),
        // Below the first key of the first part: no part is read for it.
        RecordKey::new("0", "000000"),
    ];
    let mut found = src.fetch(&wanted).unwrap();
    found.sort_by(|x, y| (&x.entity, &x.id).cmp(&(&y.entity, &y.id)));
    let mut expect = vec![in_part_1.clone(), in_part_3.clone()];
    expect.sort_by(|x, y| (&x.entity, &x.id).cmp(&(&y.entity, &y.id)));
    assert_eq!(found, expect);

    let gets: Vec<String> = store.events();
    assert_eq!(gets.len(), 2, "{gets:?}");
    for (event, ordinal) in gets.iter().zip([1_usize, 3]) {
        assert!(event.ends_with(&part_name(ordinal)), "{event}");
    }
}

#[test]
fn a_fetch_before_any_scan_builds_the_first_key_index_and_answers() {
    let store = snapshot();
    let probe = reader(&store);
    let last = block_on(probe.part(probe.part_count() - 1))
        .unwrap()
        .pop()
        .unwrap();
    let src = source(&store);
    let found = src
        .fetch(&[RecordKey::new(&last.entity, &last.id)])
        .unwrap();
    assert_eq!(found, vec![last]);
    // An empty request reads nothing.
    store.clear();
    assert!(src.fetch(&[]).unwrap().is_empty());
    assert!(store.events().is_empty());
}

#[test]
fn a_part_that_was_altered_stops_scan_and_fetch_and_none_of_its_images_is_visited() {
    let good = snapshot();
    let bad = LoggedStore::default();
    let target = part_name(2);
    for entry in block_on(good.list("", None, 10_000)).unwrap() {
        let mut bytes = block_on(good.get(&entry.key)).unwrap().unwrap().bytes;
        if entry.key.ends_with(&target) {
            bytes[3] ^= 0x01;
        }
        block_on(bad.put_new(&entry.key, bytes)).unwrap();
    }
    let probe = reader(&good);
    let before: u64 = (0..2).map(|i| probe.index().body.parts[i].images).sum();
    let in_bad_part = block_on(probe.part(2)).unwrap().remove(0);

    let src = source(&bad);
    let mut visited = 0_u64;
    let error = src
        .scan(&mut |_| {
            visited += 1;
            Ok(())
        })
        .unwrap_err();
    match &error {
        BootstrapError::SourceFailed { reason } => {
            assert!(reason.contains("journal_snapshot.part_invalid"), "{reason}");
            assert!(reason.contains(&target), "{reason}");
            assert!(!reason.contains("record "), "{reason}");
        }
        other => panic!("expected SourceFailed, got {other:?}"),
    }
    assert_eq!(
        visited, before,
        "only the parts before the bad one were visited"
    );

    let fetched = src.fetch(&[RecordKey::new(&in_bad_part.entity, &in_bad_part.id)]);
    assert!(matches!(fetched, Err(BootstrapError::SourceFailed { .. })));
}

#[test]
fn scan_holds_one_part_at_a_time() {
    let store = snapshot();
    let counts: Vec<u64> = reader(&store)
        .index()
        .body
        .parts
        .iter()
        .map(|p| p.images)
        .collect();
    let src = source(&store);
    store.clear();
    src.scan(&mut |_| {
        store.note("visit");
        Ok(())
    })
    .unwrap();
    let events = store.events();
    // The k-th part is read only after every image of the parts before it was visited.
    let (mut visits, mut part) = (0_u64, 0_usize);
    for event in &events {
        if event == "visit" {
            visits += 1;
        } else {
            let before: u64 = counts.iter().take(part).sum();
            assert_eq!(visits, before, "part {part} was read early: {event}");
            part += 1;
        }
    }
    assert_eq!(part, counts.len());
    assert_eq!(visits, 2_000);
}

#[test]
fn an_info_that_differs_from_the_index_or_another_tenant_is_refused_at_open() {
    let store = snapshot();
    let mut moved = info();
    moved.position += 1;
    let error = ReaderSource::open(reader(&store), moved, TENANT, PollBlocker)
        .err()
        .unwrap();
    assert_eq!(error.code(), "sync_bootstrap_reader.snapshot_mismatch");
    assert!(matches!(error, OpenError::SnapshotMismatch { .. }));

    let mut other_generation = info();
    other_generation.generation += 1;
    let error = ReaderSource::open(reader(&store), other_generation, TENANT, PollBlocker)
        .err()
        .unwrap();
    assert_eq!(error.code(), "sync_bootstrap_reader.snapshot_mismatch");

    let error = ReaderSource::open(reader(&store), info(), "other-tenant", PollBlocker)
        .err()
        .unwrap();
    assert_eq!(error, OpenError::TenantMismatch);
    // Nothing of any part was read by the refused opens.
    assert!(store.events().iter().all(|e| !e.contains("part-")));
}
