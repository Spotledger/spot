//! `KeyProvider::derive_key`: determinism, separation, bounds, refusal by default and silence.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

use spot_secrets::{
    DERIVE_PREFIX, EnvKey, INDEX_KEY_INFO, KeyProvider, KeyRef, MAX_DERIVE_INFO_LEN,
    MAX_DERIVED_LEN, MIN_DERIVED_LEN, MemoryKeyring, ProviderFuture, SecretBytes, SecretError,
    WrappedKey,
};

const KEK: &str = "c3BvdC1ibGluZC1pbmRleC1nb2xkZW4ta2V5LTMyYiE=";
const OTHER_KEK: &str = "b3RoZXIta2V5LW90aGVyLWtleS1vdGhlci1rZXktMzI=";

fn env(kek: &str) -> EnvKey {
    EnvKey::from_base64(kek).expect("env key")
}

async fn derive(p: &impl KeyProvider, info: &[u8], len: usize) -> Vec<u8> {
    p.derive_key(&p.current(), info, len)
        .await
        .expect("derived")
        .expose()
        .to_vec()
}

#[tokio::test]
async fn the_same_inputs_give_the_same_output() {
    let p = env(KEK);
    assert_eq!(
        derive(&p, b"purpose", 32).await,
        derive(&p, b"purpose", 32).await
    );
    // A second provider built from the same KEK derives the same key (a restart).
    assert_eq!(
        derive(&p, b"purpose", 32).await,
        derive(&env(KEK), b"purpose", 32).await
    );
}

#[tokio::test]
async fn a_different_info_kek_or_length_gives_a_different_output() {
    let p = env(KEK);
    let base = derive(&p, b"purpose", 32).await;
    assert_ne!(base, derive(&p, b"purpose2", 32).await);
    assert_ne!(base, derive(&p, b"Purpose", 32).await);
    assert_ne!(base, derive(&env(OTHER_KEK), b"purpose", 32).await);
    let short = derive(&p, b"purpose", 16).await;
    let long = derive(&p, b"purpose", 64).await;
    assert_ne!(base, short);
    assert_ne!(base, long);
    assert_eq!(short.len(), 16);
    assert_eq!(long.len(), 64);
    // Standard HKDF: the longer output extends the shorter one.
    assert_eq!(&long[..32], base.as_slice());
}

#[tokio::test]
async fn a_different_key_version_gives_a_different_output() {
    let ring = MemoryKeyring::new("throttle").expect("keyring");
    let v1 = ring.current();
    ring.rotate().expect("rotate");
    let v2 = ring.current();
    assert_ne!(v1.version, v2.version);
    let a = ring.derive_key(&v1, b"purpose", 32).await.expect("v1");
    let b = ring.derive_key(&v2, b"purpose", 32).await.expect("v2");
    assert_ne!(a.expose(), b.expose());
    // A version the provider never had, and one it retired, are errors.
    let unknown = KeyRef {
        id: v1.id.clone(),
        version: 99,
    };
    assert_eq!(
        ring.derive_key(&unknown, b"purpose", 32)
            .await
            .err()
            .expect("unknown")
            .code(),
        "secrets.unknown_key"
    );
    ring.retire(v1.version, false).expect("retire");
    assert_eq!(
        ring.derive_key(&v1, b"purpose", 32)
            .await
            .err()
            .expect("retired")
            .code(),
        "secrets.retired_key"
    );
    let foreign = KeyRef {
        id: "elsewhere".into(),
        version: v2.version,
    };
    assert!(ring.derive_key(&foreign, b"purpose", 32).await.is_err());
}

#[tokio::test]
async fn the_output_differs_from_the_index_key_and_from_every_other_label() {
    let p = env(KEK);
    let index = p.index_key(&p.current()).await.expect("index key");
    // The label of the index key, passed as a caller label, does not reproduce it.
    assert_ne!(derive(&p, INDEX_KEY_INFO, 32).await, index.expose());
    assert_ne!(derive(&p, b"spot blind index v1", 32).await, index.expose());
    // The prefix is what separates the labels: nothing the crate uses starts with it.
    assert!(!INDEX_KEY_INFO.starts_with(DERIVE_PREFIX));
    assert!(DERIVE_PREFIX.ends_with(&[0]));
}

#[tokio::test]
async fn the_length_and_info_bounds_refuse() {
    let p = env(KEK);
    let key = p.current();
    for len in [0, 1, MIN_DERIVED_LEN - 1, MAX_DERIVED_LEN + 1, 1_000] {
        let error = p
            .derive_key(&key, b"purpose", len)
            .await
            .err()
            .expect("refused");
        assert!(matches!(error, SecretError::DeriveKey(_)), "len {len}");
        assert_eq!(error.code(), "secrets.derive_key", "len {len}");
        assert!(
            error.to_string().starts_with("secrets.derive_key:"),
            "{error}"
        );
    }
    for len in [MIN_DERIVED_LEN, 17, 33, 63, MAX_DERIVED_LEN] {
        assert_eq!(derive(&p, b"purpose", len).await.len(), len);
    }
    assert_eq!(
        p.derive_key(&key, b"", 32)
            .await
            .err()
            .expect("empty")
            .code(),
        "secrets.derive_key"
    );
    let longest = vec![b'x'; MAX_DERIVE_INFO_LEN];
    assert_eq!(derive(&p, &longest, 32).await.len(), 32);
    let too_long = vec![b'x'; MAX_DERIVE_INFO_LEN + 1];
    assert_eq!(
        p.derive_key(&key, &too_long, 32)
            .await
            .err()
            .expect("long")
            .code(),
        "secrets.derive_key"
    );
}

/// A provider that implements nothing optional.
struct Plain(MemoryKeyring);

impl KeyProvider for Plain {
    fn current(&self) -> KeyRef {
        self.0.current()
    }
    fn wrap<'a>(&'a self, d: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        self.0.wrap(d, aad)
    }
    fn unwrap<'a>(
        &'a self,
        k: &'a KeyRef,
        w: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        self.0.unwrap(k, w, aad)
    }
}

#[tokio::test]
async fn a_provider_that_does_not_opt_in_refuses() {
    let p = Plain(MemoryKeyring::new("plain").expect("keyring"));
    let error = p
        .derive_key(&p.current(), b"purpose", 32)
        .await
        .err()
        .expect("refused");
    assert!(matches!(error, SecretError::DeriveKey(_)));
    assert_eq!(error.code(), "secrets.derive_key");
}

#[tokio::test]
async fn smart_pointers_and_trait_objects_forward() {
    let arc = std::sync::Arc::new(env(KEK));
    let boxed: Box<dyn KeyProvider> = Box::new(env(KEK));
    let direct = derive(&env(KEK), b"purpose", 32).await;
    assert_eq!(derive(&arc, b"purpose", 32).await, direct);
    assert_eq!(derive(&boxed, b"purpose", 32).await, direct);
}

#[tokio::test]
async fn neither_an_error_nor_a_debug_carries_the_label_or_the_key() {
    let p = env(KEK);
    let label = b"super-secret-label-0123";
    let derived = p
        .derive_key(&p.current(), label, 32)
        .await
        .expect("derived");
    let debug = format!("{derived:?}");
    assert!(debug.contains("redacted"), "{debug}");
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    assert!(!debug.contains(&hex(derived.expose())));
    // Every refusal that takes the label in hand leaves it out.
    for error in [
        p.derive_key(&p.current(), label, 8)
            .await
            .err()
            .expect("len"),
        p.derive_key(&p.current(), &[b'S'; 300], 32)
            .await
            .err()
            .expect("long"),
        p.derive_key(
            &KeyRef {
                id: "env".into(),
                version: 9,
            },
            label,
            32,
        )
        .await
        .err()
        .expect("version"),
    ] {
        let text = format!("{error} {error:?}");
        assert!(!text.contains("super-secret-label"), "{text}");
        assert!(!text.contains("SSSS"), "{text}");
    }
}
