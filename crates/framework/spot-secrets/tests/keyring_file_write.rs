#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]
use spot_secrets::{LocalKeyring, SecretBox, SecretString};
use std::path::Path;

const AAD: &[u8] = b"site=acme;field=password";

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).expect("meta").permissions().mode() & 0o777
}

fn tmp_of(path: &Path) -> std::path::PathBuf {
    let mut t = path.as_os_str().to_owned();
    t.push(".tmp");
    t.into()
}

#[tokio::test]
async fn leftover_temp_file_is_refused_not_overwritten() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("keys.json");
    let ring = LocalKeyring::create(&path, "k").await.expect("create");
    let before = std::fs::read(&path).expect("read");

    let tmp = tmp_of(&path);
    std::fs::write(&tmp, b"OLD").expect("plant");
    let err = ring.rotate().await.err().expect("refused");
    assert_eq!(err.code(), "secrets.provider");
    // The message names the path and says nothing about content.
    let msg = err.to_string();
    assert!(msg.contains("key file temp exists"));
    assert!(!msg.contains("OLD"));
    // Neither file was touched, and the in-memory ring did not advance.
    assert_eq!(std::fs::read(&tmp).expect("tmp"), b"OLD");
    assert_eq!(std::fs::read(&path).expect("key"), before);

    // After the operator removes it, the rotation succeeds and leaves no temp file.
    std::fs::remove_file(&tmp).expect("rm");
    assert_eq!(ring.rotate().await.expect("rotate"), 2);
    assert!(!tmp.exists());
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600);
}

#[cfg(unix)]
#[tokio::test]
async fn planted_symlink_is_never_written_through() {
    let dir = tempfile::tempdir().expect("tempdir");
    let other = tempfile::tempdir().expect("other");
    let path = dir.path().join("keys.json");
    let ring = LocalKeyring::create(&path, "k").await.expect("create");

    let target = other.path().join("victim");
    std::fs::write(&target, b"VICTIM").expect("victim");
    std::os::unix::fs::symlink(&target, tmp_of(&path)).expect("link");

    assert!(ring.rotate().await.is_err());
    assert_eq!(std::fs::read(&target).expect("victim"), b"VICTIM");
    // The key file is still the valid version 1 file.
    let reopened = LocalKeyring::open(&path).await.expect("open");
    let sb = SecretBox::new(reopened);
    let sealed = sb.seal(&SecretString::new("v"), AAD).await.expect("seal");
    assert_eq!(sb.open(&sealed, AAD).await.expect("open").expose(), "v");
}

#[cfg(unix)]
#[tokio::test]
async fn relative_file_name_without_parent_component_works() {
    // Only this test changes the working directory in this binary.
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_current_dir(dir.path()).expect("cd");
    let ring = LocalKeyring::create("rel-keys.json", "k")
        .await
        .expect("create");
    assert_eq!(ring.rotate().await.expect("rotate"), 2);
    assert_eq!(mode(Path::new("rel-keys.json")), 0o600);
    assert!(!Path::new("rel-keys.json.tmp").exists());
    let reopened = LocalKeyring::open("rel-keys.json").await.expect("open");
    drop(reopened);
}

#[cfg(unix)]
#[tokio::test]
async fn read_only_directory_errors_and_keeps_the_old_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("keys.json");
    let ring = LocalKeyring::create(&path, "k").await.expect("create");
    let before = std::fs::read(&path).expect("read");

    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).expect("ro");
    // Root ignores directory modes: skip then.
    let writable = std::fs::write(dir.path().join("probe"), b"x").is_ok();
    let result = if writable {
        None
    } else {
        Some(ring.rotate().await)
    };
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("rw");
    if let Some(result) = result {
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).expect("key"), before);
    }
}
