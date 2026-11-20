//! The `spot-secrets` provider contract (round trip, wrong aad, seal and open through
//! `SecretBox`) against `KmsKeyProvider` over the fake Transit service.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use spot_secrets::{KeyProvider, Sealed, SecretBox, SecretString};
use support::*;

#[tokio::test]
async fn wrap_and_unwrap_round_trip() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let data_key = [9u8; 32];
    let wrapped = provider.wrap(&data_key, AAD).await.expect("wrap");
    assert_eq!(wrapped.key, provider.current());
    assert_eq!(wrapped.key.id, "kms:spotkey");
    assert_eq!(wrapped.key.version, 1);
    let plain = provider
        .unwrap(&wrapped.key, &wrapped, AAD)
        .await
        .expect("unwrap");
    assert_eq!(plain.expose(), &data_key);
}

#[tokio::test]
async fn a_secret_box_seals_and_opens_through_the_kms() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let sb = SecretBox::new(open(&fake, dir.path(), &clock()).await);
    let sealed = sb
        .seal(&SecretString::new("canary-value"), AAD)
        .await
        .expect("seal");
    let reparsed = Sealed::from_bytes(&sealed.to_bytes()).expect("parse");
    let opened = sb.open(&reparsed, AAD).await.expect("open");
    assert_eq!(opened.expose(), "canary-value");
}

#[tokio::test]
async fn the_wrong_aad_fails() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let wrapped = provider.wrap(&[1u8; 32], AAD).await.expect("wrap");
    let err = provider
        .unwrap(&wrapped.key, &wrapped, b"site=acme;field=username")
        .await
        .err()
        .expect("must fail");
    assert_eq!(err.code(), "secrets.tampered");
}

#[tokio::test]
async fn a_foreign_key_id_is_unknown() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let mut wrapped = provider.wrap(&[1u8; 32], AAD).await.expect("wrap");
    wrapped.key.id = "kms:other".into();
    let key = wrapped.key.clone();
    let err = provider
        .unwrap(&key, &wrapped, AAD)
        .await
        .err()
        .expect("must fail");
    assert_eq!(err.code(), "secrets.unknown_key");
}

#[tokio::test]
async fn open_refuses_a_disabled_key_and_an_unreachable_one() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    fake.disable();
    let err = open_with(&fake, dir.path(), &clock(), 7, |_| {})
        .await
        .err()
        .expect("disabled");
    assert!(
        err.to_string().contains("secrets_kms.key_disabled"),
        "{err}"
    );
    assert!(err.to_string().contains("spotkey"), "{err}");

    let fake = FakeTransit::new();
    fake.revoke_token();
    let err = open_with(&fake, dir.path(), &clock(), 7, |_| {})
        .await
        .err()
        .expect("token revoked");
    assert!(err.to_string().contains("secrets_kms.rejected"), "{err}");
}

#[tokio::test]
async fn out_of_range_configuration_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    type Tweak = fn(&mut spot_secrets_kms::ProviderConfig);
    let tweaks: [Tweak; 3] = [
        |c| c.cache_max_entries = 0,
        |c| c.cache_ttl = std::time::Duration::ZERO,
        |c| c.refresh_interval = std::time::Duration::ZERO,
    ];
    for tweak in tweaks {
        let err = open_with(&fake, dir.path(), &clock(), 7, tweak)
            .await
            .err()
            .expect("refused");
        assert!(
            err.to_string().contains("secrets_kms.config_invalid"),
            "{err}"
        );
    }
}
