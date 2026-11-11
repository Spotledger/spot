//! Shared test doubles: a fixed-key signer, a store that records and fails on demand, helpers.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use spot_events_contract::envelope::Position;
use spot_journal_manifest::{Manifest, SignedManifest, Verified, VerifyingKeys, sign, verify};
use spot_journal_segment::{SealLimits, SegmentReader};
use spot_journal_store::{
    JournalStore, Layout, MemoryStore, ObjectEntry, ObjectMeta, StoreError, StoreFuture,
    StoredObject,
};

use crate::{Batch, Durability, ManifestSigner, Relay, RelayConfig, RelayError};

pub(crate) const TENANT: &str = "acme";
pub(crate) const SOURCE: &str = "acme-site";
pub(crate) const KEY_ID: &str = "journal-key-1";

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[7u8; 32])
}

/// A signer over a fixed Ed25519 key.
pub(crate) struct FixedSigner;

impl ManifestSigner for FixedSigner {
    fn sign(&self, manifest: &Manifest) -> SignedManifest {
        sign(manifest, &signing_key(), KEY_ID)
    }
    fn key_id(&self) -> &str {
        KEY_ID
    }
}

pub(crate) fn keys() -> VerifyingKeys {
    let mut keys = VerifyingKeys::new();
    keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
        .expect("key window");
    keys
}

pub(crate) fn line(generation: u64, seq: u64) -> Vec<u8> {
    format!("{{\"g\":{generation},\"seq\":{seq},\"data\":\"entry number {seq}\"}}").into_bytes()
}

pub(crate) fn pos(seq: u64) -> Position {
    Position { generation: 1, seq }
}

/// A batch of generation 1 holding `first..=last`.
pub(crate) fn batch(batch_id: u64, first: u64, last: u64) -> Batch {
    batch_in(1, batch_id, first, last)
}

pub(crate) fn batch_in(generation: u64, batch_id: u64, first: u64, last: u64) -> Batch {
    Batch {
        batch_id,
        entries: (first..=last)
            .map(|seq| (Position { generation, seq }, line(generation, seq)))
            .collect(),
    }
}

pub(crate) fn cfg(durability: Durability) -> RelayConfig {
    RelayConfig::new(TENANT, SOURCE, durability)
}

pub(crate) fn never_due(durability: Durability) -> RelayConfig {
    let mut config = cfg(durability);
    config.limits = SealLimits {
        max_bytes: u64::MAX,
        max_age_millis: u64::MAX,
    };
    config
}

pub(crate) async fn open(
    store: &FaultStore,
    config: RelayConfig,
) -> Result<Relay<FaultStore>, RelayError> {
    Relay::open(store.clone(), Box::new(FixedSigner), keys(), config, 1_000).await
}

/// A store that records calls, can fail after a number of writes and can lose a manifest race.
#[derive(Clone, Default)]
pub(crate) struct FaultStore {
    inner: Arc<MemoryStore>,
    log: Arc<Mutex<Vec<String>>>,
    writes: Arc<AtomicUsize>,
    /// Writes numbered from this one on fail (`usize::MAX` when none).
    fail_from: Arc<AtomicUsize>,
    /// The first failing write is applied, then reported as failed (an unknown outcome).
    apply_first_failure: Arc<AtomicBool>,
    /// Every `put_replace` answers `EtagMismatch` without writing.
    lose_replace: Arc<AtomicBool>,
}

impl FaultStore {
    pub(crate) fn new() -> Self {
        let store = Self::default();
        store.fail_from.store(usize::MAX, Ordering::SeqCst);
        store
    }

    pub(crate) fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    /// Writes from now on: the first `n` succeed, later ones fail.
    pub(crate) fn fail_after(&self, n: usize, apply_first_failure: bool) {
        self.fail_from.store(self.writes() + n, Ordering::SeqCst);
        self.apply_first_failure
            .store(apply_first_failure, Ordering::SeqCst);
    }

    pub(crate) fn heal(&self) {
        self.fail_from.store(usize::MAX, Ordering::SeqCst);
    }

    pub(crate) fn lose_replace(&self) {
        self.lose_replace.store(true, Ordering::SeqCst);
    }

    pub(crate) fn log(&self) -> Vec<String> {
        self.log.lock().expect("log").clone()
    }

    pub(crate) fn clear_log(&self) {
        self.log.lock().expect("log").clear();
    }

    fn note(&self, entry: String) {
        self.log.lock().expect("log").push(entry);
    }

    /// `Some(apply)` when this write must fail; `apply` says whether it is applied first.
    fn gate(&self, operation: &'static str, key: &str) -> Option<bool> {
        let index = self.writes.fetch_add(1, Ordering::SeqCst);
        let from = self.fail_from.load(Ordering::SeqCst);
        if index < from {
            return None;
        }
        Some(index == from && self.apply_first_failure.load(Ordering::SeqCst))
            .inspect(|_| self.note(format!("fail {operation} {key}")))
    }

    fn injected(operation: &'static str, key: &str) -> StoreError {
        StoreError::Io {
            operation,
            key: key.to_owned(),
            message: "injected failure".to_owned(),
        }
    }

    pub(crate) async fn keys_under(&self, prefix: &str) -> Vec<String> {
        self.inner
            .list(prefix, None, 10_000)
            .await
            .expect("list")
            .into_iter()
            .map(|entry| entry.key)
            .collect()
    }

    pub(crate) async fn read(&self, key: &str) -> Option<Vec<u8>> {
        self.inner.get(key).await.expect("get").map(|o| o.bytes)
    }

    pub(crate) async fn put(&self, key: &str, bytes: Vec<u8>) {
        self.inner.put_new(key, bytes).await.expect("seed put");
    }
}

impl JournalStore for FaultStore {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            match self.gate("write", key) {
                None => {
                    let meta = self.inner.put_new(key, bytes).await?;
                    self.note(format!("put_new {key}"));
                    Ok(meta)
                }
                Some(apply) => {
                    if apply {
                        let _ = self.inner.put_new(key, bytes).await;
                    }
                    Err(Self::injected("write", key))
                }
            }
        })
    }

    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            if self.lose_replace.load(Ordering::SeqCst) {
                self.note(format!("lost put_replace {key}"));
                return Err(StoreError::EtagMismatch {
                    key: key.to_owned(),
                    expected: expected_etag.to_owned(),
                });
            }
            match self.gate("replace", key) {
                None => {
                    let meta = self.inner.put_replace(key, bytes, expected_etag).await?;
                    self.note(format!("put_replace {key}"));
                    Ok(meta)
                }
                Some(apply) => {
                    if apply {
                        let _ = self.inner.put_replace(key, bytes, expected_etag).await;
                    }
                    Err(Self::injected("replace", key))
                }
            }
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
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
        Box::pin(async move {
            match self.gate("delete", key) {
                None => {
                    self.inner.delete(key).await?;
                    self.note(format!("delete {key}"));
                    Ok(())
                }
                Some(apply) => {
                    if apply {
                        let _ = self.inner.delete(key).await;
                    }
                    Err(Self::injected("delete", key))
                }
            }
        })
    }
}

pub(crate) fn manifest_key(generation: u64) -> String {
    Layout::manifest(TENANT, generation).expect("key").to_key()
}

pub(crate) fn tail_key(generation: u64, batch_id: u64) -> String {
    Layout::tail_chunk(TENANT, generation, batch_id)
        .expect("key")
        .to_key()
}

pub(crate) fn tail_prefix(generation: u64) -> String {
    format!("{TENANT}/tail/{}/", Layout::padded(generation))
}

pub(crate) fn segment_prefix(generation: u64) -> String {
    format!("{TENANT}/segments/{}/", Layout::padded(generation))
}

/// The manifest in the store, verified under the test keys.
pub(crate) async fn stored_manifest(store: &FaultStore, generation: u64) -> Verified {
    let bytes = store
        .read(&manifest_key(generation))
        .await
        .expect("manifest");
    let signed = SignedManifest::from_bytes(&bytes).expect("canonical");
    verify(&signed, &keys()).expect("manifest verifies")
}

/// Every segment of a generation, in order, as bytes.
pub(crate) async fn stored_segments(store: &FaultStore, generation: u64) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for key in store.keys_under(&segment_prefix(generation)).await {
        out.push(store.read(&key).await.expect("segment"));
    }
    out
}

/// The `(position, line)` pairs of a list of segments, concatenated.
pub(crate) fn entries_of(segments: &[Vec<u8>]) -> Vec<(Position, Vec<u8>)> {
    let mut out = Vec::new();
    for bytes in segments {
        let reader = SegmentReader::open(bytes.as_slice()).expect("open");
        for entry in reader.entries() {
            let entry = entry.expect("entry");
            out.push((entry.position, entry.line.into_bytes()));
        }
    }
    out
}

pub(crate) fn input(generation: u64, first: u64, last: u64) -> Vec<(Position, Vec<u8>)> {
    batch_in(generation, 0, first, last).entries
}
