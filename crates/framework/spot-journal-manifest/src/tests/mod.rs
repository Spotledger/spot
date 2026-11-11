//! Tests of the manifest model, its canonical encoding, its signature and its findings.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod append_tests;
mod canonical_tests;
mod findings_tests;
mod property_tests;
mod sign_tests;

use ed25519_dalek::SigningKey;

use crate::{
    Fork, Manifest, SegmentSummary, SignedManifest, Verified, VerifyingKeys, sign, verify,
};

/// A 32-byte hash that stands in for "the bytes hashed to `n`".
pub(crate) fn hash(n: u8) -> [u8; 32] {
    [n; 32]
}

/// The signing key of a fixed seed.
pub(crate) fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// The key id a test registers `seed` under.
pub(crate) fn key_id(seed: u8) -> String {
    format!("journal-key-{seed}")
}

/// A verifying key set trusting `seed` for the manifest generations `from..=to`.
pub(crate) fn keys(seed: u8, from: u64, to: u64) -> VerifyingKeys {
    let mut set = VerifyingKeys::new();
    set.add_key(&key_id(seed), key(seed).verifying_key(), from, to)
        .expect("the window is valid");
    set
}

/// Segments of `generation` tiling the positions `1..=total` in spans of `span`, each hashed from
/// `seed` and linked to the one before.
pub(crate) fn tiling(generation: u64, total: u64, span: u64, seed: u8) -> Vec<SegmentSummary> {
    let mut out = Vec::new();
    let mut first = 1;
    let mut previous = hash(seed);
    while first <= total {
        let last = first.saturating_add(span).saturating_sub(1).min(total);
        let segment_hash = hash(seed.wrapping_add(u8::try_from(out.len()).unwrap_or(u8::MAX)));
        out.push(SegmentSummary::new(
            generation,
            first,
            last,
            last - first + 1,
            segment_hash,
            previous,
        ));
        previous = segment_hash;
        first = last + 1;
    }
    out
}

/// A manifest of `generation` holding `segments`, with a fork record for every generation after
/// the first and a previous-manifest link (a generation-1 manifest carries neither).
pub(crate) fn manifest(source: &str, generation: u64, segments: Vec<SegmentSummary>) -> Manifest {
    let forks = (2..=generation)
        .map(|age| {
            Fork::new(
                age,
                10,
                format!("rebuild to generation {age}"),
                1_700_000_000_000,
            )
        })
        .collect();
    let previous_manifest_hash = if generation == 1 {
        None
    } else {
        Some(hash(0x5a))
    };
    Manifest {
        source: source.to_owned(),
        generation,
        segments,
        forks,
        previous_manifest_hash,
        issued_at_millis: 1_700_000_000_000,
    }
}

/// `manifest` signed by the fixed test key.
pub(crate) fn signed(manifest: &Manifest) -> SignedManifest {
    sign(manifest, &key(7), &key_id(7))
}

/// `manifest` signed by the fixed test key and verified under a set trusting it.
pub(crate) fn verified(manifest: &Manifest) -> Verified {
    verify(&signed(manifest), &keys(7, 1, 10)).expect("the test manifest verifies")
}

/// A manifest of generation 1 whose segments tile `1..=total`.
pub(crate) fn journal(total: u64, span: u64) -> Manifest {
    manifest("site:demo", 1, tiling(1, total, span, 3))
}
