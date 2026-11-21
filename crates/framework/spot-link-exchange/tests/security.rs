//! Fail-closed behaviour: identity, replay, audit, quarantine, secrets.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use spot_audit_chain::PayloadValue;
use spot_link_exchange::{
    EntryStatus, Inbox, InboxCommit, InboxConfig, InboxEntry, InboxStore, Lifetimes, Message,
    MessageId, MessageParts, MessageState, ParkedEntry, PurgedPayload, Receipt, ReceiptOutcome,
    ReceiveStatus, RejectCode, SeenKind, TransportParty, canonicalize,
};
use spot_tenant_link::{
    IdentitySigner, LinkId, LinkStore, MemoryLinkStore, RevokeReason, SeedSigner,
};

fn one_message(w: &World, now: u64) -> Message {
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    outbox
        .enqueue_message(&w.submit(&conv, &topic, now), &body("secret-ish body"))
        .unwrap();
    outbox
        .next_batch(&w.link_a, now + 1, 1, "system", None)
        .unwrap()
        .remove(0)
}

#[test]
fn the_transport_identity_must_be_the_links_peer() {
    let w = World::new();
    let message = one_message(&w, T0);
    // The connection authenticated as B itself (or anyone but A).
    let error = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.b), &message, T0 + 2)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.identity_mismatch");
    assert!(w.inbox.stored().unwrap().is_empty());
}

#[test]
fn a_refusal_is_audited() {
    let w = World::new();
    let message = one_message(&w, T0);
    let audit = spot_tenant_link::MemoryAuditLog::new();
    let inbox = Inbox::new(&w.inbox, &audit, &w.quarantine, &w.signer_b);
    assert!(
        inbox
            .receive_from(&w.link_b, &connection(&w.b), &message, T0 + 2)
            .is_err()
    );
    assert_eq!(audit.entries().unwrap().len(), 1);
    let rejected = raw_bad_topic(&w, T0);
    inbox
        .receive_from(&w.link_b, &connection(&w.a), &rejected, T0 + 3)
        .unwrap();
    assert_eq!(audit.entries().unwrap().len(), 2);
}

fn raw_bad_topic(w: &World, now: u64) -> Message {
    Message::sign(
        MessageParts {
            id: spot_link_exchange::IdSource::next_id(&w.ids, now),
            link: w.link_a.id,
            conversation: conversation("c"),
            seq: 1,
            topic: spot_tenant_link::Topic::new("hr.payroll").unwrap(),
            schema_version: 1,
            sent_at: now,
            expires_at: now + DAY,
            tombstone: false,
            replaces: None,
            payload: "{}".to_owned(),
        },
        &w.signer_a,
    )
    .unwrap()
}

#[test]
fn a_tampered_or_foreign_signature_is_refused_without_a_receipt() {
    let w = World::new();
    let mut message = one_message(&w, T0);
    message.payload = r#"{"message.body":{"text":"changed"}}"#.to_owned();
    let error = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 2)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.bad_signature");

    // Signed by someone else who claims to be A.
    let mallory = SeedSigner::from_seed([9u8; 32], w.a.clone());
    let forged = Message::sign(
        MessageParts {
            id: spot_link_exchange::IdSource::next_id(&w.ids, T0),
            link: w.link_a.id,
            conversation: conversation("c"),
            seq: 1,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"x"}}"#.to_owned(),
        },
        &mallory,
    )
    .unwrap();
    let error = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &forged, T0 + 2)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.bad_signature");
    assert!(w.inbox.stored().unwrap().is_empty());
    assert!(
        w.quarantine.held().is_empty(),
        "an unattributable input is not the peer's to quarantine"
    );
}

#[test]
fn a_message_cannot_be_moved_to_another_link() {
    let w = World::new();
    let message = one_message(&w, T0);
    let mut other = w.link_b.clone();
    other.id = spot_tenant_link::LinkId::new_v7(1_700_000_000_000, [99u8; 10]);
    let error = w
        .receiver()
        .receive_from(&other, &connection(&w.a), &message, T0 + 2)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.link_mismatch");
}

#[test]
fn replaying_an_applied_message_changes_nothing() {
    let w = World::new();
    let message = one_message(&w, T0);
    let inbox = w.receiver();
    inbox
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 2)
        .unwrap();
    for n in 0..5 {
        let again = inbox
            .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 3 + n)
            .unwrap();
        assert_eq!(again.status, ReceiveStatus::Duplicate);
    }
    assert_eq!(w.inbox.stored().unwrap().len(), 1);
    assert_eq!(
        spot_link_exchange::NoticeFeed::read_from(&w.inbox, 0, 10)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_second_message_for_a_taken_sequence_number_is_refused() {
    let w = World::new();
    let message = one_message(&w, T0);
    let inbox = w.receiver();
    inbox
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 2)
        .unwrap();
    // Same conversation and seq 1, different id and content, validly signed.
    let conflicting = Message::sign(
        MessageParts {
            id: spot_link_exchange::IdSource::next_id(&w.ids, T0 + 5),
            link: w.link_a.id,
            conversation: conversation("c"),
            seq: 1,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0 + 5,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"rewritten"}}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap();
    let outcome = inbox
        .receive_from(&w.link_b, &connection(&w.a), &conflicting, T0 + 6)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::OutOfOrderExpired)
    );
    assert_eq!(w.inbox.stored().unwrap().len(), 1);
}

#[test]
fn a_forged_receipt_does_not_move_the_senders_state() {
    let w = World::new();
    let message = one_message(&w, T0);
    let outbox = w.sender(Lifetimes::default());
    let mallory = SeedSigner::from_seed([9u8; 32], w.b.clone());
    let forged = Receipt::sign(
        &mallory,
        w.link_a.id,
        message.id,
        message.conversation.clone(),
        message.seq,
        ReceiptOutcome::Applied,
        T0 + 3,
    );
    let error = outbox
        .on_receipt_from(&w.link_a, &connection(&w.b), &forged, T0 + 4)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.bad_signature");
    assert_eq!(outbox.state(&message.id).unwrap(), MessageState::Sent);
    // A genuine receipt arriving over the wrong transport identity is refused too.
    let genuine = Receipt::sign(
        &w.signer_b,
        w.link_a.id,
        message.id,
        message.conversation.clone(),
        message.seq,
        ReceiptOutcome::Applied,
        T0 + 3,
    );
    let error = outbox
        .on_receipt_from(&w.link_a, &connection(&w.a), &genuine, T0 + 4)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.identity_mismatch");
    assert_eq!(outbox.state(&message.id).unwrap(), MessageState::Sent);
}

#[test]
fn a_failing_audit_stops_the_receive_before_anything_is_stored() {
    let w = World::new();
    let message = one_message(&w, T0);
    let inbox = Inbox::new(&w.inbox, &FailingAudit, &w.quarantine, &w.signer_b);
    let error = inbox
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 2)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.audit_failed");
    assert!(w.inbox.stored().unwrap().is_empty());
    // The sender never got a receipt, so it keeps the message; a later receive succeeds.
    let ok = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 3)
        .unwrap();
    assert_eq!(ok.status, ReceiveStatus::Applied);
}

#[test]
fn a_failing_audit_stops_a_send() {
    let w = World::new();
    let outbox = spot_link_exchange::Outbox::new(
        &w.outbox,
        &FailingAudit,
        &w.signer_a,
        &w.ids,
        Lifetimes::default(),
    );
    let conv = conversation("c");
    let topic = message_topic();
    let error = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("x"))
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.audit_failed");
    assert!(w.outbox.all().unwrap().is_empty());
}

#[test]
fn a_failing_quarantine_acknowledges_nothing() {
    let w = World::new();
    let bad = raw_bad_topic(&w, T0);
    let inbox = Inbox::new(&w.inbox, &w.audit_b, &FailingQuarantine, &w.signer_b);
    let error = inbox
        .receive_from(&w.link_b, &connection(&w.a), &bad, T0 + 1)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.quarantine_failed");
    // Not recorded as seen: a retry is judged afresh and not answered as a duplicate.
    let retry = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &bad, T0 + 2)
        .unwrap();
    assert_eq!(
        retry.status,
        ReceiveStatus::Rejected(RejectCode::NotGranted)
    );
}

#[test]
fn a_float_oversize_or_future_dated_payload_is_refused() {
    let w = World::new();
    let topic = message_topic();
    let conv = conversation("c");
    let outbox = w.sender(Lifetimes::default());
    let mut payload = body("x");
    payload.insert(
        spot_tenant_link::FieldName::new("message.subject").unwrap(),
        spot_tenant_link::FieldValue::Text("y".repeat(300 * 1024)),
    );
    let error = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &payload)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.invalid_payload");
    assert!(canonicalize(r#"{"amount":12.5}"#).is_err());

    // A far-future `sent_at` is refused by the receiver.
    let future = Message::sign(
        MessageParts {
            id: spot_link_exchange::IdSource::next_id(&w.ids, T0),
            link: w.link_a.id,
            conversation: conv,
            seq: 1,
            topic,
            schema_version: 1,
            sent_at: T0 + 30 * DAY,
            expires_at: T0 + 31 * DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"x"}}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap();
    let outcome = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &future, T0 + 1)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::PayloadInvalid)
    );
}

#[test]
fn debug_output_never_shows_a_payload_or_a_signature() {
    let w = World::new();
    let message = one_message(&w, T0);
    let text = format!("{message:?}");
    assert!(!text.contains("secret-ish"), "{text}");
    assert!(text.contains("payload_bytes"));
    let receipt = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 2)
        .unwrap()
        .receipts
        .remove(0);
    let text = format!("{receipt:?}");
    assert!(!text.contains("signature"), "{text}");
}

#[test]
fn a_peer_region_requirement_without_an_oracle_fails_closed() {
    let w = World::new();
    let mut link = w.link_a.clone();
    for grant in &mut link.grants {
        grant.require_peer_region = Some(spot_data_policy_region());
    }
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    let request = spot_link_exchange::Submit {
        link: &link,
        ..w.submit(&conv, &topic, T0)
    };
    let error = outbox.enqueue_message(&request, &body("x")).unwrap_err();
    assert_eq!(error.code(), "tenant_link.peer_region_mismatch");
}

fn spot_data_policy_region() -> spot_data_policy::Region {
    spot_data_policy::Region::new("eu").unwrap()
}

/// Parks `message` directly in the store for `link`, as a receive would (a test that needs a
/// second link on the same inbox, without a second handshake).
fn park_under(w: &World, link: LinkId, entropy: u8, conversation_name: &str, seq: u64) -> Message {
    let message = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, [entropy; 10]),
            link,
            conversation: conversation(conversation_name),
            seq,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"parked"}}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap();
    let mut commit = InboxCommit::new(link, conversation(conversation_name), 1);
    commit.park.push(ParkedEntry {
        message: message.clone(),
        parked_at: T0,
    });
    w.inbox.commit(commit).unwrap();
    message
}

#[test]
fn a_flood_behind_a_gap_is_refused_with_a_stable_code_and_stores_nothing() {
    let w = World::new();
    let config = InboxConfig {
        max_parked_per_link: 1,
        ..InboxConfig::default()
    };
    let inbox = w.receiver().with_config(config);
    // A gap at sequence 1: everything ahead of it is parked.
    let first = park_under(&w, w.link_a.id, 9, "flood", 2);
    let ahead = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, [8; 10]),
            link: w.link_a.id,
            conversation: conversation("flood"),
            seq: 3,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"one too many"}}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap();
    let outcome = inbox
        .receive_from(&w.link_b, &connection(&w.a), &ahead, T0 + 1)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::CapacityExceeded)
    );
    assert_eq!(
        RejectCode::CapacityExceeded.code(),
        "link_exchange.capacity_exceeded"
    );
    assert_eq!(outcome.receipts.len(), 1);
    assert_eq!(
        outcome.receipts[0].outcome,
        ReceiptOutcome::Rejected(RejectCode::CapacityExceeded)
    );
    // Nothing was stored, and the message is not answered as known later on.
    assert!(w.inbox.seen(&w.link_b.id, &ahead.id).unwrap().is_none());
    assert!(w.inbox.stored().unwrap().is_empty());
    assert_eq!(w.inbox.parked().unwrap().len(), 1);
    // The refusal is audited under the stable code. `MemoryAuditLog::events` only replays
    // details it knows by name (the table lives in `spot-tenant-link`), so the code itself is
    // read from the record payload, which is what the sink stored.
    let refused: Vec<_> = w
        .audit_b
        .entries()
        .unwrap()
        .into_iter()
        .filter(|record| {
            record
                .payload
                .get("detail")
                .is_some_and(|value| matches!(value, PayloadValue::Text(text) if text == "link_exchange.capacity_exceeded"))
        })
        .collect();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].kind, "tenant_link.refused");
    // A repeat of the same message is refused again but held in the quarantine once.
    let again = inbox
        .receive_from(&w.link_b, &connection(&w.a), &ahead, T0 + 2)
        .unwrap();
    assert_eq!(
        again.status,
        ReceiveStatus::Rejected(RejectCode::CapacityExceeded)
    );
    assert_eq!(w.quarantine.held().len(), 1);
    assert_eq!(w.quarantine.held()[0].message.id, ahead.id);
    assert_ne!(first.id, ahead.id);
}

#[test]
fn one_links_flood_never_refuses_another_links_messages() {
    let w = World::new();
    let other = LinkId::new_v7(1_700_000_000_000, [3; 10]);
    // The other link parks a message, and holds a conversation, on the same inbox.
    park_under(&w, other, 7, "neighbour", 5);
    let config = InboxConfig {
        max_parked_per_link: 1,
        max_conversations_per_link: 1,
        ..InboxConfig::default()
    };
    let inbox = w.receiver().with_config(config);
    let ours = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, [6; 10]),
            link: w.link_a.id,
            conversation: conversation("ours"),
            seq: 2,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"ours"}}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap();
    // The neighbour's park and conversation are not counted against this link.
    let outcome = inbox
        .receive_from(&w.link_b, &connection(&w.a), &ours, T0 + 1)
        .unwrap();
    assert_eq!(outcome.status, ReceiveStatus::Parked);
    // Now this link holds its one conversation and its one park; the next conversation is
    // refused (the conversation cap is per link too).
    let third = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, [5; 10]),
            link: w.link_a.id,
            conversation: conversation("other"),
            seq: 2,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"third"}}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap();
    let refused = inbox
        .receive_from(&w.link_b, &connection(&w.a), &third, T0 + 2)
        .unwrap();
    assert_eq!(
        refused.status,
        ReceiveStatus::Rejected(RejectCode::CapacityExceeded)
    );
    assert!(w.inbox.seen(&w.link_a.id, &third.id).unwrap().is_none());
}

#[test]
fn the_parked_byte_cap_refuses_a_payload_that_would_exceed_it() {
    let w = World::new();
    let config = InboxConfig {
        max_parked_bytes_per_link: 8,
        ..InboxConfig::default()
    };
    let inbox = w.receiver().with_config(config);
    let big = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, [4; 10]),
            link: w.link_a.id,
            conversation: conversation("bytes"),
            seq: 2,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"far too many bytes to park"}}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap();
    let outcome = inbox
        .receive_from(&w.link_b, &connection(&w.a), &big, T0 + 1)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::CapacityExceeded)
    );
    assert!(w.inbox.parked().unwrap().is_empty());
}

/// A message with a chosen id and link, signed by the sender: the id is what the dedupe is keyed
/// on, so tests that use one id on two links need to build it themselves.
fn signed_for(w: &World, link: LinkId, id: MessageId, name: &str, seq: u64) -> Message {
    Message::sign(
        MessageParts {
            id,
            link,
            conversation: conversation(name),
            seq,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"shared id"}}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap()
}

#[test]
fn a_message_id_another_link_parked_does_not_suppress_this_links_message() {
    let w = World::new();
    let other = LinkId::new_v7(1_700_000_000_000, [3; 10]);
    // The other link already holds a message with the id this link is about to use.
    park_under(&w, other, 11, "shared", 5);
    let id = MessageId::new_v7(T0, [11; 10]);
    let message = signed_for(&w, w.link_a.id, id, "shared", 1);
    let outcome = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 2)
        .unwrap();
    // Stored in order: the other link's parking slot neither suppresses it nor answers for it.
    assert_eq!(outcome.status, ReceiveStatus::Applied);
    assert_eq!(
        w.inbox.seen(&w.link_a.id, &id).unwrap(),
        Some(SeenKind::Stored(EntryStatus::Applied))
    );
    assert_eq!(w.inbox.seen(&other, &id).unwrap(), Some(SeenKind::Parked));
    assert_eq!(w.inbox.stored().unwrap().len(), 1);
    assert_eq!(w.inbox.parked().unwrap().len(), 1);
}

#[test]
fn two_links_may_store_the_same_message_id_independently() {
    let w = World::new();
    let other = LinkId::new_v7(1_700_000_000_000, [2; 10]);
    let id = MessageId::new_v7(T0, [12; 10]);
    for link in [w.link_a.id, other] {
        let message = signed_for(&w, link, id, "both", 1);
        let mut commit = InboxCommit::new(link, conversation("both"), 1);
        commit.next_seq = 2;
        commit.store.push(InboxEntry {
            message,
            received_at: T0,
            status: EntryStatus::Applied,
        });
        w.inbox.commit(commit).unwrap();
    }
    // Both links keep their own message: one commit did not overwrite the other.
    assert_eq!(w.inbox.stored().unwrap().len(), 2);
    assert_eq!(
        w.inbox
            .entry(&w.link_a.id, &id)
            .unwrap()
            .unwrap()
            .message
            .link,
        w.link_a.id
    );
    assert_eq!(
        w.inbox.entry(&other, &id).unwrap().unwrap().message.link,
        other
    );
    assert_eq!(
        w.inbox.seen(&other, &id).unwrap(),
        Some(SeenKind::Stored(EntryStatus::Applied))
    );
}

/// A record of `link` waiting for the apply path, with the id and sequence a test names.
fn pending_record(w: &World, entropy: u8, name: &str, seq: u64) -> Message {
    let message = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, [entropy; 10]),
            link: w.link_a.id,
            conversation: conversation(name),
            seq,
            topic: record_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: r#"{"name":"INV-0001","version":1}"#.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap();
    let mut commit = InboxCommit::new(w.link_a.id, conversation(name), 1);
    commit.next_seq = seq + 1;
    commit.store.push(InboxEntry {
        message: message.clone(),
        received_at: T0,
        status: EntryStatus::PendingApply,
    });
    w.inbox.commit(commit).unwrap();
    message
}

#[test]
fn a_record_in_flight_at_revocation_is_not_marked_applied() {
    let w = World::new();
    let links = MemoryLinkStore::new();
    links.insert(w.link_b.clone()).unwrap();
    let inbox = w.receiver().with_links(&links);

    // While the link is active the apply path marks the record applied.
    let first = pending_record(&w, 21, "applied", 1);
    let receipt = inbox
        .record_applied(&w.link_b, &first.id, T0 + 1)
        .unwrap()
        .unwrap();
    assert_eq!(receipt.outcome, ReceiptOutcome::Applied);

    // A second record is in flight when the link is revoked behind the inbox's back (the caller
    // still holds the active snapshot).
    let second = pending_record(&w, 22, "inflight", 1);
    let revoked = w
        .link_b
        .revoked_by(&w.b, RevokeReason::TermsEnded, T0 + 5)
        .unwrap();
    links.update(revoked, 1).unwrap();

    let error = inbox
        .record_applied(&w.link_b, &second.id, T0 + 6)
        .unwrap_err();
    assert_eq!(error.code(), "tenant_link.not_active");
    // No applied row: the entry is still pending, where the retention sweep can close it.
    assert_eq!(
        w.inbox.seen(&w.link_a.id, &second.id).unwrap(),
        Some(SeenKind::Stored(EntryStatus::PendingApply))
    );
    assert_eq!(
        w.inbox
            .entry(&w.link_a.id, &second.id)
            .unwrap()
            .unwrap()
            .status,
        EntryStatus::PendingApply
    );
    // The refusal is on the trail under the code that caused it.
    let refused = w
        .audit_b
        .entries()
        .unwrap()
        .into_iter()
        .filter(|record| {
            record
                .payload
                .get("detail")
                .is_some_and(|value| matches!(value, PayloadValue::Text(text) if text == "tenant_link.not_active"))
        })
        .count();
    assert_eq!(refused, 1);
}

#[test]
fn only_the_connection_party_can_deliver_a_message() {
    let w = World::new();
    let message = one_message(&w, T0);
    // The carrier verified tenant A's identity on this connection.
    let connection = TransportParty::from_verified_connection(w.a.clone());
    let outcome = w
        .receiver()
        .receive_from(&w.link_b, &connection, &message, T0 + 2)
        .unwrap();
    assert_eq!(outcome.status, ReceiveStatus::Applied);

    // A message that names tenant B as its sender is refused even though it is validly signed:
    // the party that counts is the one the connection authenticated.
    let mut forged = signed_for(
        &w,
        w.link_a.id,
        MessageId::new_v7(T0, [31; 10]),
        "identity",
        1,
    );
    forged.sender = w.b.clone();
    let error = w
        .receiver()
        .receive_from(&w.link_b, &connection, &forged, T0 + 3)
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.identity_mismatch");
    assert_eq!(w.inbox.stored().unwrap().len(), 1);
}

#[test]
fn a_purged_payload_keeps_the_digest_and_signature_to_re_verify() {
    let w = World::new();
    let message = one_message(&w, T0);
    let inbox = w.receiver();
    inbox
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 2)
        .unwrap();

    // The keep period passes and the sweep empties the payload.
    assert_eq!(w.inbox.purge_applied(T0 + 2, T0 + 2 * DAY, 10).unwrap(), 1);
    let stored = w.inbox.entry(&w.link_a.id, &message.id).unwrap().unwrap();
    assert_eq!(stored.message.payload, "{}");
    // What the purge kept: the digest of the payload it emptied and the sender's signature over
    // it, so the row can still be shown to be the message that was signed.
    let marker = w.inbox.purged(&w.link_a.id, &message.id).unwrap().unwrap();
    assert_eq!(marker.purged_at, T0 + 2 * DAY);
    let mut rebuilt = stored.message.clone();
    rebuilt.payload = message.payload.clone();
    rebuilt.sender_signature = marker.signature;
    assert!(
        rebuilt
            .verify(&IdentitySigner::public_key(&w.signer_a))
            .is_ok(),
        "the kept signature still verifies"
    );
    assert_eq!(
        PurgedPayload::digest_hex(&rebuilt.payload_digest()),
        marker.digest,
        "the kept digest is the one the signature covers"
    );
    // A second sweep purges nothing twice.
    assert_eq!(w.inbox.purge_applied(T0 + 2, T0 + 3 * DAY, 10).unwrap(), 0);
}
