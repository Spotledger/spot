//! The data-key cache: a hit never contacts the KMS, the cache is bounded, and
//! `flush_cache` forces the next unwrap to ask again.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use spot_secrets::KeyProvider;
use support::*;

#[tokio::test]
async fn a_hit_never_contacts_the_kms_and_a_flush_forces_a_call() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let wrapped = provider.wrap(&[4u8; 32], AAD).await.expect("wrap");
    let start = fake.decrypts();
    provider
        .unwrap(&wrapped.key, &wrapped, AAD)
        .await
        .expect("miss");
    assert_eq!(fake.decrypts(), start + 1);
    for _ in 0..5 {
        provider
            .unwrap(&wrapped.key, &wrapped, AAD)
            .await
            .expect("hit");
    }
    assert_eq!(fake.decrypts(), start + 1, "hits are served from memory");
    assert_eq!(provider.cache_len(), 1);

    provider.flush_cache();
    assert_eq!(provider.cache_len(), 0);
    provider
        .unwrap(&wrapped.key, &wrapped, AAD)
        .await
        .expect("after flush");
    assert_eq!(fake.decrypts(), start + 2, "a flush forces a KMS call");
}

#[tokio::test]
async fn the_cache_is_bounded_in_count() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open_with(&fake, dir.path(), &clock(), 7, |c| c.cache_max_entries = 3)
        .await
        .expect("open");
    for i in 0..10u8 {
        let wrapped = provider.wrap(&[i + 1; 32], AAD).await.expect("wrap");
        provider
            .unwrap(&wrapped.key, &wrapped, AAD)
            .await
            .expect("unwrap");
        assert!(provider.cache_len() <= 3, "len {}", provider.cache_len());
    }
    assert_eq!(provider.cache_len(), 3);
}

#[tokio::test]
async fn the_cache_key_binds_the_aad() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let wrapped = provider.wrap(&[6u8; 32], AAD).await.expect("wrap");
    provider
        .unwrap(&wrapped.key, &wrapped, AAD)
        .await
        .expect("fills");
    // A cached key opened under the right aad must not open for another aad.
    let err = provider
        .unwrap(&wrapped.key, &wrapped, b"another aad")
        .await
        .err()
        .expect("wrong aad refused");
    assert_eq!(err.code(), "secrets.tampered");
}

#[tokio::test]
async fn a_failed_unwrap_caches_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let wrapped = provider.wrap(&[6u8; 32], AAD).await.expect("wrap");
    assert!(
        provider
            .unwrap(&wrapped.key, &wrapped, b"nope")
            .await
            .is_err()
    );
    assert_eq!(provider.cache_len(), 0);
}
