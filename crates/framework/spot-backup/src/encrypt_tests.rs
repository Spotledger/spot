//! Tests for the streaming payload encryption (WP-PRD-04.T6).
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::Cursor;

use base64::Engine as _;
use spot_secrets::{KeyProvider, KeyRef as SecretsKeyRef, MemoryKeyring, WrappedKey};

use crate::encrypt::{
    CHUNK_SIZE, Header, MAX_HEADER_LEN, TAG_LEN, decrypt_stream, encrypt_stream, wrap_aad,
};
use crate::error::{BackupError, Result};
use crate::model::KeyRef;

const KEK_ID: &str = "backup-kek";
const SEALED_FULL: usize = CHUNK_SIZE + TAG_LEN;

fn keyring() -> MemoryKeyring {
    MemoryKeyring::new(KEK_ID).expect("keyring")
}

fn current(provider: &MemoryKeyring) -> KeyRef {
    let key = provider.current();
    KeyRef {
        id: key.id,
        version: key.version,
    }
}

fn payload(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| u8::try_from(index.wrapping_mul(31) % 251).expect("fits"))
        .collect()
}

async fn seal(provider: &MemoryKeyring, package_id: &str, plain: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encrypt_stream(
        provider,
        &current(provider),
        package_id,
        Cursor::new(plain.to_vec()),
        &mut out,
    )
    .await
    .expect("encrypt");
    out
}

async fn open(
    provider: &dyn KeyProvider,
    package_id: &str,
    sealed: &[u8],
) -> (Result<()>, Vec<u8>) {
    let mut out = Vec::new();
    let result = decrypt_stream(provider, package_id, Cursor::new(sealed.to_vec()), &mut out).await;
    (result, out)
}

fn header_len(sealed: &[u8]) -> usize {
    Header::parse(sealed).expect("header").1
}

#[tokio::test]
async fn round_trips_at_every_chunk_boundary() {
    let provider = keyring();
    for len in [0, 1, CHUNK_SIZE, CHUNK_SIZE + 1, 5 * 1024 * 1024] {
        let plain = payload(len);
        let mut sealed = Vec::new();
        let info = encrypt_stream(
            &provider,
            &current(&provider),
            "pkg-1",
            Cursor::new(plain.clone()),
            &mut sealed,
        )
        .await
        .expect("encrypt");
        let chunks = len.div_ceil(CHUNK_SIZE).max(1);
        assert_eq!(info.chunks, chunks as u64, "len {len}");
        assert_eq!(info.plaintext_bytes, len as u64);
        assert_eq!(info.encrypted_bytes, sealed.len() as u64);
        let head = header_len(&sealed);
        assert_eq!(sealed.len(), head + len + chunks * TAG_LEN, "len {len}");

        if len > 0 {
            assert_ne!(&sealed[head..head + len.min(64)], &plain[..len.min(64)]);
        }
        let again = seal(&provider, "pkg-1", &plain).await;
        assert_ne!(
            sealed, again,
            "a fresh key and prefix each time (len {len})"
        );

        let (result, out) = open(&provider, "pkg-1", &sealed).await;
        result.expect("decrypt");
        assert!(out == plain, "round trip of {len} bytes");
    }
}

#[tokio::test]
async fn tampering_fails_closed_and_writes_only_verified_chunks() {
    let provider = keyring();
    let plain = payload(2 * CHUNK_SIZE + 10);
    let sealed = seal(&provider, "pkg-2", &plain).await;
    let head = header_len(&sealed);

    // A flipped bit inside chunk 1: chunk 0 verified and was written, nothing more.
    let mut bad = sealed.clone();
    bad[head + SEALED_FULL + 5] ^= 1;
    let (result, out) = open(&provider, "pkg-2", &bad).await;
    assert_eq!(
        result.expect_err("flipped chunk").code(),
        "backup.decrypt_failed"
    );
    assert_eq!(out, plain[..CHUNK_SIZE]);

    // A flipped byte of the nonce prefix in the header (offset 14): no plaintext at all.
    let mut bad = sealed.clone();
    bad[14] ^= 1;
    let (result, out) = open(&provider, "pkg-2", &bad).await;
    assert_eq!(
        result.expect_err("flipped header").code(),
        "backup.decrypt_failed"
    );
    assert!(out.is_empty());

    // A flipped byte of the wrapped key: the provider cannot authenticate it.
    let mut bad = sealed.clone();
    bad[head - 1] ^= 1;
    let (result, out) = open(&provider, "pkg-2", &bad).await;
    assert_eq!(
        result.expect_err("flipped wrapped key").code(),
        "backup.decrypt_failed"
    );
    assert!(out.is_empty());

    // Cut exactly at a chunk boundary: the last chunk present lacks the final flag.
    let (result, out) = open(&provider, "pkg-2", &sealed[..head + 2 * SEALED_FULL]).await;
    assert_eq!(
        result.expect_err("boundary cut").code(),
        "backup.decrypt_failed"
    );
    assert_eq!(out, plain[..CHUNK_SIZE]);

    // Cut inside a chunk, and cut down to the header alone.
    let (result, _) = open(&provider, "pkg-2", &sealed[..sealed.len() - 3]).await;
    assert_eq!(result.expect_err("mid cut").code(), "backup.decrypt_failed");
    let (result, out) = open(&provider, "pkg-2", &sealed[..head]).await;
    assert_eq!(
        result.expect_err("no chunks").code(),
        "backup.decrypt_failed"
    );
    assert!(out.is_empty());

    // An extra chunk appended after the final one.
    let mut bad = sealed.clone();
    bad.extend_from_slice(&sealed[head..head + SEALED_FULL]);
    let (result, _) = open(&provider, "pkg-2", &bad).await;
    assert_eq!(
        result.expect_err("appended chunk").code(),
        "backup.decrypt_failed"
    );

    // Chunks 0 and 1 swapped.
    let mut bad = sealed.clone();
    bad[head..head + SEALED_FULL]
        .copy_from_slice(&sealed[head + SEALED_FULL..head + 2 * SEALED_FULL]);
    bad[head + SEALED_FULL..head + 2 * SEALED_FULL]
        .copy_from_slice(&sealed[head..head + SEALED_FULL]);
    let (result, out) = open(&provider, "pkg-2", &bad).await;
    assert_eq!(
        result.expect_err("swapped chunks").code(),
        "backup.decrypt_failed"
    );
    assert!(out.is_empty());

    // The same bytes under another package id.
    let (result, out) = open(&provider, "pkg-other", &sealed).await;
    assert_eq!(
        result.expect_err("other package id").code(),
        "backup.decrypt_failed"
    );
    assert!(out.is_empty());

    // The untouched stream still opens.
    let (result, out) = open(&provider, "pkg-2", &sealed).await;
    result.expect("untouched");
    assert!(out == plain);
}

#[tokio::test]
async fn a_header_naming_another_key_version_is_refused_not_replaced() {
    let provider = keyring();
    let sealed = seal(&provider, "pkg-3", b"payload").await;
    // The key version sits right after the key id: 8 + 2 + 4 + 16 + 2 + len(id).
    let at = 8 + 2 + 4 + 16 + 2 + KEK_ID.len();
    let mut bad = sealed.clone();
    bad[at..at + 4].copy_from_slice(&2u32.to_be_bytes());
    let (result, out) = open(&provider, "pkg-3", &bad).await;
    assert_eq!(
        result.expect_err("unknown version").code(),
        "backup.encryption_unavailable"
    );
    assert!(out.is_empty());
}

#[tokio::test]
async fn a_wrong_provider_fails() {
    let provider = keyring();
    let sealed = seal(&provider, "pkg-4", b"secret payload").await;

    // Same key id, different key material.
    let (result, out) = open(&keyring(), "pkg-4", &sealed).await;
    assert_eq!(
        result.expect_err("other kek").code(),
        "backup.decrypt_failed"
    );
    assert!(out.is_empty());

    // A provider that does not know the key at all.
    let stranger = MemoryKeyring::new("someone-else").expect("keyring");
    let (result, out) = open(&stranger, "pkg-4", &sealed).await;
    assert_eq!(
        result.expect_err("unknown kek").code(),
        "backup.encryption_unavailable"
    );
    assert!(out.is_empty());
}

#[tokio::test]
async fn a_retired_key_version_is_reported_and_never_replaced() {
    let provider = keyring();
    let old = current(&provider);
    let sealed = seal(&provider, "pkg-5", b"payload").await;
    provider.rotate().expect("rotate");
    provider.retire(1, false).expect("retire");

    let (result, out) = open(&provider, "pkg-5", &sealed).await;
    let error = result.expect_err("retired");
    assert_eq!(error.code(), "backup.encryption_unavailable");
    assert!(error.to_string().contains("retired"), "{error}");
    assert!(out.is_empty());

    // Asking to encrypt under the old version while the provider wraps under the new
    // one fails; it does not quietly use the other key.
    let mut sink = Vec::new();
    let error = encrypt_stream(
        &provider,
        &old,
        "pkg-6",
        Cursor::new(b"x".to_vec()),
        &mut sink,
    )
    .await
    .expect_err("version mismatch");
    assert_eq!(error.code(), "backup.encryption_unavailable");
    assert!(
        sink.is_empty(),
        "nothing is written before the key is settled"
    );
}

#[tokio::test]
async fn no_key_material_reaches_an_error_a_debug_or_the_manifest() {
    let provider = keyring();
    let key = current(&provider);
    let mut sealed = Vec::new();
    let info = encrypt_stream(
        &provider,
        &key,
        "pkg-7",
        Cursor::new(payload(100)),
        &mut sealed,
    )
    .await
    .expect("encrypt");
    let (header, _) = Header::parse(&sealed).expect("header");

    // Recover the data key the way a test can: through the provider it was wrapped by.
    let secrets_key = SecretsKeyRef {
        id: key.id.clone(),
        version: key.version,
    };
    let wrapped = WrappedKey {
        key: secrets_key.clone(),
        bytes: header.wrapped.clone(),
    };
    let data_key = provider
        .unwrap(&secrets_key, &wrapped, &wrap_aad("pkg-7"))
        .await
        .expect("unwrap");

    let mut needles: Vec<String> = Vec::new();
    for bytes in [data_key.expose(), &header.prefix[..], &header.wrapped[..]] {
        needles.push(bytes.iter().map(|byte| format!("{byte:02x}")).collect());
        needles.push(base64::engine::general_purpose::STANDARD.encode(bytes));
        needles.push(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes));
    }

    let mut shown: Vec<String> = vec![format!("{info:?}")];
    let mut bad = sealed.clone();
    let last = bad.len() - 1;
    bad[last] ^= 1;
    let truncated = sealed[..sealed.len() - 3].to_vec();
    for stream in [bad, truncated, sealed[..10].to_vec()] {
        let (result, _) = open(&provider, "pkg-7", &stream).await;
        let error = result.expect_err("must fail");
        shown.push(error.to_string());
        shown.push(format!("{error:?}"));
        shown.push(error.reason());
    }
    let (result, _) = open(&keyring(), "pkg-7", &sealed).await;
    let error = result.expect_err("wrong kek");
    shown.push(error.to_string());
    shown.push(format!("{error:?}"));
    let mut manifest = crate::testing::sample_manifest("pkg-7", "acme", 1);
    manifest.encryption = Some(key);
    shown.push(manifest.to_toml().expect("manifest"));

    for text in &shown {
        for needle in &needles {
            assert!(!text.contains(needle.as_str()), "{text} leaks key material");
        }
    }
}

#[tokio::test]
async fn a_hostile_header_is_refused_before_any_allocation() {
    let provider = keyring();
    let sealed = seal(&provider, "pkg-8", b"payload").await;

    // A chunk size of 4 GiB - 1 would be an allocation attack.
    let mut bad = sealed.clone();
    bad[10..14].copy_from_slice(&u32::MAX.to_be_bytes());
    let (result, out) = open(&provider, "pkg-8", &bad).await;
    assert_eq!(
        result.expect_err("chunk size").code(),
        "backup.encryption_header_invalid"
    );
    assert!(out.is_empty());

    // A key id length beyond its cap.
    let mut bad = sealed.clone();
    let at = 8 + 2 + 4 + 16;
    bad[at..at + 2].copy_from_slice(&60_000u16.to_be_bytes());
    let (result, _) = open(&provider, "pkg-8", &bad).await;
    assert_eq!(
        result.expect_err("key id length").code(),
        "backup.encryption_header_invalid"
    );

    // Wrong magic, unsupported version, empty input, a header cut short.
    let mut bad = sealed.clone();
    bad[0] ^= 0xff;
    let (result, _) = open(&provider, "pkg-8", &bad).await;
    assert_eq!(
        result.expect_err("magic").code(),
        "backup.encryption_header_invalid"
    );
    let mut bad = sealed.clone();
    bad[8..10].copy_from_slice(&9u16.to_be_bytes());
    let (result, _) = open(&provider, "pkg-8", &bad).await;
    assert_eq!(
        result.expect_err("version").code(),
        "backup.encryption_header_invalid"
    );
    let (result, _) = open(&provider, "pkg-8", &[]).await;
    assert_eq!(
        result.expect_err("empty").code(),
        "backup.encryption_header_invalid"
    );
    let (result, _) = open(&provider, "pkg-8", &sealed[..20]).await;
    assert_eq!(
        result.expect_err("cut header").code(),
        "backup.encryption_header_invalid"
    );

    const { assert!(MAX_HEADER_LEN < 2048, "the stated header cap stays small") };
}

#[tokio::test]
async fn an_empty_package_id_is_refused() {
    let provider = keyring();
    let mut sink = Vec::new();
    let error = encrypt_stream(
        &provider,
        &current(&provider),
        "",
        Cursor::new(Vec::new()),
        &mut sink,
    )
    .await
    .expect_err("empty id");
    assert_eq!(error.code(), "backup.encryption_unavailable");
    assert!(matches!(error, BackupError::EncryptionUnavailable { .. }));
}
