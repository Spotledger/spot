//! Tests of the verification: a clean journal, each way a journal can be damaged or mixed up, the
//! ownership checks, and the incremental and budgeted runs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod damage_tests;
mod ownership_tests;
mod pinned_tests;
mod run_tests;

use std::future::Future;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use ed25519_dalek::SigningKey;
use spot_events_contract::envelope::Position;
use spot_journal_manifest::{Manifest, SegmentSummary, SignedManifest, VerifyingKeys, sign};
use spot_journal_segment::{SealedSegment, SegmentConfig, SegmentWriter};
use spot_journal_store::{
    JournalStore, Layout, MemoryStore, ObjectEntry, ObjectMeta, StoreFuture, StoredObject, TenantId,
};

use crate::{Expected, FindingKind, VerifyReport};

pub(crate) const TENANT: &str = "acme";
pub(crate) const SOURCE: &str = "site-1";
pub(crate) const KEY_ID: &str = "journal-key-1";

/// Drives a store future without an async runtime.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

pub(crate) fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// The tenant's public keys: seed 1 for every generation.
pub(crate) fn keys() -> VerifyingKeys {
    let mut set = VerifyingKeys::new();
    set.add_key(KEY_ID, signing_key(1).verifying_key(), 1, u64::MAX)
        .unwrap();
    set
}

pub(crate) fn expected(generation: Option<u64>) -> Expected {
    Expected {
        tenant: TenantId::new(TENANT).unwrap(),
        source: SOURCE.to_owned(),
        generation,
    }
}

/// `count` segments of `per` entries each for `source`, chained from the source's genesis,
/// starting at position `first_seq` of `generation`. `salt` changes every entry's content.
pub(crate) fn seal(
    source: &str,
    generation: u64,
    first_seq: u64,
    count: u64,
    per: u64,
    salt: &str,
) -> Vec<SealedSegment> {
    let mut sealed: Vec<SealedSegment> = Vec::new();
    let mut next = first_seq;
    for _ in 0..count {
        let config = match sealed.last() {
            Some(previous) => SegmentConfig::after(source, generation, previous.hash),
            None => SegmentConfig::first(source, generation),
        };
        let mut writer = SegmentWriter::new(config).unwrap();
        for seq in next..next + per {
            let line = format!(
                "{{\"entry\":{seq},\"salt\":\"{salt}\",\"pad\":\"{}\"}}",
                "x".repeat(300)
            );
            writer
                .push(Position { generation, seq }, line.as_bytes())
                .unwrap();
        }
        next += per;
        sealed.push(writer.seal(["v1"]).unwrap());
    }
    sealed
}

pub(crate) fn summary(segment: &SealedSegment) -> SegmentSummary {
    let header = &segment.header;
    SegmentSummary::new(
        header.generation,
        header.first_seq,
        header.last_seq,
        header.entry_count,
        segment.hash,
        header.prev_hash,
    )
}

pub(crate) fn segment_key(segment: &SealedSegment) -> String {
    let header = &segment.header;
    Layout::segment(TENANT, header.generation, header.first_seq, header.last_seq)
        .unwrap()
        .to_key()
}

pub(crate) fn manifest_key(generation: u64) -> String {
    Layout::manifest(TENANT, generation).unwrap().to_key()
}

/// A generation-1 journal of sealed segments and the manifest that lists them.
pub(crate) struct Journal {
    pub(crate) segments: Vec<SealedSegment>,
    pub(crate) manifest: Manifest,
}

impl Journal {
    pub(crate) fn new(count: u64) -> Self {
        Self::salted(count, "a")
    }

    pub(crate) fn salted(count: u64, salt: &str) -> Self {
        let segments = seal(SOURCE, 1, 1, count, 3, salt);
        let summaries: Vec<SegmentSummary> = segments.iter().map(summary).collect();
        let manifest = spot_journal_manifest::append_segments(
            &Manifest::first(SOURCE, 1_000).unwrap(),
            &summaries,
        )
        .unwrap();
        Self { segments, manifest }
    }

    pub(crate) fn signed(&self) -> SignedManifest {
        sign(&self.manifest, &signing_key(1), KEY_ID)
    }

    /// A store holding the manifest and every segment, each segment passed through `edit`
    /// (`None` leaves it out).
    pub(crate) fn store_with(
        &self,
        edit: impl Fn(usize, Vec<u8>) -> Option<Vec<u8>>,
    ) -> MemoryStore {
        let store = MemoryStore::new();
        block_on(store.put_new(&manifest_key(1), self.signed().to_bytes())).unwrap();
        for (index, segment) in self.segments.iter().enumerate() {
            if let Some(bytes) = edit(index, segment.bytes.clone()) {
                block_on(store.put_new(&segment_key(segment), bytes)).unwrap();
            }
        }
        store
    }

    pub(crate) fn store(&self) -> MemoryStore {
        self.store_with(|_, bytes| Some(bytes))
    }

    pub(crate) fn range(&self, index: usize) -> (u64, u64) {
        let header = &self.segments[index].header;
        (header.first_seq, header.last_seq)
    }
}

/// The kinds of a report's findings, in report order.
pub(crate) fn kinds(report: &VerifyReport) -> Vec<FindingKind> {
    report.findings.iter().map(|f| f.kind).collect()
}

/// A store that remembers which objects were read.
pub(crate) struct CountingStore {
    pub(crate) inner: MemoryStore,
    pub(crate) reads: Mutex<Vec<String>>,
}

impl CountingStore {
    pub(crate) fn new(inner: MemoryStore) -> Self {
        Self {
            inner,
            reads: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn reads_of(&self, key: &str) -> usize {
        self.reads
            .lock()
            .unwrap()
            .iter()
            .filter(|k| *k == key)
            .count()
    }

    pub(crate) fn clear(&self) {
        self.reads.lock().unwrap().clear();
    }

    fn note(&self, key: &str) {
        self.reads.lock().unwrap().push(key.to_owned());
    }
}

impl JournalStore for CountingStore {
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
        self.note(key);
        self.inner.get(key)
    }

    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        self.note(key);
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
