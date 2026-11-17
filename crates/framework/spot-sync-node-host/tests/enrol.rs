//! First-start enrolment: the code file is read once, redeemed once, wiped, and a node with no
//! code and no pin is not enrolled.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use spot_sync_http::BoxFuture;
use spot_sync_node_host::enrol::{
    Enroller, EnrolmentFile, Identity, IdentityPaths, identity_step, load_pin, read_code_file,
    wipe_code_file,
};
use spot_sync_node_host::error::{CODE_FILE_UNSAFE, NOT_ENROLLED};
use spot_sync_protocol::NodeId;

const NOW: i64 = 1_800_000_000;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("spot-node-host-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn node() -> NodeId {
    NodeId::new("edge-khi#branch_01").unwrap()
}

#[cfg(unix)]
fn private(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[cfg(not(unix))]
fn private(_path: &std::path::Path) {}

struct Counting {
    calls: AtomicUsize,
    refuse: bool,
    seen_code: std::sync::Mutex<Option<String>>,
}

impl Counting {
    fn new(refuse: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            refuse,
            seen_code: std::sync::Mutex::new(None),
        })
    }
}

impl Enroller for Counting {
    fn redeem(
        &self,
        _node: &NodeId,
        code: &str,
        identity: &spot_sync_enrol::NodeIdentity,
        _hub_fingerprint: &str,
    ) -> BoxFuture<spot_sync_node_host::Result<()>> {
        assert!(!identity.certificate_der().is_empty());
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.seen_code.lock().unwrap() = Some(code.to_owned());
        let refuse = self.refuse;
        Box::pin(async move {
            if refuse {
                Err(spot_sync_node_host::Error::new(
                    "sync_enrol.code_invalid",
                    "the enrolment code was not accepted",
                ))
            } else {
                Ok(())
            }
        })
    }
}

#[test]
fn the_file_forms_parse_and_never_show_the_code() {
    let file = EnrolmentFile::parse("code=abc123\nhub_fingerprint=AA11\n")
        .unwrap()
        .unwrap();
    assert_eq!(file.code(), "abc123");
    assert_eq!(file.hub_fingerprint(), Some("AA11"));
    assert!(!format!("{file:?}").contains("abc123"));
    let bare = EnrolmentFile::parse("abc123\n").unwrap().unwrap();
    assert_eq!(bare.code(), "abc123");
    assert!(EnrolmentFile::parse("  \n# comment\n").unwrap().is_none());
    assert!(EnrolmentFile::parse("code=a\ncode=b\n").is_err());
    assert!(EnrolmentFile::parse("code=a\nwho=me\n").is_err());
}

#[cfg(unix)]
#[test]
fn a_code_file_readable_by_others_is_refused_before_it_is_read() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("perm");
    let path = dir.join("code");
    fs::write(&path, "code=secret-value\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let error = read_code_file(&path).unwrap_err();
    assert_eq!(error.code, CODE_FILE_UNSAFE);
    assert!(!error.message.contains("secret-value"));
    private(&path);
    assert!(read_code_file(&path).unwrap().is_some());
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_symlink_is_not_a_code_file() {
    let dir = scratch("link");
    let target = dir.join("real");
    fs::write(&target, "code=x\n").unwrap();
    private(&target);
    let link = dir.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(read_code_file(&link).unwrap_err().code, CODE_FILE_UNSAFE);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_or_empty_file_is_no_code_and_wiping_nothing_is_fine() {
    let dir = scratch("none");
    let path = dir.join("code");
    assert!(read_code_file(&path).unwrap().is_none());
    wipe_code_file(&path).unwrap();
    fs::write(&path, "").unwrap();
    private(&path);
    assert!(read_code_file(&path).unwrap().is_none());
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_code_is_redeemed_once_the_pin_stored_and_the_file_wiped() {
    let dir = scratch("redeem");
    let code = dir.join("code");
    fs::write(&code, "code=one-time\nhub_fingerprint=ABCDEF\n").unwrap();
    private(&code);
    let paths = IdentityPaths {
        dir: dir.join("identity"),
        code_file: Some(code.clone()),
        configured_pin: None,
    };
    let enroller = Counting::new(false);
    let Identity::Enrolled {
        hub_fingerprint, ..
    } = identity_step(&paths, &node(), NOW, enroller.as_ref())
        .await
        .unwrap()
    else {
        panic!("expected enrolled");
    };
    assert_eq!(hub_fingerprint, "ABCDEF");
    assert_eq!(
        enroller.seen_code.lock().unwrap().as_deref(),
        Some("one-time")
    );
    assert!(!code.exists(), "the code file is gone");
    assert_eq!(load_pin(&paths.dir).as_deref(), Some("ABCDEF"));

    // A second start never needs the code again: no redemption, same pin.
    let again = identity_step(&paths, &node(), NOW, enroller.as_ref())
        .await
        .unwrap();
    assert!(matches!(again, Identity::Enrolled { .. }));
    assert_eq!(enroller.calls.load(Ordering::SeqCst), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_refused_redemption_leaves_the_file_and_stores_no_pin() {
    let dir = scratch("refused");
    let code = dir.join("code");
    fs::write(&code, "code=bad\nhub_fingerprint=ABCDEF\n").unwrap();
    private(&code);
    let paths = IdentityPaths {
        dir: dir.join("identity"),
        code_file: Some(code.clone()),
        configured_pin: None,
    };
    let enroller = Counting::new(true);
    let error = identity_step(&paths, &node(), NOW, enroller.as_ref())
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "sync_enrol.code_invalid");
    assert!(code.exists());
    assert_eq!(load_pin(&paths.dir), None);
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn no_code_and_no_pin_is_not_enrolled_and_a_configured_pin_alone_is_not_enough() {
    let dir = scratch("no-code");
    let paths = IdentityPaths {
        dir: dir.join("identity"),
        code_file: Some(dir.join("absent")),
        configured_pin: Some("ABCDEF".to_owned()),
    };
    let enroller = Counting::new(false);
    let outcome = identity_step(&paths, &node(), NOW, enroller.as_ref())
        .await
        .unwrap();
    assert!(matches!(outcome, Identity::NotEnrolled));
    assert_eq!(enroller.calls.load(Ordering::SeqCst), 0);
    // A code with no fingerprint anywhere cannot pin a hub.
    let code = dir.join("code");
    fs::write(&code, "only-a-code\n").unwrap();
    private(&code);
    let paths = IdentityPaths {
        configured_pin: None,
        code_file: Some(code),
        ..paths
    };
    let error = identity_step(&paths, &node(), NOW, enroller.as_ref())
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, NOT_ENROLLED);
    let _ = fs::remove_dir_all(&dir);
}
