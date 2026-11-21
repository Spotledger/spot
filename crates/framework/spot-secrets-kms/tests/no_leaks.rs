//! No test output, `Debug` or error holds the canary token, the canary data key or the seed.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use spot_secrets::KeyProvider;
use std::time::Duration;
use support::*;

const DATA_KEY: &[u8; 32] = b"CANARY-DATA-KEY-0123456789abcdef";

fn canaries() -> Vec<String> {
    let seed: Vec<u8> = (0..32u8).map(|i| 7u8.wrapping_add(i)).collect();
    let mut out = vec![
        TOKEN.to_owned(),
        String::from_utf8_lossy(DATA_KEY).into_owned(),
        STANDARD.encode(DATA_KEY),
        STANDARD.encode(&seed),
    ];
    out.push(seed.iter().map(|b| format!("{b:02x}")).collect());
    out.push(format!("{seed:?}"));
    out
}

fn assert_clean(label: &str, text: &str) {
    for canary in canaries() {
        assert!(!text.contains(&canary), "{label} leaks a canary: {text}");
    }
}

#[tokio::test]
async fn nothing_leaks_on_success_or_failure() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let p = open(&fake, dir.path(), &clock).await;
    assert_clean("provider debug", &format!("{p:?}"));

    let wrapped = p.wrap(DATA_KEY, AAD).await.expect("wrap");
    assert_clean("wrapped debug", &format!("{wrapped:?}"));
    let plain = p.unwrap(&wrapped.key, &wrapped, AAD).await.expect("unwrap");
    assert_clean("secret bytes debug", &format!("{plain:?} {plain}"));
    let idx = p.index_key(&p.current()).await.expect("index");
    assert_clean("index key debug", &format!("{idx:?}"));
    let derived = p
        .derive_key(&p.current(), b"purpose", 32)
        .await
        .expect("derive");
    assert_clean("derived key debug", &format!("{derived:?}"));

    // The seed file holds no seed in the clear.
    assert_clean(
        "seed file",
        &std::fs::read_to_string(seed_path(dir.path())).expect("seed file"),
    );
    // The KMS is sent data keys to encrypt (that is the protocol), but never the token.
    assert!(!fake.seen().contains(TOKEN));

    // Failure paths: wrong aad, a revoked token, a disabled key, a bad derive request.
    let mut errors = Vec::new();
    errors.push(
        p.unwrap(&wrapped.key, &wrapped, b"wrong")
            .await
            .err()
            .expect("aad"),
    );
    errors.push(
        p.derive_key(&p.current(), b"", 32)
            .await
            .err()
            .expect("derive"),
    );
    fake.revoke_token();
    p.flush_cache();
    errors.push(
        p.unwrap(&wrapped.key, &wrapped, AAD)
            .await
            .err()
            .expect("revoked"),
    );
    errors.push(p.wrap(DATA_KEY, AAD).await.err().expect("revoked wrap"));
    fake.disable();
    clock.advance(Duration::from_secs(61));
    errors.push(p.wrap(DATA_KEY, AAD).await.err().expect("disabled"));
    for e in errors {
        assert_clean("error", &format!("{e} | {e:?}"));
    }
}
