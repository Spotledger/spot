//! The functional contract of the sender and the receiver.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use spot_link_exchange::{
    EventKind, Lifetimes, MemoryOutbox, Message, MessageId, MessageParts, MessageState, NoticeFeed,
    Outbox, OutboxEntry, OutboxStore, ReceiptEffect, ReceiveStatus, RejectCode, Retention,
    Thresholds, stuck_messages,
};
use spot_tenant_link::{Direction, FieldName, FieldValue, Link, RevokeReason, Topic};

fn deliver(w: &World, batch: &[Message], now: u64) -> Vec<spot_link_exchange::Receipt> {
    let inbox = w.receiver();
    let mut receipts = Vec::new();
    for message in batch {
        let outcome = inbox
            .receive_from(&w.link_b, &connection(&w.a), message, now)
            .unwrap();
        receipts.extend(outcome.receipts);
    }
    receipts
}

fn settle(w: &World, receipts: &[spot_link_exchange::Receipt], now: u64) {
    let outbox = w.sender(Lifetimes::default());
    for receipt in receipts {
        outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), receipt, now)
            .unwrap();
    }
}

fn raw(
    w: &World,
    conv: &str,
    seq: u64,
    topic: &Topic,
    version: u32,
    payload: &str,
    now: u64,
) -> Message {
    Message::sign(
        MessageParts {
            id: spot_link_exchange::IdSource::next_id(&w.ids, now),
            link: w.link_a.id,
            conversation: conversation(conv),
            seq,
            topic: topic.clone(),
            schema_version: version,
            sent_at: now,
            expires_at: now + DAY,
            tombstone: false,
            replaces: None,
            payload: payload.to_owned(),
        },
        &w.signer_a,
    )
    .unwrap()
}

#[test]
fn a_message_travels_from_queued_to_applied_and_leaves_a_notice() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("order-1");
    let topic = message_topic();
    let queued = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("ship it"))
        .unwrap();
    assert_eq!(queued.seq, 1);
    assert_eq!(outbox.state(&queued.id).unwrap(), MessageState::Queued);

    let batch = outbox
        .next_batch(&w.link_a, T0 + 1, 10, "system", None)
        .unwrap();
    assert_eq!(batch.len(), 1);
    assert_eq!(outbox.state(&queued.id).unwrap(), MessageState::Sent);

    let outcome = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], T0 + 2)
        .unwrap();
    assert_eq!(outcome.status, ReceiveStatus::Applied);
    for receipt in &outcome.receipts {
        let effect = outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), receipt, T0 + 3)
            .unwrap();
        assert_eq!(effect, ReceiptEffect::Moved(MessageState::Applied));
    }
    assert_eq!(outbox.state(&queued.id).unwrap(), MessageState::Applied);

    let notices = w.inbox.read_from(0, 10).unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].link, w.link_b.id);
    assert_eq!(notices[0].message_id, queued.id);
    assert_eq!(notices[0].topic, topic);
    assert_eq!(notices[0].conversation, conv);
    assert_eq!(notices[0].source, w.a);
    assert_eq!(notices[0].received_at, T0 + 2);

    let events = outbox.events_from(0, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1.kind, EventKind::Applied);
}

#[test]
fn the_feed_is_at_least_once_until_acked() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    for n in 0..3 {
        outbox
            .enqueue_message(&w.submit(&conv, &topic, T0 + n), &body("x"))
            .unwrap();
    }
    let batch = outbox
        .next_batch(&w.link_a, T0 + 10, 10, "system", None)
        .unwrap();
    deliver(&w, &batch, T0 + 11);
    let first = w.inbox.read_from(0, 2).unwrap();
    assert_eq!(first.len(), 2);
    // A consumer that crashed before acking reads the same notices again.
    assert_eq!(w.inbox.read_from(0, 2).unwrap(), first);
    w.inbox.ack("process-engine", first[1].cursor).unwrap();
    assert_eq!(w.inbox.acked("process-engine").unwrap(), 2);
    // Acking an older cursor moves nothing back.
    w.inbox.ack("process-engine", 1).unwrap();
    assert_eq!(w.inbox.acked("process-engine").unwrap(), 2);
    let rest = w
        .inbox
        .read_from(w.inbox.acked("process-engine").unwrap(), 10)
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].cursor, 3);
}

#[test]
fn a_duplicate_is_absorbed_and_the_receipt_repeated() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("once"))
        .unwrap();
    let batch = outbox
        .next_batch(&w.link_a, T0 + 1, 10, "system", None)
        .unwrap();
    let inbox = w.receiver();
    let first = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], T0 + 2)
        .unwrap();
    let second = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], T0 + 3)
        .unwrap();
    assert_eq!(first.status, ReceiveStatus::Applied);
    assert_eq!(second.status, ReceiveStatus::Duplicate);
    assert_eq!(second.receipts.len(), 1);
    assert_eq!(w.inbox.stored().unwrap().len(), 1);
    assert_eq!(w.inbox.read_from(0, 10).unwrap().len(), 1);
    // The repeated receipt repairs a lost one.
    settle(&w, &second.receipts, T0 + 4);
    assert_eq!(outbox.state(&batch[0].id).unwrap(), MessageState::Applied);
}

#[test]
fn a_gap_parks_and_the_arrival_releases_in_order() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    let ids: Vec<MessageId> = (0..3)
        .map(|n| {
            outbox
                .enqueue_message(&w.submit(&conv, &topic, T0 + n), &body(&format!("m{n}")))
                .unwrap()
                .id
        })
        .collect();
    let batch = outbox
        .next_batch(&w.link_a, T0 + 10, 10, "system", None)
        .unwrap();
    let inbox = w.receiver();
    let third = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[2], T0 + 11)
        .unwrap();
    let second = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[1], T0 + 12)
        .unwrap();
    assert_eq!(third.status, ReceiveStatus::Parked);
    assert_eq!(second.status, ReceiveStatus::Parked);
    assert!(
        w.inbox.read_from(0, 10).unwrap().is_empty(),
        "parked is not applied"
    );
    settle(&w, &third.receipts, T0 + 13);
    assert_eq!(outbox.state(&ids[2]).unwrap(), MessageState::Acknowledged);

    let first = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], T0 + 14)
        .unwrap();
    assert_eq!(first.status, ReceiveStatus::Applied);
    assert_eq!(
        first.receipts.len(),
        3,
        "the arrival also releases both parked messages"
    );
    settle(&w, &first.receipts, T0 + 15);
    for id in &ids {
        assert_eq!(outbox.state(id).unwrap(), MessageState::Applied);
    }
    let order: Vec<MessageId> = w
        .inbox
        .read_from(0, 10)
        .unwrap()
        .iter()
        .map(|n| n.message_id)
        .collect();
    assert_eq!(order, ids, "applied in sequence order");
}

#[test]
fn fields_outside_the_grant_never_leave_the_sender() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    let mut payload = body("hello");
    payload.insert(
        FieldName::new("employee.salary").unwrap(),
        FieldValue::Int(1_234_567),
    );
    let queued = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &payload)
        .unwrap();
    assert_eq!(queued.withheld, vec!["employee.salary".to_owned()]);
    let batch = outbox
        .next_batch(&w.link_a, T0 + 1, 10, "system", None)
        .unwrap();
    assert!(!batch[0].payload.contains("salary"));
    assert!(!batch[0].payload.contains("1234567"));
}

#[test]
fn the_receiver_refuses_a_field_outside_its_grant_even_from_a_valid_signer() {
    let w = World::new();
    let topic = message_topic();
    let bad = raw(
        &w,
        "c",
        1,
        &topic,
        1,
        r#"{"employee.salary":{"int":1},"message.body":{"text":"x"}}"#,
        T0,
    );
    let outcome = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &bad, T0 + 1)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::PayloadInvalid)
    );
    assert!(w.inbox.stored().unwrap().is_empty());
    assert!(w.inbox.read_from(0, 10).unwrap().is_empty());
    let held = w.quarantine.held();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].code, RejectCode::PayloadInvalid);
    // The sender sees the rejection, as a terminal state with an event.
    let outbox = w.sender(Lifetimes::default());
    assert!(
        outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), &outcome.receipts[0], T0 + 2)
            .is_err(),
        "a message this side never queued is unknown to its outbox"
    );
}

#[test]
fn an_unsupported_schema_version_is_rejected_with_its_code() {
    let w = World::new();
    let topic = message_topic();
    let message = raw(
        &w,
        "c",
        1,
        &topic,
        9,
        r#"{"message.body":{"text":"x"}}"#,
        T0,
    );
    let outcome = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 1)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::VersionUnsupported)
    );
    // The sender refuses to send it in the first place.
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let mut request = w.submit(&conv, &topic, T0);
    request.schema_version = 9;
    let error = outbox.enqueue_message(&request, &body("x")).unwrap_err();
    assert_eq!(error.code(), "link_exchange.schema_unsupported");
}

#[test]
fn an_ungranted_topic_or_wrong_direction_is_not_granted() {
    let w = World::new();
    let other = Topic::new("hr.payroll").unwrap();
    let message = raw(&w, "c", 1, &other, 1, "{}", T0);
    let outcome = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 1)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::NotGranted)
    );

    // B granted only Send; A's messages are not received.
    let mut send_only = w.link_b.clone();
    for grant in &mut send_only.grants {
        grant.direction = Direction::Send;
    }
    let message = raw(
        &w,
        "c",
        1,
        &message_topic(),
        1,
        r#"{"message.body":{"text":"x"}}"#,
        T0,
    );
    let outcome = w
        .receiver()
        .receive_from(&send_only, &connection(&w.a), &message, T0 + 1)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::NotGranted)
    );
}

#[test]
fn a_revoked_link_holds_queued_messages_and_the_receiver_refuses_arrivals() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    let queued = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("x"))
        .unwrap();
    let revoked: Link = w
        .link_a
        .revoked_by(&w.a, RevokeReason::SecurityConcern, T0 + 5)
        .unwrap();
    let batch = outbox
        .next_batch(&revoked, T0 + 6, 10, "system", None)
        .unwrap();
    assert!(batch.is_empty(), "nothing is sent on a revoked link");
    assert_eq!(outbox.state(&queued.id).unwrap(), MessageState::Queued);
    assert!(
        outbox
            .enqueue_message(
                &spot_link_exchange::Submit {
                    link: &revoked,
                    ..w.submit(&conv, &topic, T0 + 7)
                },
                &body("y")
            )
            .is_err()
    );
    // The receiving side, revoked too, refuses what was already in flight.
    let revoked_b = w
        .link_b
        .revoked_by(&w.b, RevokeReason::TermsEnded, T0 + 5)
        .unwrap();
    let message = raw(
        &w,
        "c",
        1,
        &topic,
        1,
        r#"{"message.body":{"text":"x"}}"#,
        T0,
    );
    let outcome = w
        .receiver()
        .receive_from(&revoked_b, &connection(&w.a), &message, T0 + 6)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::NotGranted)
    );
}

#[test]
fn retention_sets_the_expiry_and_expiry_is_visible_to_the_sender() {
    let w = World::new();
    let lifetimes = Lifetimes::new(Retention::from_days(2).unwrap());
    let outbox = w.sender(lifetimes);
    let conv = conversation("c");
    let topic = message_topic();
    let queued = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("x"))
        .unwrap();
    assert_eq!(queued.expires_at, T0 + 2 * DAY);
    // Not yet: still deliverable.
    assert_eq!(outbox.expire_due(T0 + 2 * DAY - 1).unwrap().len(), 0);
    let events = outbox.expire_due(T0 + 2 * DAY).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, EventKind::Expired);
    assert_eq!(outbox.state(&queued.id).unwrap(), MessageState::Expired);
    assert_eq!(outbox.events_from(0, 10).unwrap().len(), 1);
    // Once is enough.
    assert_eq!(outbox.expire_due(T0 + 3 * DAY).unwrap().len(), 0);
}

#[test]
fn the_default_retention_is_thirty_days() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    let queued = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("x"))
        .unwrap();
    assert_eq!(queued.expires_at, T0 + 30 * DAY);
}

#[test]
fn an_expired_message_does_not_block_the_messages_behind_it() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::new(Retention::from_days(2).unwrap()));
    let conv = conversation("c");
    let topic = message_topic();
    let first = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("old"))
        .unwrap();
    let second = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0 + DAY + HOUR), &body("new"))
        .unwrap();
    // Day 2: the first expired without ever being delivered; the second is still good.
    let now = T0 + 2 * DAY + HOUR;
    let batch = outbox
        .next_batch(&w.link_a, now, 10, "system", None)
        .unwrap();
    assert_eq!(outbox.state(&first.id).unwrap(), MessageState::Expired);
    assert_eq!(
        batch.len(),
        2,
        "the tombstone for slot 1 and the second message"
    );
    assert!(batch[0].tombstone && batch[0].seq == 1);
    // Deliver the second first: it parks behind the open slot, then the tombstone releases it.
    let inbox = w.receiver();
    let parked = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[1], now + 1)
        .unwrap();
    assert_eq!(parked.status, ReceiveStatus::Parked);
    let filled = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], now + 2)
        .unwrap();
    assert_eq!(filled.status, ReceiveStatus::Applied);
    settle(&w, &parked.receipts, now + 3);
    settle(&w, &filled.receipts, now + 3);
    assert_eq!(outbox.state(&second.id).unwrap(), MessageState::Applied);
    let stored = w.inbox.stored().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].1.message.id, second.id);
    // The tombstone is confirmed and the sender stops sending it.
    assert!(
        outbox
            .next_batch(&w.link_a, now + 4, 10, "system", None)
            .unwrap()
            .is_empty()
    );
}

fn first_message(w: &World, id: &MessageId) -> Message {
    use spot_link_exchange::OutboxStore;
    w.outbox.find(id).unwrap().unwrap().message
}

#[test]
fn a_late_original_of_an_expired_slot_is_never_applied() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::new(Retention::from_days(1).unwrap()));
    let conv = conversation("c");
    let topic = message_topic();
    let first = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("old"))
        .unwrap();
    let original = first_message(&w, &first.id);
    let now = T0 + DAY + 1;
    let batch = outbox
        .next_batch(&w.link_a, now, 10, "system", None)
        .unwrap();
    assert!(batch[0].tombstone);
    let inbox = w.receiver();
    inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], now + 1)
        .unwrap();
    let late = inbox
        .receive_from(&w.link_b, &connection(&w.a), &original, now + 2)
        .unwrap();
    assert_eq!(
        late.status,
        ReceiveStatus::Rejected(RejectCode::OutOfOrderExpired)
    );
    assert!(w.inbox.stored().unwrap().is_empty());
    assert_eq!(w.quarantine.held().len(), 1);
}

#[test]
fn a_rejected_message_gets_a_tombstone_so_the_conversation_moves_on() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    // The receiver's grant range is 1..=2; the sender's own check is bypassed by a lower grant
    // on the receiving side: use a link copy whose grant range excludes version 1.
    let mut narrow = w.link_b.clone();
    for grant in &mut narrow.grants {
        grant.schema_range = spot_tenant_link::SchemaRange::new(2, 2).unwrap();
    }
    let one = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("v1"))
        .unwrap();
    let mut v2 = w.submit(&conv, &topic, T0 + 1);
    v2.schema_version = 2;
    let two = outbox.enqueue_message(&v2, &body("v2")).unwrap();
    let batch = outbox
        .next_batch(&w.link_a, T0 + 5, 10, "system", None)
        .unwrap();
    let inbox = w.receiver();
    let mut receipts = Vec::new();
    for message in &batch {
        receipts.extend(
            inbox
                .receive_from(&narrow, &connection(&w.a), message, T0 + 6)
                .unwrap()
                .receipts,
        );
    }
    for receipt in &receipts {
        outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), receipt, T0 + 7)
            .unwrap();
    }
    assert_eq!(
        outbox.state(&one.id).unwrap(),
        MessageState::Rejected(RejectCode::VersionUnsupported)
    );
    // The second is parked behind the refused slot until the tombstone arrives.
    assert_eq!(outbox.state(&two.id).unwrap(), MessageState::Acknowledged);
    let batch = outbox
        .next_batch(&w.link_a, T0 + 7 + 60_000, 10, "system", None)
        .unwrap();
    assert!(batch.iter().any(|m| m.tombstone && m.seq == 1));
    let mut receipts = Vec::new();
    for message in batch.iter().filter(|m| m.tombstone) {
        receipts.extend(
            inbox
                .receive_from(&narrow, &connection(&w.a), message, T0 + 8 + 60_000)
                .unwrap()
                .receipts,
        );
    }
    for receipt in &receipts {
        outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), receipt, T0 + 9 + 60_000)
            .unwrap();
    }
    assert_eq!(outbox.state(&two.id).unwrap(), MessageState::Applied);
    assert_eq!(w.inbox.stored().unwrap().len(), 1);
}

#[test]
fn a_parked_message_that_expires_is_refused_by_the_sweep() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::new(Retention::from_days(1).unwrap()));
    let conv = conversation("c");
    let topic = message_topic();
    outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("a"))
        .unwrap();
    let second = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0 + 1), &body("b"))
        .unwrap();
    let batch = outbox
        .next_batch(&w.link_a, T0 + 2, 10, "system", None)
        .unwrap();
    let inbox = w.receiver();
    inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[1], T0 + 3)
        .unwrap();
    assert!(inbox.sweep_expired(T0 + DAY - 1).unwrap().is_empty());
    let receipts = inbox.sweep_expired(T0 + DAY + 2).unwrap();
    assert_eq!(receipts.len(), 1);
    for receipt in &receipts {
        outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), receipt, T0 + DAY + 3)
            .unwrap();
    }
    assert_eq!(
        outbox.state(&second.id).unwrap(),
        MessageState::Rejected(RejectCode::OutOfOrderExpired)
    );
    assert_eq!(w.quarantine.held().len(), 1);
}

#[test]
fn stuck_exchange_becomes_plain_language_conditions() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("c");
    let topic = message_topic();
    outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("a"))
        .unwrap();
    outbox
        .enqueue_message(&w.submit(&conv, &topic, T0 + 1), &body("b"))
        .unwrap();
    let batch = outbox
        .next_batch(&w.link_a, T0 + 2, 10, "system", None)
        .unwrap();
    // Only the second arrives: it parks behind the first.
    w.receiver()
        .receive_from(&w.link_b, &connection(&w.a), &batch[1], T0 + 3)
        .unwrap();
    let now = T0 + 3 * HOUR;
    let thresholds = Thresholds::default();
    let sender_side = stuck_messages(
        &w.outbox,
        &w.inbox,
        std::slice::from_ref(&w.link_a),
        now,
        &thresholds,
    )
    .unwrap();
    let codes: Vec<&str> = sender_side.iter().map(|c| c.code.as_str()).collect();
    assert!(codes.contains(&"LINK_MESSAGES_UNCONFIRMED"), "{codes:?}");
    let receiver_side = stuck_messages(
        &w.outbox,
        &w.inbox,
        std::slice::from_ref(&w.link_b),
        now,
        &thresholds,
    )
    .unwrap();
    let gap = receiver_side
        .iter()
        .find(|c| c.code == "LINK_INBOX_GAP")
        .unwrap();
    assert!(gap.sentence.contains("1 message"), "{}", gap.sentence);
    assert!(!gap.action.is_empty());
    // Nothing is reported while things are fresh.
    let fresh = stuck_messages(
        &w.outbox,
        &w.inbox,
        std::slice::from_ref(&w.link_a),
        T0 + 10,
        &thresholds,
    )
    .unwrap();
    assert!(fresh.is_empty());
}

#[test]
fn a_link_close_to_its_end_is_reported() {
    let w = World::new();
    let mut link = w.link_a.clone();
    link.expires = Some(T0 + 2 * DAY);
    let list = stuck_messages(
        &w.outbox,
        &w.inbox,
        std::slice::from_ref(&link),
        T0,
        &Thresholds::default(),
    )
    .unwrap();
    assert!(list.iter().any(|c| c.code == "LINK_NEARLY_EXPIRED"));
}

/// An outbox store whose `settle` fails: the crash window the old two-write path could fall into,
/// where the state moved and the producer event did not. Like a transaction, it stores
/// nothing at all when it fails.
struct EventWriteFails {
    inner: MemoryOutbox,
}

impl EventWriteFails {
    fn new() -> Self {
        Self {
            inner: MemoryOutbox::new(),
        }
    }
}

impl OutboxStore for EventWriteFails {
    fn insert(&self, entry: OutboxEntry) -> spot_link_exchange::Result<()> {
        self.inner.insert(entry)
    }

    fn find(&self, id: &MessageId) -> spot_link_exchange::Result<Option<OutboxEntry>> {
        self.inner.find(id)
    }

    fn update(&self, entry: OutboxEntry) -> spot_link_exchange::Result<()> {
        self.inner.update(entry)
    }

    fn last_seq(
        &self,
        link: &spot_tenant_link::LinkId,
        conversation: &spot_link_exchange::Conversation,
    ) -> spot_link_exchange::Result<u64> {
        self.inner.last_seq(link, conversation)
    }

    fn live(
        &self,
        link: Option<&spot_tenant_link::LinkId>,
    ) -> spot_link_exchange::Result<Vec<OutboxEntry>> {
        self.inner.live(link)
    }

    fn settle(
        &self,
        _entry: OutboxEntry,
        _event: Option<spot_link_exchange::ProducerEvent>,
    ) -> spot_link_exchange::Result<()> {
        Err(spot_link_exchange::Error::StoreFailed {
            reason: "the producer event could not be written",
        })
    }

    fn push_event(
        &self,
        event: spot_link_exchange::ProducerEvent,
    ) -> spot_link_exchange::Result<()> {
        self.inner.push_event(event)
    }

    fn events_from(
        &self,
        after: u64,
        limit: usize,
    ) -> spot_link_exchange::Result<Vec<(u64, spot_link_exchange::ProducerEvent)>> {
        self.inner.events_from(after, limit)
    }
}

#[test]
fn an_expired_entry_never_exists_without_its_producer_event() {
    let w = World::new();
    let store = EventWriteFails::new();
    let outbox = Outbox::new(
        &store,
        &w.audit_a,
        &w.signer_a,
        &w.ids,
        Lifetimes::default(),
    );
    let queued = outbox
        .enqueue_message(
            &w.submit(&conversation("crash"), &message_topic(), T0),
            &body("expiring"),
        )
        .unwrap();
    // The event cannot be written, so the state move is rolled back with it: no Expired entry
    // without its event, which is the crash window a recorded security finding is about.
    let error = outbox.expire_due(queued.expires_at + 1).unwrap_err();
    assert!(matches!(
        error,
        spot_link_exchange::Error::StoreFailed { .. }
    ));
    assert_eq!(
        outbox.state(&queued.id).unwrap(),
        MessageState::Queued,
        "the entry did not move"
    );
    assert!(outbox.events_from(0, 10).unwrap().is_empty());

    // With a store that commits, the entry and the event are there together.
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let queued = outbox
        .enqueue_message(
            &w.submit(&conversation("crash"), &message_topic(), T0),
            &body("expiring"),
        )
        .unwrap();
    let events = outbox.expire_due(queued.expires_at + 1).unwrap();
    assert_eq!(outbox.state(&queued.id).unwrap(), MessageState::Expired);
    assert_eq!(events.len(), 1);
    assert_eq!(outbox.events_from(0, 10).unwrap().len(), 1);
    assert_eq!(events[0].message, queued.id);
}

// ADR-058 rule 6 at the receiver: a grant may require the peer to sit in a region.

fn region(name: &str) -> spot_data_policy::Region {
    spot_data_policy::Region::new(name).unwrap()
}

/// B's link with every grant requiring the peer in `eu`.
fn restricted_b(w: &World) -> Link {
    let mut link = w.link_b.clone();
    for grant in &mut link.grants {
        grant.require_peer_region = Some(region("eu"));
    }
    link
}

fn one_body(w: &World) -> Message {
    raw(
        w,
        "c",
        1,
        &message_topic(),
        1,
        r#"{"message.body":{"text":"x"}}"#,
        T0,
    )
}

fn receive_with(
    w: &World,
    link: &Link,
    oracle: Option<&dyn spot_tenant_link::RegionOracle>,
) -> ReceiveStatus {
    let inbox = w.receiver();
    let inbox = match oracle {
        Some(oracle) => inbox.with_region_oracle(oracle),
        None => inbox,
    };
    inbox
        .receive_from(link, &connection(&w.a), &one_body(w), T0 + 1)
        .unwrap()
        .status
}

#[test]
fn an_oracle_that_places_the_peer_in_the_required_region_lets_the_message_in() {
    let w = World::new();
    let oracle = spot_tenant_link::MemoryRegionOracle::new();
    oracle.insert(w.a.clone(), region("eu")).unwrap();
    let status = receive_with(&w, &restricted_b(&w), Some(&oracle));
    assert_eq!(status, ReceiveStatus::Applied);
    assert_eq!(w.inbox.stored().unwrap().len(), 1);
}

#[test]
fn an_oracle_that_places_the_peer_elsewhere_refuses_and_stores_nothing() {
    let w = World::new();
    let oracle = spot_tenant_link::MemoryRegionOracle::new();
    oracle.insert(w.a.clone(), region("us")).unwrap();
    let status = receive_with(&w, &restricted_b(&w), Some(&oracle));
    assert_eq!(status, ReceiveStatus::Rejected(RejectCode::NotGranted));
    assert!(w.inbox.stored().unwrap().is_empty());
}

#[test]
fn an_oracle_that_does_not_know_the_peer_refuses() {
    let w = World::new();
    let oracle = spot_tenant_link::MemoryRegionOracle::new();
    let status = receive_with(&w, &restricted_b(&w), Some(&oracle));
    assert_eq!(status, ReceiveStatus::Rejected(RejectCode::NotGranted));
    assert!(w.inbox.stored().unwrap().is_empty());
}

#[test]
fn no_oracle_and_a_region_restricted_grant_still_refuses() {
    let w = World::new();
    let status = receive_with(&w, &restricted_b(&w), None);
    assert_eq!(status, ReceiveStatus::Rejected(RejectCode::NotGranted));
    assert!(w.inbox.stored().unwrap().is_empty());
}

#[test]
fn a_grant_without_a_region_requirement_ignores_the_oracle() {
    // With an oracle that knows nothing, and with none, the unrestricted grant behaves as before.
    let w = World::new();
    let oracle = spot_tenant_link::MemoryRegionOracle::new();
    assert_eq!(
        receive_with(&w, &w.link_b, Some(&oracle)),
        ReceiveStatus::Applied
    );
    let w = World::new();
    assert_eq!(receive_with(&w, &w.link_b, None), ReceiveStatus::Applied);
}
