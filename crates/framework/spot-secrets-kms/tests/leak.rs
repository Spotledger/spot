//! Nothing secret appears in an error, `Debug` output or what the KMS is sent beyond what it
//! must receive: the token, a data key and the derivation seed are canaries.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use spot_secrets::KeyProvider;
use std::time::Duration;
use support::*;

/// The canary data key: distinctive, 32 bytes.
const DATA_KEY: [u8; 32] = *b"CANARY-DATA-KEY-0123456789abcdef";

fn seed_bytes() -> Vec<u8> {
    (0..32u8).map(|i| 7u8.wrapping_add(i)).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every textual form the three canaries could leak in.
fn canaries() -> Vec<(&'static str, String)> {
    let mut out = vec![("token", TOKEN.to_owned())];
    for (label, bytes) in [("data key", DATA_KEY.to_vec()), ("seed", seed_bytes())] {
        out.push((label, STANDARD.encode(&bytes)));
        out.push((label, hex(&bytes)));
        out.push((label, format!("{bytes:?}")));
        if let Ok(text) = std::str::from_utf8(&bytes) {
            out.push((label, text.to_owned()));
        }
    }
    out
}

fn assert_clean(what: &str, text: &str) {
    for (label, needle) in canaries() {
        assert!(!text.contains(&needle), "{what} leaks the {label}");
    }
}

#[tokio::test]
async fn debug_output_and_errors_hold_no_canary() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let client = client(&fake, dir.path()).await;
    assert_clean("client Debug", &format!("{client:?}"));
    let provider = open(&fake, dir.path(), &clock).await;
    assert_clean("provider Debug", &format!("{provider:?}"));

    let wrapped = provider.wrap(&DATA_KEY, AAD).await.expect("wrap");
    assert_clean("wrapped key Debug", &format!("{wrapped:?}"));
    provider
        .unwrap(&wrapped.key, &wrapped, AAD)
        .await
        .expect("unwrap");

    // Errors from every refusal path.
    let mut errors: Vec<String> = Vec::new();
    errors.push(
        provider
            .unwrap(&wrapped.key, &wrapped, b"wrong aad")
            .await
            .err()
            .expect("tampered")
            .to_string(),
    );
    let mut altered = wrapped.clone();
    let last = altered.bytes.len() - 1;
    altered.bytes[last] = if altered.bytes[last] == b'A' {
        b'B'
    } else {
        b'A'
    };
    let key = altered.key.clone();
    errors.push(
        provider
            .unwrap(&key, &altered, AAD)
            .await
            .err()
            .expect("altered")
            .to_string(),
    );
    errors.push(
        provider
            .derive_key(&provider.current(), b"", 32)
            .await
            .err()
            .expect("bad info")
            .to_string(),
    );
    provider.flush_cache();
    fake.revoke_token();
    clock.advance(Duration::from_secs(1000));
    errors.push(
        provider
            .unwrap(&wrapped.key, &wrapped, AAD)
            .await
            .err()
            .expect("revoked")
            .to_string(),
    );
    errors.push(
        provider
            .wrap(&DATA_KEY, AAD)
            .await
            .err()
            .expect("revoked wrap")
            .to_string(),
    );
    fake.disable();
    errors.push(
        provider
            .refresh()
            .await
            .map(|()| String::new())
            .unwrap_or_else(|e| e.to_string()),
    );
    for text in &errors {
        assert_clean("an error", text);
        assert!(
            !text.to_lowercase().contains("vault:v"),
            "a ciphertext leaked: {text}"
        );
        assert!(
            !text.contains("permission denied"),
            "a response body leaked: {text}"
        );
    }
    assert!(
        errors.iter().any(|e| e.contains("spotkey")),
        "errors name the key"
    );
}

#[tokio::test]
async fn the_seed_never_reaches_the_kms_in_derived_form_and_keys_are_not_in_headers() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let provider = open(&fake, dir.path(), &clock()).await;
    let idx = provider
        .index_key(&provider.current())
        .await
        .expect("index key");
    let derived = provider
        .derive_key(&provider.current(), b"purpose", 32)
        .await
        .expect("derived");
    // The KMS saw the seed once, as the plaintext of the seed's own wrap, and never a derivative.
    let seen = fake.seen();
    for secret in [idx.expose(), derived.expose()] {
        assert!(!seen.contains(&STANDARD.encode(secret)));
        assert!(!seen.contains(&hex(secret)));
    }
    assert!(!seen.contains(TOKEN), "the token travels in a header only");
    // The derived keys themselves are not in any Debug text either.
    assert_clean("provider Debug", &format!("{provider:?}"));
    assert!(!format!("{provider:?}").contains(&hex(idx.expose())));
}
