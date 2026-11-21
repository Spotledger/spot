//! The own-mail marker: a keyed MAC over the Message-ID.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_mail_intake::OwnMailKey;

fn key(secret: &[u8]) -> OwnMailKey {
    OwnMailKey::derive(secret).unwrap()
}

#[test]
fn a_marker_verifies_under_its_key_and_only_for_its_message_id() {
    let k = key(b"mail secret one");
    let marker = k.marker("<a@x.test>").unwrap();
    assert!(marker.starts_with("v1."));
    assert_eq!(marker.len(), 3 + 64);
    assert!(k.verifies("<a@x.test>", &marker));
    assert!(
        k.verifies("A@X.TEST", &marker),
        "case and brackets do not matter"
    );
    assert!(!k.verifies("<b@x.test>", &marker));
}

#[test]
fn another_secret_cannot_make_or_verify_the_marker() {
    let ours = key(b"mail secret one");
    let theirs = key(b"mail secret two");
    let marker = ours.marker("<a@x.test>").unwrap();
    assert!(!theirs.verifies("<a@x.test>", &marker));
    assert_ne!(marker, theirs.marker("<a@x.test>").unwrap());
}

#[test]
fn malformed_values_and_empty_inputs_never_verify() {
    let k = key(b"mail secret one");
    let marker = k.marker("<a@x.test>").unwrap();
    assert!(!k.verifies("<a@x.test>", ""));
    assert!(!k.verifies("<a@x.test>", "v1."));
    assert!(!k.verifies("<a@x.test>", &marker[3..]), "no version prefix");
    assert!(!k.verifies("<a@x.test>", &format!("v2.{}", &marker[3..])));
    assert!(!k.verifies("<a@x.test>", &marker[..marker.len() - 1]));
    assert!(!k.verifies("<a@x.test>", &format!("{marker}00")));
    assert!(!k.verifies("", &marker));
    assert!(k.marker("").is_none());
    assert!(k.marker("<>").is_none());
}

#[test]
fn an_empty_secret_makes_no_key_and_the_key_never_prints() {
    assert!(OwnMailKey::derive(b"").is_none());
    assert_eq!(format!("{:?}", key(b"s")), "OwnMailKey(..)");
}
