#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Tests: rotation, tenancy, no key material in `Debug`, and the snapshot end to end.

use std::future::Future;

use ed25519_dalek::SigningKey;
use spot_journal_manifest::VerifyingKeys;

use crate::{Ed25519SnapshotSigner, ManifestSnapshotKeys};

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

pub(crate) fn secret(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

pub(crate) fn signer(id: &str, seed: u8) -> Ed25519SnapshotSigner {
    Ed25519SnapshotSigner::new(id, secret(seed))
}

/// A key set trusting each `(id, seed, from, to)` window.
pub(crate) fn keys(windows: &[(&str, u8, u64, u64)]) -> ManifestSnapshotKeys {
    let mut set = VerifyingKeys::new();
    for (id, seed, from, to) in windows {
        set.add_key(id, secret(*seed).verifying_key(), *from, *to)
            .unwrap();
    }
    ManifestSnapshotKeys::new(set)
}
