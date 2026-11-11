//! The committed sealed value must keep opening with the committed keyring: the format is
//! stable. Regenerate only on a deliberate format change (new magic/version):
//! `cargo test -p spot-secrets --test golden regenerate -- --ignored`.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

use spot_secrets::{LocalKeyring, Sealed, SecretBox, SecretString};
use std::path::PathBuf;

const AAD: &[u8] = b"site=acme;doctype=Email Account;field=password;name=Sales";
const PLAIN: &str = "golden-پاس-ورڈ-1";

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

#[tokio::test]
async fn committed_value_still_opens() {
    let ring = LocalKeyring::open(golden("keyring.json"))
        .await
        .expect("keyring");
    let sb = SecretBox::new(ring);
    let text = std::fs::read_to_string(golden("sealed-v1.txt")).expect("golden value");
    let sealed: Sealed = text.trim().parse().expect("parse");
    assert_eq!(sealed.key_ref().version, 1);
    assert_eq!(sb.open(&sealed, AAD).await.expect("open").expose(), PLAIN);
    // Re-encoding is byte-stable.
    assert_eq!(sealed.to_string(), text.trim());
}

#[tokio::test]
#[ignore = "writes the golden files"]
async fn regenerate() {
    let _ = std::fs::remove_file(golden("keyring.json"));
    let ring = LocalKeyring::create(golden("keyring.json"), "golden")
        .await
        .expect("create");
    let sealed = SecretBox::new(ring)
        .seal(&SecretString::new(PLAIN), AAD)
        .await
        .expect("seal");
    std::fs::write(golden("sealed-v1.txt"), format!("{sealed}\n")).expect("write");
}
