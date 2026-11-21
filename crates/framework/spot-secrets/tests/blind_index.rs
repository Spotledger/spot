//! The blind index: determinism, scope and value separation, rotation, and the format.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

use proptest::prelude::*;
use spot_secrets::{BlindIndex, INDEX_LEN, KeyProvider, KeyRef, MemoryKeyring, SecretBox};
use std::sync::Arc;

/// A 32-byte KEK with a fixed value, so the derivation is a committed golden vector.
const GOLDEN_KEK: &str = "c3BvdC1ibGluZC1pbmRleC1nb2xkZW4ta2V5LTMyYiE=";
const GOLDEN_INDEX_KEY: &str = "9a87e320ec82c47e76333427055e71abd5f71fcac32170baed1f6db762d65906";
const GOLDEN_INDEX: &str = "01e424dc7068ccecb16e5e7e076892adcef46a48e7cdb8478c7fccfa8fb456d7bf";
const GOLDEN_INDEX_OTHER_SCOPE: &str =
    "01fba7e272ca7dfae2dc591e5d95d4d8294c7167ddf9ef6a62b2f9dc6feb591926";
const SCOPE: &str = "session.token_digest";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn golden_box() -> SecretBox<spot_secrets::EnvKey> {
    SecretBox::new(spot_secrets::EnvKey::from_base64(GOLDEN_KEK).expect("env key"))
}

#[tokio::test]
async fn golden_vector_is_stable() {
    // The index key is HKDF-SHA256(kek, salt = empty, info = "spot blind index v1"); the index
    // is version || HMAC-SHA256(index_key, scope || 0x00 || value). A refactor that changes
    // either derivation changes every index in an installation, so this vector must hold.
    let sb = golden_box();
    let index = sb
        .blind_index(SCOPE, b"golden-session-token")
        .await
        .expect("index");
    assert_eq!(hex(index.to_bytes().as_slice()), GOLDEN_INDEX);
    // The MAC of a different scope, over the same value, is a different 32 bytes.
    let other = sb
        .blind_index("user.password", b"golden-session-token")
        .await
        .expect("index");
    assert_eq!(hex(other.to_bytes().as_slice()), GOLDEN_INDEX_OTHER_SCOPE);
}

#[tokio::test]
async fn index_key_is_the_documented_derivation() {
    let provider = spot_secrets::EnvKey::from_base64(GOLDEN_KEK).expect("env key");
    let key = provider
        .index_key(&provider.current())
        .await
        .expect("index key");
    assert_eq!(hex(key.expose()), GOLDEN_INDEX_KEY);
    // It is 32 bytes and is not the KEK itself.
    assert_eq!(key.expose().len(), 32);
    assert_ne!(key.expose(), [32u8; 32].as_slice());
}

#[tokio::test]
async fn index_is_deterministic_and_separates_scope_and_value() {
    let sb = golden_box();
    let a = sb.blind_index(SCOPE, b"t0ken").await.expect("index");
    let b = sb.blind_index(SCOPE, b"t0ken").await.expect("index");
    assert_eq!(a, b, "same scope, value and key give the same index");

    let other_scope = sb
        .blind_index("api_key.key_digest", b"t0ken")
        .await
        .expect("index");
    let other_value = sb.blind_index(SCOPE, b"t0kem").await.expect("index");
    assert_ne!(
        a, other_scope,
        "equal values in two fields must not collide"
    );
    assert_ne!(a, other_value);

    // A value that starts with another scope's text is still separated by the 0x00 byte.
    let edge = sb
        .blind_index(SCOPE, format!("{SCOPE}\0x").as_bytes())
        .await
        .expect("index");
    assert_ne!(a, edge);
}

#[tokio::test]
async fn rotation_changes_the_version_byte_and_the_index() {
    let ring = Arc::new(MemoryKeyring::new("rotating").expect("keyring"));
    let sb = SecretBox::new(Arc::clone(&ring));
    let before = sb.blind_index(SCOPE, b"t0ken").await.expect("index");
    assert_eq!(before.version(), 1);
    assert!(!sb.needs_reindex(&before));

    let version = ring.rotate().expect("rotate");
    assert_eq!(version, 2);
    let after = sb.blind_index(SCOPE, b"t0ken").await.expect("index");
    assert_eq!(after.version(), 2);
    assert_ne!(before.to_bytes(), after.to_bytes());
    assert_eq!(before.to_bytes()[0], 1);
    assert_eq!(after.to_bytes()[0], 2);
    assert!(sb.needs_reindex(&before), "an old index must be recomputed");
    assert!(!sb.needs_reindex(&after));
}

#[tokio::test]
async fn the_index_never_contains_the_value() {
    let sb = golden_box();
    for value in [
        &b"hunter2"[..],
        &b""[..],
        &b"\x00\x01\x02\x03"[..],
        &[0xff; 32][..],
    ] {
        let index = sb.blind_index(SCOPE, value).await.expect("index");
        let bytes = index.to_bytes();
        assert_eq!(bytes.len(), INDEX_LEN);
        if value.len() >= 3 {
            assert!(
                !bytes.windows(value.len()).any(|w| w == value),
                "the value must not appear in the index"
            );
            assert_ne!(&bytes[1..], value);
        }
    }
}

#[tokio::test]
async fn stored_form_round_trips_and_is_checked() {
    let sb = golden_box();
    let index = sb.blind_index(SCOPE, b"t0ken").await.expect("index");
    let bytes = index.to_bytes();
    assert_eq!(BlindIndex::from_bytes(&bytes).expect("parse"), index);
    assert_eq!(BlindIndex::from_bytes(&bytes).expect("parse").version(), 1);

    let too_long = [bytes.as_slice(), &[0u8][..]].concat();
    for bad in [&bytes[..32], too_long.as_slice()] {
        let err = BlindIndex::from_bytes(bad).err().expect("must fail");
        assert_eq!(err.code(), "secrets.malformed");
    }

    // A zero version byte was never written by this crate: versions start at 1.
    let mut zero = bytes;
    zero[0] = 0;
    let err = BlindIndex::from_bytes(&zero).err().expect("must fail");
    assert_eq!(err.code(), "secrets.unknown_key_version");
}

#[tokio::test]
async fn a_bad_scope_is_refused_before_any_key_work() {
    let sb = golden_box();
    for bad in [
        "",
        "session",
        "session.token.extra",
        "Session.token_digest",
        "1session.token",
        "session.token-digest",
        "session..token",
    ] {
        let err = sb.blind_index(bad, b"v").await.err().expect("must fail");
        assert_eq!(err.code(), "secrets.malformed", "scope {bad:?}");
    }
    // The `__` infix is the engine's: a scope naming it could collide with a managed column.
    for reserved in ["session.token__sealed", "user__x.password", "__.x"] {
        let err = sb
            .blind_index(reserved, b"v")
            .await
            .err()
            .expect("must fail");
        assert_eq!(err.code(), "secrets.reserved_name", "scope {reserved:?}");
    }
}

/// A provider that implements only wrapping: it must refuse to derive an index key rather than
/// derive one from something else.
struct NoIndexKeys(Arc<MemoryKeyring>);

impl KeyProvider for NoIndexKeys {
    fn current(&self) -> KeyRef {
        self.0.current()
    }

    fn wrap<'a>(
        &'a self,
        data_key: &'a [u8],
        aad: &'a [u8],
    ) -> spot_secrets::ProviderFuture<'a, spot_secrets::WrappedKey> {
        self.0.wrap(data_key, aad)
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a spot_secrets::WrappedKey,
        aad: &'a [u8],
    ) -> spot_secrets::ProviderFuture<'a, spot_secrets::SecretBytes> {
        self.0.unwrap(key, wrapped, aad)
    }
}

#[tokio::test]
async fn a_provider_without_index_keys_refuses() {
    let ring = Arc::new(MemoryKeyring::new("plain").expect("keyring"));
    let sb = SecretBox::new(NoIndexKeys(ring));
    let err = sb
        .blind_index(SCOPE, b"the-token-value")
        .await
        .err()
        .expect("must fail");
    assert_eq!(err.code(), "secrets.index_key");
    assert!(
        !err.to_string().contains("the-token-value"),
        "the message must not repeat the value"
    );
}

#[tokio::test]
async fn a_retired_key_cannot_derive_an_index_key() {
    let ring = Arc::new(MemoryKeyring::new("retiring").expect("keyring"));
    let sb = SecretBox::new(Arc::clone(&ring));
    let before = sb.blind_index(SCOPE, b"v").await.expect("index");
    ring.rotate().expect("rotate");
    ring.retire(before.version().into(), false).expect("retire");
    // The current version still derives (a new value is indexed under it) ...
    assert!(sb.blind_index(SCOPE, b"v").await.is_ok());
    // ... but the retired version's index key is gone with its key material.
    let err = ring
        .index_key(&KeyRef {
            id: "retiring".into(),
            version: before.version().into(),
        })
        .await
        .err()
        .expect("must fail");
    assert_eq!(err.code(), "secrets.retired_key");
}

proptest! {
    /// An arbitrary value never panics, never appears in the index and never equals it.
    #[test]
    fn arbitrary_values_never_leak(value: Vec<u8>) {
        let ring = Arc::new(MemoryKeyring::new("prop").expect("keyring"));
        let sb = SecretBox::new(ring);
        let index = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(sb.blind_index(SCOPE, &value))
            .expect("index");
        let bytes = index.to_bytes();
        prop_assert_eq!(bytes.len(), INDEX_LEN);
        prop_assert_ne!(bytes.as_slice(), value.as_slice());
        if value.len() >= 4 {
            prop_assert!(!bytes.windows(value.len()).any(|w| w == value.as_slice()));
        }
    }
}
