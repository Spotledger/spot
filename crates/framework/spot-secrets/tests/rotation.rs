#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]
use spot_secrets::{
    KeyProvider, KeyRef, MemoryKeyring, ProviderFuture, SecretBox, SecretBytes, SecretString,
    WrappedKey,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const AAD: &[u8] = b"site=acme;doctype=Email Account;field=password;name=Sales";

/// Delegates to a shared keyring and counts calls.
struct Counting {
    inner: Arc<MemoryKeyring>,
    unwraps: AtomicUsize,
    wraps: AtomicUsize,
}

impl KeyProvider for Counting {
    fn current(&self) -> KeyRef {
        self.inner.current()
    }
    fn wrap<'a>(&'a self, d: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        self.wraps.fetch_add(1, Ordering::SeqCst);
        self.inner.wrap(d, aad)
    }
    fn unwrap<'a>(
        &'a self,
        k: &'a KeyRef,
        w: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        self.unwraps.fetch_add(1, Ordering::SeqCst);
        self.inner.unwrap(k, w, aad)
    }
}

#[tokio::test]
async fn rotate_rewrap_then_retire() {
    let ring = Arc::new(MemoryKeyring::new("test").expect("keyring"));
    let sb = SecretBox::new(Arc::clone(&ring));
    let old = sb
        .seal(&SecretString::new("canary"), AAD)
        .await
        .expect("seal");
    let kept = sb
        .seal(&SecretString::new("other"), AAD)
        .await
        .expect("seal");
    assert_eq!(old.key_ref().version, 1);
    assert!(!sb.needs_rewrap(&old));

    assert_eq!(ring.rotate().expect("rotate"), 2);
    assert!(sb.needs_rewrap(&old));
    // Both versions open while values are being rewrapped: no downtime.
    assert_eq!(
        sb.open(&old, AAD).await.expect("open v1").expose(),
        "canary"
    );
    let fresh = sb.seal(&SecretString::new("new"), AAD).await.expect("seal");
    assert_eq!(fresh.key_ref().version, 2);

    let rewrapped = sb.rewrap(&old, AAD).await.expect("rewrap");
    assert_eq!(rewrapped.key_ref().version, 2);
    assert!(!sb.needs_rewrap(&rewrapped));
    assert_eq!(
        sb.open(&rewrapped, AAD).await.expect("open v2").expose(),
        "canary"
    );

    // Retiring is refused while values still use it, and for the current version.
    assert_eq!(
        ring.retire(1, true).err().expect("refused").code(),
        "secrets.provider"
    );
    assert_eq!(
        ring.retire(2, false).err().expect("refused").code(),
        "secrets.provider"
    );
    ring.retire(1, false).expect("retire");

    assert_eq!(
        sb.open(&rewrapped, AAD)
            .await
            .expect("still opens")
            .expose(),
        "canary"
    );
    assert_eq!(
        sb.open(&kept, AAD).await.err().expect("retired").code(),
        "secrets.retired_key"
    );
}

#[tokio::test]
async fn unknown_version_is_unknown_key() {
    let a = SecretBox::new(MemoryKeyring::new("one").expect("k"));
    let b = SecretBox::new(MemoryKeyring::new("two").expect("k"));
    let sealed = a.seal(&SecretString::new("v"), AAD).await.expect("seal");
    assert_eq!(
        b.open(&sealed, AAD).await.err().expect("fails").code(),
        "secrets.unknown_key"
    );
}

#[tokio::test]
async fn rewrap_never_decrypts_the_value() {
    let ring = Arc::new(MemoryKeyring::new("test").expect("keyring"));
    let sb = SecretBox::new(Counting {
        inner: Arc::clone(&ring),
        unwraps: AtomicUsize::new(0),
        wraps: AtomicUsize::new(0),
    });
    let sealed = sb
        .seal(&SecretString::new("canary"), AAD)
        .await
        .expect("seal");
    ring.rotate().expect("rotate");

    // Corrupt the value's ciphertext. Opening must fail, but rewrap must still succeed
    // because it only touches the wrapped data key.
    let mut bytes = sealed.to_bytes();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    let corrupted = spot_secrets::Sealed::from_bytes(&bytes).expect("parse");
    assert_eq!(
        sb.open(&corrupted, AAD).await.err().expect("fails").code(),
        "secrets.tampered"
    );

    let before = sb.provider().unwraps.load(Ordering::SeqCst);
    let rewrapped = sb
        .rewrap(&corrupted, AAD)
        .await
        .expect("rewrap does not read the value");
    assert_eq!(sb.provider().unwraps.load(Ordering::SeqCst) - before, 1);
    assert_eq!(rewrapped.key_ref().version, 2);
    // Value bytes are carried over untouched.
    assert_eq!(
        &rewrapped.to_bytes()[rewrapped.to_bytes().len() - 40..],
        &bytes[bytes.len() - 40..]
    );
}

#[tokio::test]
async fn rewrap_with_wrong_aad_fails() {
    let sb = SecretBox::new(MemoryKeyring::new("test").expect("k"));
    let sealed = sb.seal(&SecretString::new("v"), AAD).await.expect("seal");
    assert_eq!(
        sb.rewrap(&sealed, b"other")
            .await
            .err()
            .expect("fails")
            .code(),
        "secrets.tampered"
    );
}
