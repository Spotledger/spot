//! The wrapped derivation seed file: created once, mode `0600`, never plaintext, and a corrupt,
//! foreign or open file refuses to start.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use support::*;

#[tokio::test]
async fn the_first_start_writes_the_seed_wrapped_with_mode_0600() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let _provider = open(&fake, dir.path(), &clock()).await;
    let path = seed_path(dir.path());
    let text = std::fs::read_to_string(&path).expect("seed file");
    let json: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(json["format"], "spot-kms-seed-v1");
    assert_eq!(json["key_id"], "kms:spotkey");
    assert_eq!(json["key_version"], 1);
    assert!(json["ciphertext"].as_str().expect("ciphertext").len() > 8);
    // FixedEntropy(7) draws the bytes 7, 8, 9, ...: the file holds neither raw nor base64 form.
    let seed: Vec<u8> = (0..32u8).map(|i| 7u8.wrapping_add(i)).collect();
    assert!(!text.contains(&STANDARD.encode(&seed)));
    assert!(
        !std::fs::read(&path)
            .expect("bytes")
            .windows(seed.len())
            .any(|w| w == seed.as_slice())
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // No temp file is left behind.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .expect("dir")
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[tokio::test]
async fn a_restart_does_not_write_the_seed_again() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let _first = open(&fake, dir.path(), &clock()).await;
    let before = std::fs::read(seed_path(dir.path())).expect("seed");
    let encrypts = fake.encrypts();
    let _second = open_with(&fake, dir.path(), &clock(), 50, |_| {})
        .await
        .expect("restart");
    assert_eq!(std::fs::read(seed_path(dir.path())).expect("seed"), before);
    assert_eq!(fake.encrypts(), encrypts, "a restart only unwraps");
}

async fn refused(dir: &std::path::Path, fake: &std::sync::Arc<FakeTransit>) -> String {
    open_with(fake, dir, &clock(), 7, |_| {})
        .await
        .err()
        .expect("must refuse to open")
        .to_string()
}

#[tokio::test]
async fn a_corrupt_seed_file_refuses_to_open() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let _ = open(&fake, dir.path(), &clock()).await;
    let path = seed_path(dir.path());
    for garbage in [
        &b"not json at all"[..],
        &b"{}"[..],
        &b""[..],
        &br#"{"format":"other","key_id":"kms:spotkey","key_version":1,"ciphertext":"AAAA"}"#[..],
        &br#"{"format":"spot-kms-seed-v1","key_id":"kms:spotkey","key_version":0,"ciphertext":"AAAA"}"#[..],
        &br#"{"format":"spot-kms-seed-v1","key_id":"kms:spotkey","key_version":1,"ciphertext":"%%%"}"#[..],
    ] {
        std::fs::write(&path, garbage).expect("write");
        let message = refused(dir.path(), &fake).await;
        assert!(message.contains("secrets_kms.seed_invalid"), "{message}");
    }
}

#[tokio::test]
async fn a_seed_file_of_another_key_is_refused() {
    let (dir_a, dir_b) = (
        tempfile::tempdir().expect("dir"),
        tempfile::tempdir().expect("dir"),
    );
    let (fake_a, fake_b) = (FakeTransit::named("alpha"), FakeTransit::named("beta"));
    let _ = open_with(&fake_a, dir_a.path(), &clock(), 7, |_| {})
        .await
        .expect("alpha");
    let _ = open_with(&fake_b, dir_b.path(), &clock(), 7, |_| {})
        .await
        .expect("beta");
    // Alpha's envelope placed in beta's directory.
    std::fs::copy(seed_path(dir_a.path()), seed_path(dir_b.path())).expect("copy");
    set_private(&seed_path(dir_b.path()));
    let message = refused(dir_b.path(), &fake_b).await;
    assert!(message.contains("secrets_kms.seed_invalid"), "{message}");
    assert!(message.contains("another key"), "{message}");
}

#[tokio::test]
async fn an_envelope_with_an_altered_ciphertext_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let _ = open(&fake, dir.path(), &clock()).await;
    let path = seed_path(dir.path());
    let mut json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("seed")).expect("json");
    let mut raw = STANDARD
        .decode(json["ciphertext"].as_str().expect("ciphertext"))
        .expect("b64");
    // The wire form is `vault:v1:<b64>`; flip the last byte of the base64 text.
    let last = raw.len() - 1;
    raw[last] = if raw[last] == b'A' { b'B' } else { b'A' };
    json["ciphertext"] = serde_json::Value::String(STANDARD.encode(raw));
    std::fs::write(&path, serde_json::to_vec(&json).expect("json")).expect("write");
    set_private(&path);
    let message = refused(dir.path(), &fake).await;
    assert!(message.contains("secrets_kms."), "{message}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_seed_file_open_to_group_or_other_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let _ = open(&fake, dir.path(), &clock()).await;
    let path = seed_path(dir.path());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let message = refused(dir.path(), &fake).await;
    assert!(message.contains("secrets_kms.seed_invalid"), "{message}");
    assert!(message.contains("mode"), "{message}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_seed_file_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let _ = open(&fake, dir.path(), &clock()).await;
    let path = seed_path(dir.path());
    let real = dir.path().join("elsewhere.json");
    std::fs::rename(&path, &real).expect("move");
    std::os::unix::fs::symlink(&real, &path).expect("link");
    let message = refused(dir.path(), &fake).await;
    assert!(message.contains("secrets_kms.seed_io"), "{message}");
}

#[tokio::test]
async fn a_leftover_temp_file_stops_a_first_write() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    std::fs::write(dir.path().join("kms-seed.json.tmp"), b"stale").expect("tmp");
    let message = refused(dir.path(), &fake).await;
    assert!(message.contains("secrets_kms.seed_io"), "{message}");
    assert!(!seed_path(dir.path()).exists());
}

#[tokio::test]
async fn rewrap_seed_moves_the_envelope_to_the_latest_version() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let clock = clock();
    let provider = open(&fake, dir.path(), &clock).await;
    let before = {
        use spot_secrets::KeyProvider;
        provider
            .derive_key(&provider.current(), b"purpose", 32)
            .await
            .expect("derive")
            .expose()
            .to_vec()
    };
    fake.rotate();
    assert_eq!(provider.rewrap_seed().await.expect("rewrap seed"), 2);
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(seed_path(dir.path())).expect("seed")).expect("json");
    assert_eq!(json["key_version"], 2);
    // A restart unwraps the new envelope and derives the same keys.
    fake.set_min_decryption(2);
    let again = open_with(&fake, dir.path(), &clock, 99, |_| {})
        .await
        .expect("restart after retiring v1");
    use spot_secrets::KeyProvider;
    let after = again
        .derive_key(&again.current(), b"purpose", 32)
        .await
        .expect("derive");
    assert_eq!(after.expose(), before.as_slice());
}

#[cfg(unix)]
fn set_private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

#[cfg(not(unix))]
fn set_private(_path: &std::path::Path) {}
