#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]
use spot_secrets::{LocalKeyring, SecretBox, SecretString};

const AAD: &[u8] = b"site=acme;field=password";

#[tokio::test]
async fn reloads_after_restart_and_survives_rotation() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("keys.json");

    let sb = SecretBox::new(
        LocalKeyring::create(&path, "site-acme")
            .await
            .expect("create"),
    );
    let v1 = sb
        .seal(&SecretString::new("canary"), AAD)
        .await
        .expect("seal");
    assert_eq!(sb.provider().rotate().await.expect("rotate"), 2);
    let v2 = sb.seal(&SecretString::new("two"), AAD).await.expect("seal");
    drop(sb);

    // "Restart": a new process loads the same file.
    let sb = SecretBox::new(LocalKeyring::open(&path).await.expect("open"));
    assert_eq!(sb.open(&v1, AAD).await.expect("v1").expose(), "canary");
    assert_eq!(sb.open(&v2, AAD).await.expect("v2").expose(), "two");
    assert!(sb.needs_rewrap(&v1));

    let moved = sb.rewrap(&v1, AAD).await.expect("rewrap");
    sb.provider().retire(1, false).await.expect("retire");
    drop(sb);

    // Retirement is persisted: the key material is gone from disk too.
    let sb = SecretBox::new(LocalKeyring::open(&path).await.expect("reopen"));
    assert_eq!(
        sb.open(&moved, AAD).await.expect("moved").expose(),
        "canary"
    );
    assert_eq!(
        sb.open(&v1, AAD).await.err().expect("retired").code(),
        "secrets.retired_key"
    );
}

#[tokio::test]
async fn create_refuses_to_overwrite_and_open_or_create_reuses() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("keys.json");
    let first = SecretBox::new(
        LocalKeyring::open_or_create(&path, "k")
            .await
            .expect("create"),
    );
    let sealed = first
        .seal(&SecretString::new("v"), AAD)
        .await
        .expect("seal");
    assert!(LocalKeyring::create(&path, "k").await.is_err());
    let second = SecretBox::new(
        LocalKeyring::open_or_create(&path, "k")
            .await
            .expect("open"),
    );
    assert_eq!(second.open(&sealed, AAD).await.expect("open").expose(), "v");
}

#[tokio::test]
async fn rejects_garbage_key_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("keys.json");
    for body in [
        "",
        "not json",
        r#"{"id":"k","current":1,"keys":[]}"#,
        r#"{"id":"k","current":1,"keys":[{"version":1,"key":"AAAA"}]}"#,
    ] {
        tokio::fs::write(&path, body).await.expect("write");
        let err = LocalKeyring::open(&path).await.err().expect("must fail");
        assert_eq!(err.code(), "secrets.provider");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn key_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("keys.json");
    let ring = LocalKeyring::create(&path, "k").await.expect("create");
    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    ring.rotate().await.expect("rotate");
    assert_eq!(mode(&path), 0o600, "rotation keeps the file private");
}
