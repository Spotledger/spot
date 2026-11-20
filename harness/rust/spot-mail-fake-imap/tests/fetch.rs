//! The fake IMAP server (WP-PLT-11.T15): a message pushed into it is fetched through the real
//! `spot-mail` client.

use std::time::Duration;

use spot_mail::{
    Credentials, FetchLimits, FetchOutcome, ImapConfig, ImapSecurity, Secret, fetch_new,
};
use spot_mail_fake_imap::{FAKE_PASSWORD, FAKE_USER, plain_message, spawn_starttls, spawn_tls};

fn config(port: u16, pem: &str, security: ImapSecurity) -> ImapConfig {
    ImapConfig {
        host: "127.0.0.1".to_owned(),
        port,
        security,
        credentials: Credentials {
            user: FAKE_USER.to_owned(),
            secret: Secret::new(FAKE_PASSWORD),
        },
        mailbox: "INBOX".to_owned(),
        timeout: Duration::from_secs(5),
        root_certificate_pem: Some(pem.as_bytes().to_vec()),
    }
}

#[tokio::test]
async fn a_pushed_message_is_fetched_with_its_text_and_uid() {
    let server = spawn_tls().await.unwrap_or_else(|e| panic!("{e}"));
    let uid = server
        .push_message(plain_message(
            "one@spot.test",
            "Rana <rana@spot.test>",
            "support@spot.test",
            "Where is my order",
            "Please help.",
            &[],
        ))
        .await;
    assert_eq!(uid, 1);
    let config = config(
        server.port(),
        server.host_certificate_pem(),
        ImapSecurity::Tls,
    );
    let batch = fetch_new(&config, None, FetchLimits::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(batch.outcome, FetchOutcome::New);
    assert_eq!(batch.messages.len(), 1);
    let message = &batch.messages[0];
    assert_eq!(message.uid, 1);
    assert_eq!(message.subject.as_deref(), Some("Where is my order"));
    assert_eq!(message.message_id.as_deref(), Some("one@spot.test"));
    assert!(
        message
            .text
            .as_deref()
            .is_some_and(|text| text.contains("Please help.")),
        "the body arrives"
    );
}

#[tokio::test]
async fn a_second_push_gets_the_next_uid_and_the_cursor_skips_the_first() {
    let server = spawn_starttls().await.unwrap_or_else(|e| panic!("{e}"));
    server
        .push_message(plain_message(
            "a@spot.test",
            "a@spot.test",
            "b@spot.test",
            "A",
            "a",
            &[],
        ))
        .await;
    let config = config(
        server.port(),
        server.host_certificate_pem(),
        ImapSecurity::StartTls,
    );
    let first = fetch_new(&config, None, FetchLimits::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first.messages.len(), 1);
    let second_uid = server
        .push_message(plain_message(
            "b@spot.test",
            "a@spot.test",
            "b@spot.test",
            "B",
            "b",
            &[],
        ))
        .await;
    assert_eq!(second_uid, 2);
    let second = fetch_new(&config, Some(first.cursor), FetchLimits::default())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(second.messages.len(), 1);
    assert_eq!(second.messages[0].subject.as_deref(), Some("B"));
    assert_eq!(server.message_count().await, 2);
    let recording = server.last_recording().await;
    assert!(
        recording.is_some_and(|r| r.logout),
        "the session ended through LOGOUT"
    );
}

#[test]
fn a_header_value_with_a_line_break_cannot_add_a_header() {
    let raw = plain_message(
        "x@spot.test",
        "a@spot.test",
        "b@spot.test",
        "S",
        "body",
        &[
            ("Auto-Submitted", "auto-replied\r\nBcc: x@evil.test"),
            ("X-Ok", "yes"),
        ],
    );
    let text = String::from_utf8_lossy(&raw);
    assert!(!text.contains("Bcc:"));
    assert!(!text.contains("Auto-Submitted"));
    assert!(text.contains("X-Ok: yes\r\n"));
}
