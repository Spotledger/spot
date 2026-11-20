//! Blind-index and derived keys: stable across a transit rotation and a restart, separated by
//! key id, info and length, and bounded like `spot-secrets`.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use spot_secrets::{
    BlindIndex, KeyProvider, MAX_DERIVE_INFO_LEN, MAX_DERIVED_LEN, MIN_DERIVED_LEN, SecretBox,
};
use std::time::Duration;
use support::*;

async fn index(p: &impl KeyProvider) -> Vec<u8> {
    p.index_key(&p.current())
        .await
        .expect("index key")
        .expose()
        .to_vec()
}

async fn derive(p: &impl KeyProvider, info: &[u8], len: usize) -> Vec<u8> {
    p.derive_key(&p.current(), info, len)
        .await
        .expect("derived")
        .expose()
        .to_vec()
}

#[tokio::test]
async fn the_index_key_is_32_bytes_and_deterministic() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let a = index(&provider).await;
    assert_eq!(a.len(), 32);
    assert_eq!(a, index(&provider).await);
    assert!(a.iter().any(|b| *b != 0));
}

#[tokio::test]
async fn blind_indexes_and_derived_keys_survive_a_transit_rotation() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let sb = SecretBox::new(open(&fake, dir.path(), &clock).await);
    let before_index = sb
        .blind_index("session.token_digest", b"t0ken")
        .await
        .expect("index");
    let before_key = derive(sb.provider(), b"purpose", 32).await;
    let index_before = index(sb.provider()).await;
    let before_version = sb.provider().current().version;

    fake.rotate();
    clock.advance(Duration::from_secs(61));
    sb.provider()
        .wrap(&[1u8; 32], AAD)
        .await
        .expect("a wrap learns the new version");
    assert_eq!(sb.provider().current().version, before_version + 1);

    let after_index: BlindIndex = sb
        .blind_index("session.token_digest", b"t0ken")
        .await
        .expect("index");
    // Only the version byte moves (it tells the host to re-stamp the row); the MAC, which is
    // what a lookup matches, is the same, so no stored index becomes unmatchable.
    assert!(sb.needs_reindex(&before_index));
    assert_eq!(before_index.to_bytes()[1..], after_index.to_bytes()[1..]);
    assert_eq!(
        sb.provider()
            .index_key(&sb.provider().current())
            .await
            .expect("key")
            .expose(),
        index_before.as_slice()
    );
    assert_eq!(before_key, derive(sb.provider(), b"purpose", 32).await);
}

#[tokio::test]
async fn keys_survive_a_restart_from_the_seed_file() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let first = open_with(&fake, dir.path(), &clock, 7, |_| {})
        .await
        .expect("first start");
    let (idx, key) = (index(&first).await, derive(&first, b"purpose", 48).await);
    drop(first);
    // A different entropy source: a restart must read the seed file, not draw a new seed.
    let second = open_with(&fake, dir.path(), &clock, 99, |_| {})
        .await
        .expect("second start");
    assert_eq!(idx, index(&second).await);
    assert_eq!(key, derive(&second, b"purpose", 48).await);
}

#[tokio::test]
async fn keys_differ_across_key_ids() {
    let (dir_a, dir_b) = (
        tempfile::tempdir().expect("dir"),
        tempfile::tempdir().expect("dir"),
    );
    let (fake_a, fake_b) = (FakeTransit::named("alpha"), FakeTransit::named("beta"));
    let clock = clock();
    // The same entropy (so the same seed bytes) under two key ids still derives different keys.
    let a = open_with(&fake_a, dir_a.path(), &clock, 7, |_| {})
        .await
        .expect("alpha");
    let b = open_with(&fake_b, dir_b.path(), &clock, 7, |_| {})
        .await
        .expect("beta");
    assert_eq!(a.current().id, "kms:alpha");
    assert_eq!(b.current().id, "kms:beta");
    assert_ne!(index(&a).await, index(&b).await);
    assert_ne!(
        derive(&a, b"purpose", 32).await,
        derive(&b, b"purpose", 32).await
    );
}

#[tokio::test]
async fn a_different_info_or_length_gives_a_different_key() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let base = derive(&provider, b"purpose", 32).await;
    assert_ne!(base, derive(&provider, b"purpose2", 32).await);
    assert_ne!(base, derive(&provider, b"Purpose", 32).await);
    let short = derive(&provider, b"purpose", MIN_DERIVED_LEN).await;
    let long = derive(&provider, b"purpose", MAX_DERIVED_LEN).await;
    assert_eq!(short.len(), MIN_DERIVED_LEN);
    assert_eq!(long.len(), MAX_DERIVED_LEN);
    assert_ne!(base, short);
    assert_ne!(base, long);
    // A derived key is never the blind-index key.
    assert_ne!(
        index(&provider).await,
        derive(&provider, b"purpose", 32).await
    );
}

#[tokio::test]
async fn derive_key_enforces_the_spot_secrets_bounds() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let key = provider.current();
    for (info, len) in [
        (&b""[..], 32),
        (&[b'x'; MAX_DERIVE_INFO_LEN + 1][..], 32),
        (&b"purpose"[..], MIN_DERIVED_LEN - 1),
        (&b"purpose"[..], MAX_DERIVED_LEN + 1),
    ] {
        let err = provider
            .derive_key(&key, info, len)
            .await
            .err()
            .expect("refused");
        assert_eq!(err.code(), "secrets.derive_key");
    }
    assert!(
        provider
            .derive_key(&key, &[b'x'; MAX_DERIVE_INFO_LEN], 32)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_foreign_or_future_key_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let mut other = provider.current();
    other.id = "kms:other".into();
    assert_eq!(
        provider
            .index_key(&other)
            .await
            .err()
            .expect("refused")
            .code(),
        "secrets.unknown_key"
    );
    let mut future = provider.current();
    future.version += 5;
    assert_eq!(
        provider
            .derive_key(&future, b"purpose", 32)
            .await
            .err()
            .expect("refused")
            .code(),
        "secrets.unknown_key_version"
    );
}
