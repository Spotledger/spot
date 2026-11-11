//! Golden MIME round-trips (WP-PLT-11.T1): whatever `build_mime` writes, ordinary
//! RFC 5322 readers (mail-parser) must be able to parse back.

use mail_parser::{MessageParser, PartType};
use spot_mail::message::{Mailbox, OutgoingMessage};
use spot_mail::mime::build_mime_with_id;
use std::time::{Duration, SystemTime};

/// Test-only unwrapping that names the failure. The workspace denies `unwrap_used` and, via
/// `-D warnings`, treats `expect_used` (warn) as an error, so tests go through `must()`
/// instead of `expect()`. Mirrors the in-crate `Must` trait, which is `pub(crate)` +
/// `#[cfg(test)]` and therefore invisible from this separate integration-test binary.
trait Must<T> {
    fn must(self) -> T;
}

impl<T, E> Must<T> for Result<T, E>
where
    E: std::fmt::Debug,
{
    fn must(self) -> T {
        self.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
    }
}

impl<T> Must<T> for Option<T> {
    fn must(self) -> T {
        self.unwrap_or_else(|| panic!("unexpected None"))
    }
}

fn message() -> OutgoingMessage {
    let mut message = OutgoingMessage::new(
        Mailbox::new(Some("Ada Author".to_owned()), "ada@spot.test").must(),
        "Quarterly report",
    );
    message.to.push(Mailbox::new(None, "bob@dest.test").must());
    message.text = Some("Hello from spot_mail.".to_owned());
    message
}

fn parse(bytes: &[u8]) -> mail_parser::Message<'_> {
    MessageParser::default().parse(bytes).must()
}

#[test]
fn round_trips_a_text_message() {
    let outgoing = message();
    let bytes = build_mime_with_id(&outgoing, "gen-1@spot.test");
    let parsed = parse(&bytes);

    assert_eq!(parsed.subject().must(), "Quarterly report");
    assert_eq!(parsed.body_text(0).must(), "Hello from spot_mail.");
    let from = parsed.from().must();
    assert_eq!(
        from.first().must().address.as_deref(),
        Some("ada@spot.test")
    );
    assert_eq!(from.first().must().name.as_deref(), Some("Ada Author"));
    let to = parsed.to().must();
    assert_eq!(to.first().must().address.as_deref(), Some("bob@dest.test"));
    assert!(parsed.attachments().next().is_none());
    let raw = String::from_utf8(bytes.to_vec()).must();
    assert!(raw.contains("Message-ID: <gen-1@spot.test>"));
    assert!(raw.contains("Subject: Quarterly report"));
    assert!(raw.contains("To: bob@dest.test"));
    assert!(raw.contains("Date:"), "got {raw:?}");
    assert!(raw.contains("Content-Transfer-Encoding"));
    // No attachments: the body stays a bare single part.
    assert!(!raw.contains("multipart"), "got {raw:?}");
}

#[test]
fn round_trips_thread_headers() {
    let mut outgoing = message();
    outgoing.in_reply_to = Some("parent-1@spot.test".to_owned());
    outgoing.references = vec![
        "root-0@spot.test".to_owned(),
        "parent-1@spot.test".to_owned(),
    ];
    let bytes = build_mime_with_id(&outgoing, "reply-2@spot.test");
    let parsed = parse(&bytes);

    let raw = String::from_utf8(bytes.to_vec()).must();
    assert!(
        raw.contains("In-Reply-To: <parent-1@spot.test>"),
        "got {raw:?}"
    );
    assert!(
        raw.contains("References: <root-0@spot.test> <parent-1@spot.test>"),
        "got {raw:?}"
    );
    assert_eq!(parsed.message_id().must(), "reply-2@spot.test");
    assert!(
        !parsed
            .header_values("In-Reply-To")
            .collect::<Vec<_>>()
            .is_empty()
    );
}

#[test]
fn embeds_a_binary_attachment_as_mime_part() {
    let mut outgoing = message();
    outgoing.attachments.push(
        spot_mail::message::Attachment::new(
            "report.bin",
            "application/octet-stream",
            b"attachment-payload".to_vec(),
        )
        .must(),
    );

    let bytes = build_mime_with_id(&outgoing, "gen-2@spot.test");
    let raw = String::from_utf8(bytes.to_vec()).must();
    // The presence of an attachment switches the top level to multipart/mixed.
    assert!(raw.contains("Content-Type: multipart/mixed"), "got {raw:?}");
    let parsed = parse(&bytes);

    let mut attachments = parsed.attachments().collect::<Vec<_>>();
    assert_eq!(attachments.len(), 1, "one attachment");
    let attachment = attachments.pop().must();
    match &attachment.body {
        PartType::Binary(contents) => {
            assert_eq!(contents.as_ref(), b"attachment-payload".as_slice());
        }
        other => panic!("expected a binary part, got {other:?}"),
    }
    // The text body is still readable next to the attachment.
    assert_eq!(parsed.body_text(0).must(), "Hello from spot_mail.");
}

#[test]
fn names_attachments_and_declares_their_type() {
    let mut outgoing = message();
    outgoing.attachments.push(
        spot_mail::message::Attachment::new(
            "notes.txt",
            "text/plain; charset=utf-8",
            b"attachment-payload".to_vec(),
        )
        .must(),
    );

    let bytes = build_mime_with_id(&outgoing, "gen-3@spot.test");
    let parsed = parse(&bytes);

    let mut attachments = parsed.attachments().collect::<Vec<_>>();
    assert_eq!(attachments.len(), 1, "one attachment");
    let attachment = attachments.pop().must();
    match &attachment.body {
        PartType::Text(text) => assert_eq!(text.as_ref(), "attachment-payload"),
        other => panic!("expected a text part, got {other:?}"),
    }
    let raw = String::from_utf8(bytes.to_vec()).must();
    assert!(raw.contains(r#"name="notes.txt""#), "got {raw:?}");
    assert!(raw.contains("Content-Type: text/plain"), "got {raw:?}");
    // No inline images: the part must be a real attachment disposition.
    assert!(
        raw.contains("Content-Disposition: attachment"),
        "got {raw:?}"
    );
}

#[test]
fn round_trips_unicode_content() {
    let mut outgoing = message();
    outgoing.subject = "Notiz mit Ümläuten ☃".to_owned();
    outgoing.text = Some("Grüße aus der Wüste! ☃".to_owned());

    let bytes = build_mime_with_id(&outgoing, "gen-4@spot.test");
    let parsed = parse(&bytes);

    assert_eq!(parsed.subject().must(), "Notiz mit Ümläuten ☃");
    assert_eq!(parsed.body_text(0).must(), "Grüße aus der Wüste! ☃");
}

/// A fixed `Date` (2023-11-14T22:13:20Z) for the explicit-date test: without one, lettre stamps
/// the build time, so two builds of the same message differ across a second boundary.
fn fixed_date() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
}

#[test]
fn uses_the_explicit_date_and_encodes_deterministically() {
    let mut outgoing = message();
    outgoing.date = Some(fixed_date());
    let bytes = build_mime_with_id(&outgoing, "gen-5@spot.test");

    // A dated message encodes reproducibly: the same message and id build to byte-identical
    // MIME at any two moments, which is what send-versus-rebuild comparisons rely on.
    let rebuilt = build_mime_with_id(&outgoing, "gen-5@spot.test");
    assert_eq!(bytes, rebuilt, "a dated message encodes reproducibly");

    // The explicit date went to the wire formatted for RFC 5322 readers.
    let raw = String::from_utf8(bytes.to_vec()).must();
    assert!(
        raw.contains("Date: Tue, 14 Nov 2023 22:13:20 +0000"),
        "got {raw:?}"
    );
    let parsed = parse(&bytes);
    let date = parsed.date().must();
    assert_eq!(
        (
            date.year,
            date.month,
            date.day,
            date.hour,
            date.minute,
            date.second
        ),
        (2023, 11, 14, 22, 13, 20)
    );
}

#[test]
fn a_multi_line_display_name_is_refused_before_it_can_reach_the_encoder() {
    // Mailbox::new refuses it ...
    assert!(Mailbox::new(Some("Ada\r\nBcc: evil@x.test".to_owned()), "ada@spot.test").is_err());
    // ... and so does validate() when the public fields were set directly (the encoder
    // would otherwise panic on such a name).
    let mut built = message();
    built.from.name = Some("Ada\nX-Evil: 1".to_owned());
    assert!(built.validate().is_err());
    let mut built = message();
    built.to[0].name = Some("Bob\rX-Evil: 1".to_owned());
    assert!(built.validate().is_err());
    assert!(message().validate().is_ok());
}
