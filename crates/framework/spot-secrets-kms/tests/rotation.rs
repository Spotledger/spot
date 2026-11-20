//! Transit key rotation, retired versions, and revocation bounded by the cache lifetime.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use spot_secrets::{KeyProvider, SecretBox, SecretString};
use std::time::Duration;
use support::*;

#[tokio::test]
async fn a_transit_rotation_moves_new_wraps_and_old_ones_still_open() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let provider = open(&fake, dir.path(), &clock).await;
    let old = provider.wrap(&[1u8; 32], AAD).await.expect("wrap v1");
    assert_eq!(old.key.version, 1);

    fake.rotate();
    // `current` follows the KMS at the next refresh interval, and a wrap learns the new version
    // from the KMS answer at once.
    clock.advance(Duration::from_secs(61));
    let fresh = provider.wrap(&[2u8; 32], AAD).await.expect("wrap v2");
    assert_eq!(fresh.key.version, 2);
    assert_eq!(provider.current().version, 2);

    let plain_old = provider.unwrap(&old.key, &old, AAD).await.expect("open v1");
    assert_eq!(plain_old.expose(), &[1u8; 32]);
    let plain_new = provider
        .unwrap(&fresh.key, &fresh, AAD)
        .await
        .expect("open v2");
    assert_eq!(plain_new.expose(), &[2u8; 32]);
}

#[tokio::test]
async fn rewrap_moves_a_data_key_to_the_latest_version() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let provider = open(&fake, dir.path(), &clock).await;
    let old = provider.wrap(&[5u8; 32], AAD).await.expect("wrap");
    fake.rotate();
    let moved = provider.rewrap(&old, AAD).await.expect("rewrap");
    assert_eq!(moved.key.version, 2);
    let plain = provider
        .unwrap(&moved.key, &moved, AAD)
        .await
        .expect("open");
    assert_eq!(plain.expose(), &[5u8; 32]);
    assert!(
        provider.rewrap(&old, b"another aad").await.is_err(),
        "a rewrap checks the aad"
    );
}

#[tokio::test]
async fn a_raised_minimum_decryption_version_refuses_version_one() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let provider = open(&fake, dir.path(), &clock).await;
    let v1 = provider.wrap(&[1u8; 32], AAD).await.expect("wrap v1");
    fake.rotate();
    fake.set_min_decryption(2);
    provider.flush_cache();
    // Within the refresh interval the provider has not heard yet; the KMS's own refusal makes it
    // look again, and the answer is the distinct code.
    let err = provider
        .unwrap(&v1.key, &v1, AAD)
        .await
        .err()
        .expect("retired");
    assert!(
        err.to_string().contains("secrets_kms.version_retired"),
        "{err}"
    );
    assert!(err.to_string().contains("spotkey"), "{err}");
    // And once it knows, it refuses without asking.
    let before = fake.decrypts();
    let again = provider
        .unwrap(&v1.key, &v1, AAD)
        .await
        .err()
        .expect("retired");
    assert!(again.to_string().contains("secrets_kms.version_retired"));
    assert_eq!(fake.decrypts(), before);
}

#[tokio::test]
async fn a_revoked_token_stops_unwrapping_only_after_the_cache_lifetime() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let provider = open_with(&fake, dir.path(), &clock, 7, |c| {
        c.cache_ttl = Duration::from_secs(300);
    })
    .await
    .expect("open");
    let wrapped = provider.wrap(&[8u8; 32], AAD).await.expect("wrap");
    provider
        .unwrap(&wrapped.key, &wrapped, AAD)
        .await
        .expect("first unwrap fills the cache");

    fake.revoke_token();
    clock.advance(Duration::from_secs(299));
    let calls = fake.decrypts();
    let hit = provider
        .unwrap(&wrapped.key, &wrapped, AAD)
        .await
        .expect("a young entry is still served");
    assert_eq!(hit.expose(), &[8u8; 32]);
    assert_eq!(fake.decrypts(), calls, "a hit never contacts the KMS");

    clock.advance(Duration::from_secs(2));
    let err = provider
        .unwrap(&wrapped.key, &wrapped, AAD)
        .await
        .err()
        .expect("an old entry is refused");
    assert!(err.to_string().contains("secrets_kms.rejected"), "{err}");
}

#[tokio::test]
async fn a_disabled_key_refuses_wraps_and_unwraps_after_the_lifetime() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let sb = SecretBox::new(open(&fake, dir.path(), &clock).await);
    let sealed = sb.seal(&SecretString::new("v"), AAD).await.expect("seal");
    sb.open(&sealed, AAD).await.expect("open fills the cache");

    fake.disable();
    clock.advance(Duration::from_secs(61));
    // The refresh interval passed: the provider now knows the key is disabled.
    let err = sb
        .seal(&SecretString::new("w"), AAD)
        .await
        .err()
        .expect("wrap refused");
    assert!(
        err.to_string().contains("secrets_kms.key_disabled"),
        "{err}"
    );
    // The cached data key is still served inside the lifetime, and not after it.
    assert!(sb.open(&sealed, AAD).await.is_ok());
    clock.advance(Duration::from_secs(300));
    assert!(sb.open(&sealed, AAD).await.is_err());
}
