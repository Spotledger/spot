#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tests: round trip, ordering, crash, corruption, tenancy, listing, verified flag, determinism.

use std::future::Future;

use sha2::{Digest, Sha256};
use spot_journal_store::{JournalStore, MemoryStore};

use crate::{ImageRecord, SnapshotKeys, SnapshotParams, SnapshotSigner};

mod cases;

pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

/// A keyed-hash signer double: SHA-256 over the secret and the message.
pub(crate) struct Keyed {
    pub id: &'static str,
    pub secret: &'static str,
}

fn mac(secret: &str, message: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    hasher.update(message);
    hasher.finalize().to_vec()
}

impl SnapshotSigner for Keyed {
    fn key_id(&self) -> &str {
        self.id
    }
    fn sign(&self, message: &[u8]) -> Vec<u8> {
        mac(self.secret, message)
    }
}

impl SnapshotKeys for Keyed {
    fn verify(&self, key_id: &str, _generation: u64, message: &[u8], signature: &[u8]) -> bool {
        key_id == self.id && mac(self.secret, message) == signature
    }
}

pub(crate) const KEY: Keyed = Keyed {
    id: "k1",
    secret: "s3cret",
};

pub(crate) fn params(tenant: &str, generation: u64, position: u64) -> SnapshotParams {
    SnapshotParams {
        tenant: tenant.to_owned(),
        generation,
        position,
        manifest_hash: "ab".repeat(32),
        taken_at_millis: 1_700_000_000_000,
    }
}

pub(crate) fn image(entity: &str, n: u64) -> ImageRecord {
    ImageRecord {
        entity: entity.to_owned(),
        id: format!("{n:06}"),
        version: n,
        entity_schema: 1 + n % 2,
        image: serde_json::json!({ "n": n, "note": format!("record number {n}") }),
    }
}

/// Copies a store, optionally altering one object's bytes or dropping one object.
pub(crate) fn copy(
    from: &MemoryStore,
    edit: impl Fn(&str, Vec<u8>) -> Option<Vec<u8>>,
) -> MemoryStore {
    let to = MemoryStore::new();
    let entries = block_on(from.list("", None, 100_000)).unwrap_or_default();
    for entry in entries {
        if let Some(object) = block_on(from.get(&entry.key)).ok().flatten()
            && let Some(bytes) = edit(&entry.key, object.bytes)
        {
            let _ = block_on(to.put_new(&entry.key, bytes));
        }
    }
    to
}
