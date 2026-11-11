#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]
use spot_secrets::{
    EnvKey, MemoryKeyring, Sealed, SecretBox, SecretBytes, SecretError, SecretString,
};

const CANARY: &str = "CANARY-hunter2-پاس";
const AAD: &[u8] = b"site=acme;field=password";

#[test]
fn secret_types_print_redacted() {
    let s = SecretString::new(CANARY);
    let b = SecretBytes::new(CANARY.as_bytes().to_vec());
    for text in [
        format!("{s:?}"),
        format!("{s}"),
        format!("{b:?}"),
        format!("{b}"),
        format!("{s:#?}"),
    ] {
        assert_eq!(text, "[redacted]");
    }
    assert_eq!(s.expose(), CANARY);
    assert_eq!(b.expose(), CANARY.as_bytes());
}

#[tokio::test]
async fn errors_and_sealed_never_contain_the_plaintext() {
    let sb = SecretBox::new(MemoryKeyring::new("test").expect("keyring"));
    let sealed = sb
        .seal(&SecretString::new(CANARY), AAD)
        .await
        .expect("seal");
    let mut texts = vec![format!("{sealed:?}"), format!("{:?}", sealed.key_ref())];

    let mut errors: Vec<SecretError> = vec![
        sb.open(&sealed, b"wrong").await.err().expect("tampered"),
        Sealed::from_bytes(b"junk").err().expect("malformed"),
        EnvKey::from_base64(CANARY).err().expect("provider"),
        SecretError::UnknownKey {
            id: "k".into(),
            version: 3,
        },
        SecretError::RetiredKey {
            id: "k".into(),
            version: 3,
        },
    ];
    let other = SecretBox::new(MemoryKeyring::new("other").expect("keyring"));
    errors.push(other.open(&sealed, AAD).await.err().expect("unknown"));
    for e in &errors {
        texts.push(format!("{e}"));
        texts.push(format!("{e:?}"));
    }
    let wrapped_hex: Vec<String> = vec![
        sealed
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    ];
    for t in texts.iter().chain(&wrapped_hex) {
        assert!(!t.contains(CANARY), "leaked plaintext in {t}");
    }
    // The wrapped key bytes do not appear in Debug output either.
    let wrapped_dbg = format!("{sealed:?}");
    assert!(!wrapped_dbg.contains(&format!("{:?}", &sealed.to_bytes()[10..14])));
    // Stable codes.
    let codes: Vec<_> = errors.iter().map(SecretError::code).collect();
    assert_eq!(
        codes,
        [
            "secrets.tampered",
            "secrets.malformed",
            "secrets.provider",
            "secrets.unknown_key",
            "secrets.retired_key",
            "secrets.unknown_key"
        ]
    );
}
