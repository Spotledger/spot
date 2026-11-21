#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]
use proptest::prelude::*;
use spot_secrets::{MemoryKeyring, Sealed, SecretBox, SecretString};

const AAD: &[u8] = b"site=acme;doctype=Email Account;field=password;name=Sales";
const REJECTED: [&str; 4] = [
    "secrets.tampered",
    "secrets.malformed",
    "secrets.unknown_key",
    "secrets.retired_key",
];

async fn sealed_value() -> (SecretBox<MemoryKeyring>, Vec<u8>) {
    let sb = SecretBox::new(MemoryKeyring::new("test").expect("keyring"));
    let sealed = sb
        .seal(&SecretString::new("canary-value"), AAD)
        .await
        .expect("seal");
    (sb, sealed.to_bytes())
}

async fn open_bytes(sb: &SecretBox<MemoryKeyring>, bytes: &[u8]) -> Result<SecretString, String> {
    let sealed = Sealed::from_bytes(bytes).map_err(|e| e.code().to_owned())?;
    sb.open(&sealed, AAD).await.map_err(|e| e.code().to_owned())
}

#[tokio::test]
async fn wrong_aad_is_tampered() {
    let (sb, bytes) = sealed_value().await;
    let sealed = Sealed::from_bytes(&bytes).expect("parse");
    let other = b"site=acme;doctype=Email Account;field=username;name=Sales";
    let err = sb.open(&sealed, other).await.err().expect("must fail");
    assert_eq!(err.code(), "secrets.tampered");
}

#[tokio::test]
async fn every_flipped_byte_is_rejected() {
    let (sb, bytes) = sealed_value().await;
    for i in 0..bytes.len() {
        let mut copy = bytes.clone();
        copy[i] ^= 0x01;
        let code = open_bytes(&sb, &copy)
            .await
            .err()
            .unwrap_or_else(|| panic!("byte {i} accepted"));
        assert!(REJECTED.contains(&code.as_str()), "byte {i}: {code}");
    }
}

#[tokio::test]
async fn every_truncation_is_rejected() {
    let (sb, bytes) = sealed_value().await;
    for len in 0..bytes.len() {
        let code = open_bytes(&sb, &bytes[..len])
            .await
            .err()
            .unwrap_or_else(|| panic!("len {len} accepted"));
        assert!(REJECTED.contains(&code.as_str()), "len {len}: {code}");
    }
}

#[tokio::test]
async fn text_form_garbage_is_malformed() {
    for text in [
        "",
        "spsec:v1:",
        "spsec:v1:!!!",
        "other:v1:AAAA",
        "spsec:v1:AAAA",
    ] {
        let err = text.parse::<Sealed>().err().expect("must fail");
        assert_eq!(err.code(), "secrets.malformed", "{text}");
    }
}

proptest! {
    #[test]
    fn random_byte_flips_never_open_or_panic(pos in 0usize..4096, mask in 1u8..=255) {
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("rt");
        rt.block_on(async {
            let (sb, mut bytes) = sealed_value().await;
            let i = pos % bytes.len();
            bytes[i] ^= mask;
            let code = open_bytes(&sb, &bytes).await.err().expect("accepted");
            prop_assert!(REJECTED.contains(&code.as_str()));
            Ok(())
        })?;
    }

    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
        let _ = Sealed::from_bytes(&bytes);
    }
}
