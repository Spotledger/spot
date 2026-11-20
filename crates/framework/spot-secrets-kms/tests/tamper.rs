//! Altered or truncated stored values are refused, never opened.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use spot_secrets::{Sealed, SecretBox, SecretString};
use support::*;

const REJECTED: [&str; 5] = [
    "secrets.tampered",
    "secrets.malformed",
    "secrets.unknown_key",
    "secrets.retired_key",
    "secrets.provider",
];

#[tokio::test]
async fn every_flipped_byte_is_rejected() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let sb = SecretBox::new(open(&fake, dir.path(), &clock()).await);
    let sealed = sb
        .seal(&SecretString::new("canary-value"), AAD)
        .await
        .expect("seal");
    let bytes = sealed.to_bytes();
    for i in 0..bytes.len() {
        let mut copy = bytes.clone();
        copy[i] ^= 0x01;
        let outcome = match Sealed::from_bytes(&copy) {
            Err(e) => Err(e.code()),
            Ok(s) => sb.open(&s, AAD).await.map_err(|e| e.code()),
        };
        let code = outcome.err().unwrap_or_else(|| panic!("byte {i} accepted"));
        assert!(REJECTED.contains(&code), "byte {i}: {code}");
    }
}

#[tokio::test]
async fn every_truncation_is_rejected() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let sb = SecretBox::new(open(&fake, dir.path(), &clock()).await);
    let sealed = sb
        .seal(&SecretString::new("canary-value"), AAD)
        .await
        .expect("seal");
    let bytes = sealed.to_bytes();
    for len in 0..bytes.len() {
        let outcome = match Sealed::from_bytes(&bytes[..len]) {
            Err(e) => Err(e.code()),
            Ok(s) => sb.open(&s, AAD).await.map_err(|e| e.code()),
        };
        let code = outcome
            .err()
            .unwrap_or_else(|| panic!("len {len} accepted"));
        assert!(REJECTED.contains(&code), "len {len}: {code}");
    }
}

#[tokio::test]
async fn an_altered_wrapped_key_is_tampered() {
    use spot_secrets::KeyProvider;
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let mut wrapped = provider.wrap(&[3u8; 32], AAD).await.expect("wrap");
    let last = wrapped.bytes.len() - 1;
    wrapped.bytes[last] = if wrapped.bytes[last] == b'A' {
        b'B'
    } else {
        b'A'
    };
    let key = wrapped.key.clone();
    let err = provider
        .unwrap(&key, &wrapped, AAD)
        .await
        .err()
        .expect("must fail");
    assert!(REJECTED.contains(&err.code()), "{err}");
    assert_eq!(provider.cache_len(), 0, "a failed unwrap caches nothing");
}
