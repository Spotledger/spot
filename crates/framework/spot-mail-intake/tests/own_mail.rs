//! Mail intake recognises its own outgoing mail (WP-PLT-11.T16): a valid `X-Spot-Outgoing`
//! marker is dropped, counted and audited without content; a forged marker is handled as
//! ordinary mail; an auto-submitted message is dropped.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_mail::SelectedHeader;
use spot_mail_intake::memory::StaticCredentials;
use spot_mail_intake::{AuditEvent, OwnMailKey, PollState, poll_once};
use support::{Harness, PASSWORD, account, dropped, message};

fn key() -> OwnMailKey {
    OwnMailKey::derive(b"site mail secret, long enough to be a key").unwrap()
}

fn marked(uid: u32, id: &str, value: &str) -> spot_mail::IncomingMessage {
    let mut message = message(uid, id);
    message.headers.push(SelectedHeader {
        name: "X-Spot-Outgoing",
        value: value.to_owned(),
    });
    message
}

async fn harness() -> Harness {
    let mut h = Harness::new().await;
    h.creds = StaticCredentials::new()
        .with("mail/support", PASSWORD)
        .with_own_mail_key(key());
    h
}

async fn poll(h: &Harness) -> spot_mail_intake::PollReport {
    let mut state = PollState::new();
    poll_once(&account(), h.now(), &mut state, &h.deps("node-a")).await
}

#[tokio::test]
async fn a_message_with_a_valid_own_marker_is_dropped_counted_and_audited() {
    let h = harness().await;
    let id = "<own-1@spot.test>";
    let marker = key().marker(id).unwrap();
    h.mailbox.push(marked(1, id, &marker));
    h.mailbox.push(message(2, "<real@x>"));
    let report = poll(&h).await;
    assert_eq!(report.accepted, 1);
    assert_eq!(dropped(&report).get("own_mail"), Some(&1));
    assert_eq!(h.sink.calls(), vec![2], "own mail never reaches the sink");
    let entries = h.audit.entries();
    assert_eq!(
        entries,
        vec![(
            "support".to_owned(),
            AuditEvent::OwnMail {
                count: 1,
                first_uid: 1
            }
        )]
    );
}

#[tokio::test]
async fn a_forged_marker_is_processed_as_normal_mail() {
    let h = harness().await;
    let id = "<forged@x>";
    // A well-formed marker made under another key, and a marker for another Message-ID.
    let other = OwnMailKey::derive(b"an outsider's guess at the secret").unwrap();
    h.mailbox.push(marked(1, id, &other.marker(id).unwrap()));
    h.mailbox.push(marked(
        2,
        "<b@x>",
        &key().marker("<a-different-id@x>").unwrap(),
    ));
    h.mailbox.push(marked(3, "<c@x>", "v1.not-hex"));
    h.mailbox.push(marked(4, "<d@x>", "garbage"));
    let report = poll(&h).await;
    assert_eq!(report.accepted, 4, "none of the forgeries is dropped");
    assert!(!dropped(&report).contains_key("own_mail"));
    assert!(h.audit.entries().is_empty());
}

#[tokio::test]
async fn without_a_key_the_marker_cannot_drop_anything() {
    let h = Harness::new().await;
    let id = "<own-2@spot.test>";
    h.mailbox.push(marked(1, id, &key().marker(id).unwrap()));
    let report = poll(&h).await;
    assert_eq!(report.accepted, 1);
}

#[tokio::test]
async fn the_marker_ignores_case_and_angle_brackets_of_the_message_id() {
    let h = harness().await;
    let marker = key().marker("<Own-3@Spot.Test>").unwrap();
    h.mailbox.push(marked(1, "own-3@spot.test", &marker));
    let report = poll(&h).await;
    assert_eq!(dropped(&report).get("own_mail"), Some(&1));
}

#[tokio::test]
async fn an_auto_submitted_message_is_dropped_and_counted() {
    let h = harness().await;
    let mut auto = message(1, "<auto@x>");
    auto.auto_submitted = true;
    h.mailbox.push(auto);
    let report = poll(&h).await;
    assert_eq!(dropped(&report).get("auto_reply"), Some(&1));
    assert_eq!(report.accepted, 0);
}

#[tokio::test]
async fn nothing_in_the_report_or_the_audit_holds_message_content() {
    let h = harness().await;
    let id = "<own-4@spot.test>";
    h.mailbox.push(marked(1, id, &key().marker(id).unwrap()));
    let report = poll(&h).await;
    let text = format!("{report:?} {:?}", h.audit.entries());
    for secret in [
        "Secret subject line",
        "Secret body text",
        "customer@elsewhere.test",
        "own-4@spot.test",
        PASSWORD,
    ] {
        assert!(!text.contains(secret), "`{secret}` leaked into {text}");
    }
}
