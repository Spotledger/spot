//! Integration tests for the IMAP ingestion transport (WP-PLT-11.T2): a fake IMAP4rev1 server
//! (tests/support/imap_server.rs) answers scripted sessions — implicit TLS, STARTTLS, LOGIN,
//! SELECT, UID SEARCH, UID FETCH — and the fixtures in it are parsed end to end into
//! `IncomingMessage`s. Covering: cursor advance and batching, UIDVALIDITY reset, the oversize
//! header-stub path, auto-submitted detection, thread keys, RFC 2231 names, quoted-printable
//! bodies, inline attachment ids, the transient/permanent error classes for broken servers, and
//! the guarantee that polling never marks messages seen or touches the mailbox.

mod support;

use std::time::Duration;

use lettre::message::header::ContentType;
use spot_mail::error::{MailError, PermanentFailure, TransientFailure};
use spot_mail::imap::{
    FetchLimits, FetchOutcome, ImapConfig, ImapSecurity, MailAddress, MailboxCursor, fetch_new,
    thread_key,
};
use spot_mail::message::Secret;
use spot_mail::smtp::Credentials;
use support::imap_server::{FixtureMessage, ImapFakeServer, ImapScript, spawn_starttls, spawn_tls};
use support::{Must, MustErr};

/// The date header every fixture carries, and its unix rendering (`fetch_new` normalizes it).
const DATE: &str = "Fri, 11 Jul 2025 09:30:00 +0000";
const DATE_UNIX: i64 = 1_752_226_200;

/// The exact Urdu sentence the quoted-printable fixture must round-trip to.
const URDU: &str = "خبر پڑھیں۔ آج کا کالم بہت اچھا تھا۔";

/// An ingest config pointed at the fake server, trusting its self-signed certificate.
fn config(server: &ImapFakeServer, security: ImapSecurity) -> ImapConfig {
    ImapConfig {
        host: "127.0.0.1".to_owned(),
        port: server.port(),
        security,
        credentials: Credentials {
            user: support::FAKE_USER.to_owned(),
            secret: Secret::new(support::FAKE_PASSWORD),
        },
        mailbox: "INBOX".to_owned(),
        timeout: Duration::from_secs(5),
        root_certificate_pem: Some(server.host_certificate_pem().must().as_bytes().to_vec()),
    }
}

/// A script serving `raw` as UIDs `1..n` under the default UIDVALIDITY.
fn script(raw: &[Vec<u8>]) -> ImapScript {
    ImapScript {
        messages: raw
            .iter()
            .enumerate()
            .map(|(index, raw)| FixtureMessage {
                uid: index as u32 + 1,
                raw: raw.clone(),
            })
            .collect(),
        ..ImapScript::default()
    }
}

/// A content type for `Attachment` assertions, parsed the same way the builder does it.
fn content_type(value: &str) -> ContentType {
    ContentType::parse(value).must()
}

/// A minimal quoted-printable encoder: enough for the Urdu fixture (it does not need soft
/// line breaks, and its last character is not a space).
fn quoted_printable(text: &str) -> String {
    text.bytes()
        .map(|byte| match byte {
            b'=' => "=3D".to_owned(),
            byte if byte.is_ascii_graphic() || byte == b' ' => (byte as char).to_string(),
            byte => format!("={byte:02X}"),
        })
        .collect()
}

/// A fixture from header pairs and a body that already carries its MIME boundaries and CRLFs.
fn message(headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut out = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    out.push_str("\r\n");
    out.push_str(body);
    out.into_bytes()
}

/// The text/html fixture: alternatives make the plain-text and HTML extraction assertions easy.
fn fixture_alternative() -> Vec<u8> {
    message(
        &[
            ("From", "Support <support@spot.test>"),
            ("To", "Rana Kirti <Rana@Spot.test>"),
            ("Subject", "Weekly report is ready"),
            ("Date", DATE),
            ("Message-ID", "<alternative-1@spot.test>"),
            ("MIME-Version", "1.0"),
            ("Content-Type", "multipart/alternative; boundary=\"alt-1\""),
        ],
        concat!(
            "--alt-1\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "Hello in plain text.\r\n",
            "--alt-1\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<p>Hello in <b>HTML</b>.</p>\r\n",
            "--alt-1--\r\n",
        ),
    )
}

/// The multipart/related fixture: an HTML body referencing an inline PNG by its `Content-ID`.
fn fixture_inline_image() -> Vec<u8> {
    message(
        &[
            ("From", "Marketing <marketing@spot.test>"),
            ("To", "Rana Kirti <Rana@Spot.test>"),
            ("Subject", "Brand kit attached"),
            ("Date", DATE),
            ("Message-ID", "<inline-2@spot.test>"),
            ("MIME-Version", "1.0"),
            ("Content-Type", "multipart/related; boundary=\"rel-2\""),
        ],
        concat!(
            "--rel-2\r\n",
            "Content-Type: text/html; charset=utf-8\r\n",
            "\r\n",
            "<p>See the <img src=\"cid:logo-2@spot.test\" alt=\"logo\"/></p>\r\n",
            "--rel-2\r\n",
            "Content-Type: image/png\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: inline; filename=\"logo.png\"\r\n",
            "Content-ID: <logo-2@spot.test>\r\n",
            "\r\n",
            "iVBORw==\r\n",
            "--rel-2--\r\n",
        ),
    )
}

/// The RFC 2231 fixture: an attached PDF whose file name carries non-ASCII percent-encoded
/// words (`naïve résumé.pdf`), the spelling some mailers still emit.
fn fixture_rfc2231() -> Vec<u8> {
    message(
        &[
            ("From", "Reports <reports@spot.test>"),
            ("To", "Rana Kirti <Rana@Spot.test>"),
            ("Subject", "Monthly report"),
            ("Date", DATE),
            ("Message-ID", "<rfc2231-3@spot.test>"),
            ("MIME-Version", "1.0"),
            ("Content-Type", "multipart/mixed; boundary=\"mix-3\""),
        ],
        concat!(
            "--mix-3\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n",
            "\r\n",
            "The monthly report is attached.\r\n",
            "--mix-3\r\n",
            "Content-Type: application/pdf\r\n",
            "Content-Transfer-Encoding: base64\r\n",
            "Content-Disposition: attachment; filename*=UTF-8''na%C3%AFve%20r%C3%A9sum%C3%A9.pdf\r\n",
            "\r\n",
            "JVBERi0xLjQKJVBERi0xLjQK\r\n",
            "--mix-3--\r\n",
        ),
    )
}

/// The quoted-printable fixture: an Urdu sentence, encoded over the SMTP wire.
fn fixture_urdu() -> Vec<u8> {
    message(
        &[
            ("From", "News Desk <news@spot.test>"),
            ("To", "Rana Kirti <Rana@Spot.test>"),
            ("Subject", "Headlines"),
            ("Date", DATE),
            ("Message-ID", "<urdu-4@spot.test>"),
            ("MIME-Version", "1.0"),
            ("Content-Type", "text/plain; charset=utf-8"),
            ("Content-Transfer-Encoding", "quoted-printable"),
        ],
        &quoted_printable(URDU),
    )
}

/// The auto-reply fixture: announces automated origination and replies to a message whose
/// thread has no `References` chain — `thread_key` must fall back to `In-Reply-To`.
fn fixture_auto_reply() -> Vec<u8> {
    message(
        &[
            ("From", "Mailer Daemon <daemon@spot.test>"),
            ("To", "Rana Kirti <Rana@Spot.test>"),
            ("Subject", "Out of office"),
            ("Date", DATE),
            ("Message-ID", "<auto-5@spot.test>"),
            ("In-Reply-To", "<kickoff-7@spot.test>"),
            ("Auto-Submitted", "auto-generated"),
        ],
        "I am out of office until Monday.\r\n",
    )
}

/// The reply fixture: a human-written follow-up with a full `References` chain — `thread_key`
/// must pick the chain's root, not the immediate parent.
fn fixture_references() -> Vec<u8> {
    message(
        &[
            ("From", "Peer Reviewer <reviewer@spot.test>"),
            ("To", "Rana Kirti <Rana@Spot.test>"),
            ("Subject", "Re: Follow-up"),
            ("Date", DATE),
            ("Message-ID", "<reply-6@spot.test>"),
            ("In-Reply-To", "<kickoff-8@spot.test>"),
            ("References", "<root-8@spot.test> <kickoff-8@spot.test>"),
        ],
        "Here is the follow-up.\r\n",
    )
}

/// The oversize fixture: a multipart message with `filler` repetitions of padding text, sent
/// against a small `max_message_bytes` limit in the stub test.
fn fixture_big(filler: usize) -> Vec<u8> {
    message(
        &[
            ("From", "Reports <reports@spot.test>"),
            ("To", "Rana Kirti <Rana@Spot.test>"),
            ("Subject", "Oversize report"),
            ("Date", DATE),
            ("Message-ID", "<big-7@spot.test>"),
            ("MIME-Version", "1.0"),
            ("Content-Type", "multipart/mixed; boundary=\"big-7\""),
        ],
        &format!(
            "--big-7\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n--big-7--\r\n",
            "padding ".repeat(filler)
        ),
    )
}

#[tokio::test]
async fn fetches_everything_without_a_cursor_and_advances_the_cursor() {
    let raw = vec![fixture_alternative(), fixture_urdu()];
    let sizes = raw.iter().map(Vec::len).collect::<Vec<_>>();
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;

    let batch = fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must();

    assert_eq!(batch.outcome, FetchOutcome::New);
    assert!(!batch.more);
    let uids: Vec<u32> = batch.messages.iter().map(|message| message.uid).collect();
    assert_eq!(uids, vec![1, 2]);
    assert_eq!(
        batch.cursor,
        MailboxCursor {
            uid_validity: 1_700_000_000,
            last_uid: 2,
        }
    );

    let plain = &batch.messages[0];
    assert_eq!(plain.subject.as_deref(), Some("Weekly report is ready"));
    assert_eq!(plain.message_id.as_deref(), Some("alternative-1@spot.test"));
    assert_eq!(plain.in_reply_to, None);
    assert_eq!(plain.references, Vec::<String>::new());
    assert_eq!(plain.date, Some(DATE_UNIX));
    assert!(!plain.auto_submitted);
    assert_eq!(
        plain.from,
        Some(MailAddress {
            name: Some("Support".to_owned()),
            address: "support@spot.test".to_owned(),
        })
    );
    assert_eq!(
        plain.to,
        vec![MailAddress {
            name: Some("Rana Kirti".to_owned()),
            // Addresses keep the casing the sender used; nothing here rewrites them.
            address: "Rana@Spot.test".to_owned(),
        }]
    );
    assert_eq!(plain.text.as_deref(), Some("Hello in plain text."));
    assert_eq!(plain.html.as_deref(), Some("<p>Hello in <b>HTML</b>.</p>"));
    assert!(plain.attachments.is_empty());
    assert!(!plain.truncated);
    assert_eq!(plain.raw_size, sizes[0]);

    let urdu = &batch.messages[1];
    assert_eq!(urdu.message_id.as_deref(), Some("urdu-4@spot.test"));
    assert_eq!(urdu.text.as_deref(), Some(URDU));
    assert_eq!(urdu.html, None);
    assert_eq!(urdu.raw_size, sizes[1]);

    let recording = server.last_recording().await;
    assert_eq!(recording.login_user.as_deref(), Some(support::FAKE_USER));
    assert_eq!(recording.login_ok, Some(true));
    assert_eq!(recording.select_mailboxes, vec!["INBOX".to_owned()]);
    assert_eq!(recording.uid_searches, vec!["UID 1:1:*".to_owned()]);
    assert_eq!(
        recording.uid_fetches,
        vec!["(RFC822.SIZE)".to_owned(), "(BODY.PEEK[])".to_owned()]
    );
    assert!(recording.logout);
}

#[tokio::test]
async fn parses_the_remaining_fixtures_end_to_end() {
    let raw = vec![
        fixture_alternative(),
        fixture_inline_image(),
        fixture_rfc2231(),
        fixture_urdu(),
        fixture_auto_reply(),
        fixture_references(),
    ];
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;

    let batch = fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must();

    assert_eq!(batch.messages.len(), 6);

    let inline = &batch.messages[1];
    let attachment = inline.attachments.first().must();
    assert_eq!(attachment.filename, "logo.png");
    assert_eq!(attachment.content_type, content_type("image/png"));
    assert_eq!(attachment.content_id.as_deref(), Some("logo-2@spot.test"));
    assert_eq!(inline.attachments.len(), 1);
    assert!(attachment.bytes.starts_with(b"\x89PNG"));
    assert!(
        inline
            .html
            .as_deref()
            .is_some_and(|html| html.contains("cid:logo-2@spot.test"))
    );

    let named = &batch.messages[2];
    let attachment = named.attachments.first().must();
    assert_eq!(attachment.filename, "naïve résumé.pdf");
    assert_eq!(attachment.content_type, content_type("application/pdf"));
    assert_eq!(attachment.content_id, None);
    assert!(attachment.bytes.starts_with(b"%PDF"));

    let auto = &batch.messages[4];
    assert!(auto.auto_submitted);
    assert_eq!(auto.in_reply_to.as_deref(), Some("kickoff-7@spot.test"));
    assert_eq!(thread_key(auto).as_deref(), Some("kickoff-7@spot.test"));

    let reply = &batch.messages[5];
    assert!(!reply.auto_submitted);
    assert_eq!(reply.in_reply_to.as_deref(), Some("kickoff-8@spot.test"));
    assert_eq!(
        reply.references,
        vec![
            "root-8@spot.test".to_owned(),
            "kickoff-8@spot.test".to_owned()
        ]
    );
    assert_eq!(thread_key(reply).as_deref(), Some("root-8@spot.test"));
}

#[tokio::test]
async fn returns_only_the_uids_past_the_cursor() {
    let raw = vec![
        fixture_alternative(),
        fixture_urdu(),
        fixture_references(),
        fixture_auto_reply(),
    ];
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;

    let batch = fetch_new(
        &config(&server, ImapSecurity::Tls),
        Some(MailboxCursor {
            uid_validity: 1_700_000_000,
            last_uid: 2,
        }),
        FetchLimits::default(),
    )
    .await
    .must();

    assert_eq!(batch.outcome, FetchOutcome::New);
    let uids: Vec<u32> = batch.messages.iter().map(|message| message.uid).collect();
    assert_eq!(uids, vec![3, 4]);
    assert_eq!(batch.cursor.last_uid, 4);
    let recording = server.last_recording().await;
    assert_eq!(recording.uid_searches, vec!["UID 3:1:*".to_owned()]);
}

#[tokio::test]
async fn batches_by_limit_with_more_true_and_walks_the_cursor() {
    let raw = vec![
        fixture_alternative(),
        fixture_urdu(),
        fixture_inline_image(),
    ];
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;
    let limits = FetchLimits {
        max_messages: 2,
        ..FetchLimits::default()
    };

    let first = fetch_new(&config(&server, ImapSecurity::Tls), None, limits)
        .await
        .must();
    let uids: Vec<u32> = first.messages.iter().map(|message| message.uid).collect();
    assert_eq!(uids, vec![1, 2]);
    assert!(first.more);
    assert_eq!(first.cursor.last_uid, 2);

    let second = fetch_new(
        &config(&server, ImapSecurity::Tls),
        Some(first.cursor),
        limits,
    )
    .await
    .must();
    let uids: Vec<u32> = second.messages.iter().map(|message| message.uid).collect();
    assert_eq!(uids, vec![3]);
    assert!(!second.more);
    assert_eq!(server.recordings().await.len(), 2);
    let recording = server.last_recording().await;
    assert_eq!(recording.uid_searches, vec!["UID 3:1:*".to_owned()]);
}

#[tokio::test]
async fn reports_a_uidvalidity_change_as_mailbox_reset() {
    let raw = vec![fixture_alternative(), fixture_urdu()];
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;

    let first = fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must();
    assert_eq!(first.outcome, FetchOutcome::New);
    server
        .set_script(ImapScript {
            uid_validity: 2_000_000_000,
            messages: script(&raw).messages,
            ..ImapScript::default()
        })
        .await;

    let second = fetch_new(
        &config(&server, ImapSecurity::Tls),
        Some(first.cursor),
        FetchLimits::default(),
    )
    .await
    .must();

    assert_eq!(second.outcome, FetchOutcome::MailboxReset);
    let uids: Vec<u32> = second.messages.iter().map(|message| message.uid).collect();
    assert_eq!(uids, vec![1, 2]);
    assert_eq!(
        second.cursor,
        MailboxCursor {
            uid_validity: 2_000_000_000,
            last_uid: 2,
        }
    );
}

#[tokio::test]
async fn stays_quiet_on_an_empty_mailbox() {
    let server = spawn_tls().await;

    let batch = fetch_new(
        &config(&server, ImapSecurity::Tls),
        Some(MailboxCursor {
            uid_validity: 1_700_000_000,
            last_uid: 7,
        }),
        FetchLimits::default(),
    )
    .await
    .must();

    assert_eq!(batch.outcome, FetchOutcome::New);
    assert!(batch.messages.is_empty());
    assert!(!batch.more);
    assert_eq!(
        batch.cursor,
        MailboxCursor {
            uid_validity: 1_700_000_000,
            last_uid: 7,
        }
    );
    let recording = server.last_recording().await;
    assert_eq!(recording.uid_searches, vec!["UID 8:1:*".to_owned()]);
}

#[tokio::test]
async fn reduces_oversize_messages_to_header_stubs() {
    let big = fixture_big(80);
    let small = fixture_alternative();
    let raw = vec![big.clone(), small.clone()];
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;
    let limits = FetchLimits {
        // Above the small fixture, below the padded one: only the oversize one stubs.
        max_message_bytes: 450,
        ..FetchLimits::default()
    };

    let batch = fetch_new(&config(&server, ImapSecurity::Tls), None, limits)
        .await
        .must();

    let stub = &batch.messages[0];
    assert!(stub.truncated);
    assert_eq!(stub.text, None);
    assert_eq!(stub.html, None);
    assert!(stub.attachments.is_empty());
    assert_eq!(stub.raw_size, big.len());
    let full = &batch.messages[1];
    assert!(!full.truncated);
    assert_eq!(full.text.as_deref(), Some("Hello in plain text."));
    assert_eq!(full.raw_size, small.len());
    let recording = server.last_recording().await;
    assert_eq!(
        recording.uid_fetches,
        vec![
            "(RFC822.SIZE)".to_owned(),
            "(BODY.PEEK[HEADER])".to_owned(),
            "(BODY.PEEK[])".to_owned(),
        ]
    );
}

#[tokio::test]
async fn treats_an_unfetchable_body_as_transient() {
    let server = spawn_tls().await;
    server.set_script(script(&[Vec::new()])).await;

    let error = fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must_err();

    assert!(matches!(
        &error,
        MailError::Transient(TransientFailure::ConnectionReset)
    ));
}

#[tokio::test]
async fn classifies_a_mid_fetch_hangup_as_transient() {
    let raw = vec![
        fixture_alternative(),
        fixture_urdu(),
        fixture_inline_image(),
    ];
    let server = spawn_tls().await;
    server
        .set_script(ImapScript {
            drop_after_fetch_responses: 1,
            ..script(&raw)
        })
        .await;

    let error = fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must_err();

    assert!(matches!(
        &error,
        MailError::Transient(TransientFailure::ConnectionReset)
    ));
    let recording = server.last_recording().await;
    assert!(recording.dropped_mid_fetch);
    assert!(!recording.logout);
}

#[tokio::test]
async fn never_marks_messages_seen_or_touches_the_mailbox() {
    let raw = vec![fixture_alternative(), fixture_urdu()];
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;

    fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must();

    let recording = server.last_recording().await;
    // Every body fetch peeks (BODY.PEEK, never BODY[…]); no command changes mailbox state.
    assert!(
        recording
            .uid_fetches
            .iter()
            .all(|items| !items.contains("BODY[") || items.contains("BODY.PEEK"))
    );
    assert!(
        recording
            .commands
            .iter()
            .all(|command| !(command.starts_with("STORE")
                || command.starts_with("EXPUNGE")
                || command.starts_with("APPEND")))
    );
    assert_eq!(recording.select_mailboxes, vec!["INBOX".to_owned()]);
}

#[tokio::test]
async fn records_the_login_user_but_never_the_secret() {
    let raw = vec![fixture_alternative(), fixture_urdu()];
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;

    fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must();

    let recording = server.last_recording().await;
    assert_eq!(recording.login_user.as_deref(), Some(support::FAKE_USER));
    assert!(
        recording
            .commands
            .iter()
            .any(|command| command.starts_with("LOGIN [secret]"))
    );
    assert!(
        recording
            .commands
            .iter()
            .all(|command| !command.contains(support::FAKE_PASSWORD))
    );
}

#[tokio::test]
async fn classifies_a_refused_login_as_permanent() {
    let server = spawn_tls().await;
    server
        .set_script(ImapScript {
            reject_login: true,
            ..ImapScript::default()
        })
        .await;

    let error = fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must_err();

    assert!(matches!(
        &error,
        MailError::Permanent(PermanentFailure::Authentication { reply: None })
    ));
    let recording = server.last_recording().await;
    assert_eq!(recording.login_ok, Some(false));
}

#[tokio::test]
async fn classifies_a_refused_mailbox_as_permanent() {
    let server = spawn_tls().await;
    server
        .set_script(ImapScript {
            reject_select: true,
            ..ImapScript::default()
        })
        .await;

    let error = fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must_err();

    assert!(
        matches!(&error, MailError::Permanent(PermanentFailure::Client(reason)) if reason.contains("mailbox"))
    );
    let recording = server.last_recording().await;
    assert_eq!(recording.select_mailboxes, vec!["INBOX".to_owned()]);
    assert_eq!(recording.login_ok, Some(true));
}

#[tokio::test]
async fn classifies_a_refused_fetch_as_transient() {
    let raw = vec![fixture_alternative(), fixture_urdu()];
    let server = spawn_tls().await;
    server
        .set_script(ImapScript {
            reject_fetch: true,
            ..script(&raw)
        })
        .await;

    let error = fetch_new(
        &config(&server, ImapSecurity::Tls),
        None,
        FetchLimits::default(),
    )
    .await
    .must_err();

    assert!(matches!(
        &error,
        MailError::Transient(TransientFailure::ConnectionReset)
    ));
    let recording = server.last_recording().await;
    // Rejections are recorded at command time, before the NO goes out.
    assert_eq!(recording.uid_fetches, vec!["(RFC822.SIZE)".to_owned()]);
    assert!(recording.logout);
}

#[tokio::test]
async fn times_out_when_the_server_never_greets() {
    let server = spawn_tls().await;
    server
        .set_script(ImapScript {
            no_reply: true,
            ..ImapScript::default()
        })
        .await;
    let mut slow = config(&server, ImapSecurity::Tls);
    slow.timeout = Duration::from_millis(250);

    let error = fetch_new(&slow, None, FetchLimits::default())
        .await
        .must_err();

    assert!(matches!(
        &error,
        MailError::Transient(TransientFailure::Timeout)
    ));
}

#[tokio::test]
async fn ingests_over_starttls_after_the_upgrade() {
    let raw = vec![fixture_alternative()];
    let server = spawn_starttls().await;
    server.set_script(script(&raw)).await;

    let batch = fetch_new(
        &config(&server, ImapSecurity::StartTls),
        None,
        FetchLimits::default(),
    )
    .await
    .must();

    assert_eq!(batch.messages.len(), 1);
    assert_eq!(
        batch.messages[0].message_id.as_deref(),
        Some("alternative-1@spot.test")
    );
    let recording = server.last_recording().await;
    assert!(
        recording
            .commands
            .iter()
            .any(|command| command.trim() == "STARTTLS")
    );
    assert_eq!(recording.login_ok, Some(true));
}

#[tokio::test]
async fn refuses_an_untrusted_certificate() {
    let raw = vec![fixture_alternative()];
    let server = spawn_tls().await;
    server.set_script(script(&raw)).await;
    let mut untrusting = config(&server, ImapSecurity::Tls);
    untrusting.root_certificate_pem = None;

    let error = fetch_new(&untrusting, None, FetchLimits::default())
        .await
        .must_err();

    assert!(matches!(
        &error,
        MailError::Transient(TransientFailure::TlsHandshake)
    ));
    // The accept on the fake server completes before the handshake fails on it too.
    assert_eq!(server.connections(), 1);
}

#[tokio::test]
async fn rejects_an_invalid_config_before_dialing() {
    let server = spawn_tls().await;
    let mut invalid = config(&server, ImapSecurity::Tls);
    invalid.host = String::new();

    let error = fetch_new(&invalid, None, FetchLimits::default())
        .await
        .must_err();

    assert!(matches!(
        &error,
        MailError::Permanent(PermanentFailure::InvalidMessage(reason)) if reason.contains("host")
    ));
    // Rejected by validation: nothing ever dialed the fake server.
    assert_eq!(server.connections(), 0);
}
