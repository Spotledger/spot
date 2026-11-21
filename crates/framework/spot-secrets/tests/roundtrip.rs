#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]
use spot_secrets::{EnvKey, MemoryKeyring, Sealed, SecretBox, SecretString};

const AAD: &[u8] = b"site=acme;doctype=Email Account;field=password;name=Sales";

fn boxed() -> SecretBox<MemoryKeyring> {
    SecretBox::new(MemoryKeyring::new("test").expect("keyring"))
}

#[tokio::test]
async fn round_trips_empty_urdu_and_one_mib() {
    let sb = boxed();
    let big = "x".repeat(1 << 20);
    for text in ["", "پاس ورڈ ۱۲۳", "hunter2", big.as_str()] {
        let sealed = sb.seal(&SecretString::new(text), AAD).await.expect("seal");
        let opened = sb.open(&sealed, AAD).await.expect("open");
        assert_eq!(opened.expose(), text);
    }
}

#[tokio::test]
async fn sealing_twice_gives_different_ciphertext() {
    let sb = boxed();
    let s = SecretString::new("same");
    let a = sb.seal(&s, AAD).await.expect("seal");
    let b = sb.seal(&s, AAD).await.expect("seal");
    assert_ne!(a.to_bytes(), b.to_bytes());
}

#[tokio::test]
async fn text_form_round_trips() {
    let sb = boxed();
    let sealed = sb.seal(&SecretString::new("v"), AAD).await.expect("seal");
    let text = sealed.to_string();
    assert!(text.starts_with("spsec:v1:"));
    let parsed: Sealed = text.parse().expect("parse");
    assert_eq!(sb.open(&parsed, AAD).await.expect("open").expose(), "v");
}

#[tokio::test]
async fn env_key_works_and_rejects_bad_input() {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    let sb = SecretBox::new(EnvKey::from_base64(&b64).expect("env key"));
    let sealed = sb.seal(&SecretString::new("v"), AAD).await.expect("seal");
    assert_eq!(sb.open(&sealed, AAD).await.expect("open").expose(), "v");
    assert_eq!(sealed.key_ref().id, "env");

    for bad in ["not base64!!", "AAAA"] {
        let err = EnvKey::from_base64(bad).err().expect("must fail");
        assert_eq!(err.code(), "secrets.provider");
    }
}

#[tokio::test]
async fn provider_is_object_safe() {
    let provider: Box<dyn spot_secrets::KeyProvider> =
        Box::new(MemoryKeyring::new("t").expect("k"));
    let sb = SecretBox::new(provider);
    let sealed = sb.seal(&SecretString::new("v"), AAD).await.expect("seal");
    assert_eq!(sb.open(&sealed, AAD).await.expect("open").expose(), "v");
}
