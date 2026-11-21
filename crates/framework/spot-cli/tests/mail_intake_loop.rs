//! The own-mail loop guard (WP-PLT-11.T16) from the outgoing side to the intake side: the marker
//! the sender puts in `X-Spot-Outgoing` is on the wire, a copy of that mail coming back to a
//! polled mailbox is dropped before the sink, and an answer from outside to our mail is not.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::ManualClock;
use spot_mail::message::HeaderName;
use spot_mail::mime::build_mime_with_id;
use spot_mail::{
    FetchBatch, FetchLimits, FetchOutcome, IncomingMessage, MailAddress, MailError, Mailbox,
    MailboxCursor, OutgoingMessage, Secret, SelectedHeader,
};
use spot_mail_intake::memory::{
    MemoryAudit, MemoryCursorStore, MemorySeenStore, StaticCredentials,
};
use spot_mail_intake::{
    Accepted, Account, AuditEvent, DedupeKey, ImapEndpoint, IntakeLimits, IntakeSink, MailFetcher,
    OwnMailKey, PollDeps, PollState, SinkError, poll_once,
};
use time::OffsetDateTime;

const OWN: &str = "support@example.test";

struct Mailbox1(Mutex<Vec<IncomingMessage>>);

#[async_trait]
impl MailFetcher for Mailbox1 {
    async fn fetch(
        &self,
        _account: &Account,
        _secret: Secret,
        cursor: Option<MailboxCursor>,
        _limits: FetchLimits,
    ) -> Result<FetchBatch, MailError> {
        let last = cursor.map_or(0, |c| c.last_uid);
        let messages: Vec<IncomingMessage> = self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.uid > last)
            .cloned()
            .collect();
        let highest = messages.last().map_or(last, |m| m.uid);
        Ok(FetchBatch {
            outcome: FetchOutcome::New,
            cursor: MailboxCursor {
                uid_validity: 1,
                last_uid: highest,
            },
            messages,
            more: false,
        })
    }
}

#[derive(Default)]
struct CountingSink(AtomicUsize);

#[async_trait]
impl IntakeSink for CountingSink {
    async fn accept(
        &self,
        _account: &Account,
        _message: &IncomingMessage,
        _key: &DedupeKey,
    ) -> Result<Accepted, SinkError> {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Accepted::Created(format!("doc-{n}")))
    }
}

fn account() -> Account {
    Account {
        name: "support".to_owned(),
        imap: ImapEndpoint {
            host: "imap.example.test".to_owned(),
            port: 993,
            security: spot_mail::ImapSecurity::Tls,
            username: OWN.to_owned(),
            timeout: Duration::from_secs(10),
            root_certificate_pem: None,
        },
        credential_ref: "mail/support".to_owned(),
        mailbox: "INBOX".to_owned(),
        address: OWN.to_owned(),
        enabled: true,
        poll_interval: Duration::from_secs(60),
        limits: IntakeLimits::default(),
    }
}

fn incoming(uid: u32, id: &str, from: &str) -> IncomingMessage {
    IncomingMessage {
        uid,
        raw_size: 100,
        truncated: false,
        message_id: Some(id.to_owned()),
        in_reply_to: None,
        references: Vec::new(),
        from: Some(MailAddress {
            name: None,
            address: from.to_owned(),
        }),
        to: Vec::new(),
        cc: Vec::new(),
        reply_to: Vec::new(),
        subject: Some("subject".to_owned()),
        date: Some(1_700_000_000),
        text: Some("body".to_owned()),
        html: None,
        attachments: Vec::new(),
        auto_submitted: false,
        headers: Vec::new(),
    }
}

#[tokio::test]
async fn our_own_outgoing_mail_coming_back_does_not_loop() {
    let key = OwnMailKey::derive(b"the site's mail secret, long enough").unwrap();
    let id = "out-1@example.test";

    // The sender side: the marker is a header on the wire.
    let from = Mailbox::new(None, "noreply@example.test").unwrap();
    let mut outgoing = OutgoingMessage::new(from, "Your ticket");
    // A message with no recipient or no body builds nothing (the builder returns no bytes).
    outgoing
        .to
        .push(Mailbox::new(None, "customer@example.test").unwrap());
    outgoing.text = Some("Your ticket is open.".to_owned());
    outgoing.headers.push((
        HeaderName::new("X-Spot-Outgoing").unwrap(),
        key.marker(id).unwrap(),
    ));
    // A long header is folded onto a continuation line (RFC 5322 section 2.2.3): read it unfolded.
    let wire = String::from_utf8(build_mime_with_id(&outgoing, id))
        .unwrap()
        .replace("\r\n ", " ")
        .replace("\r\n\t", " ");
    assert!(
        wire.contains(&format!("X-Spot-Outgoing: {}", key.marker(id).unwrap())),
        "the marker is on the wire: {wire}"
    );

    // The intake side: that mail comes back from a forward rule (the From is not the account's
    // own address, so only the marker can recognise it), an outsider answers it, and a bounce
    // announces itself auto-submitted.
    let mut returned = incoming(1, &format!("<{id}>"), "forwarder@example.test");
    returned.headers.push(SelectedHeader {
        name: "X-Spot-Outgoing",
        value: key.marker(id).unwrap(),
    });
    let mut answer = incoming(2, "<answer-1@elsewhere.test>", "customer@elsewhere.test");
    answer.in_reply_to = Some(id.to_owned());
    answer.headers.push(SelectedHeader {
        name: "In-Reply-To",
        value: format!("<{id}>"),
    });
    let mut bounce = incoming(
        3,
        "<bounce-1@elsewhere.test>",
        "mailer-daemon@elsewhere.test",
    );
    bounce.auto_submitted = true;

    let mailbox = Mailbox1(Mutex::new(vec![returned, answer, bounce]));
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let presence = Arc::new(InMemoryPresence::new());
    let coordinator = InProcessCoordinator::start(presence, NodeId::new("node-a").unwrap(), clock)
        .await
        .unwrap();
    let cursors = MemoryCursorStore::new();
    let seen = MemorySeenStore::new();
    let sink = CountingSink::default();
    let audit = MemoryAudit::new();
    let creds = StaticCredentials::new()
        .with("mail/support", "pw")
        .with_own_mail_key(key);
    let deps = PollDeps {
        coordinator: &*coordinator,
        holder: "node-a",
        site: "acme",
        lease_ttl: Duration::from_secs(300),
        cursors: &cursors,
        seen: &seen,
        sink: &sink,
        audit: &audit,
        credentials: &creds,
        fetcher: &mailbox,
    };
    let mut state = PollState::new();
    let report = poll_once(&account(), OffsetDateTime::UNIX_EPOCH, &mut state, &deps).await;

    assert_eq!(
        report.accepted, 1,
        "only the outsider's answer is processed"
    );
    assert_eq!(report.dropped.get("own_mail"), Some(&1));
    assert_eq!(report.dropped.get("auto_reply"), Some(&1));
    assert_eq!(sink.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        audit.entries(),
        vec![(
            "support".to_owned(),
            AuditEvent::OwnMail {
                count: 1,
                first_uid: 1
            }
        )]
    );
}
