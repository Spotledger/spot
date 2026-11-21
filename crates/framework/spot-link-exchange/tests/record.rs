//! Record topics: the envelope wraps the replication wire image and is applied through the
//! one apply path, never by this crate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;

use common::*;
use spot_link_exchange::{
    Envelope, Lifetimes, MessageState, NoticeFeed, ReceiptEffect, ReceiveStatus, RejectCode,
};
use spot_replica_apply::{DocPayload, RecordPayload, WireValue};
use spot_sync_protocol::{RecordName, SourceId};

fn invoice(extra_secret: bool) -> Envelope {
    let mut fields = BTreeMap::new();
    fields.insert("customer".to_owned(), WireValue::Text("Acme".to_owned()));
    fields.insert("total".to_owned(), WireValue::Decimal("1250.50".to_owned()));
    if extra_secret {
        fields.insert(
            "internal_margin".to_owned(),
            WireValue::Decimal("0.31".to_owned()),
        );
    }
    Envelope {
        name: RecordName::new("INV-0001").unwrap(),
        version: 1,
        record: RecordPayload {
            document: DocPayload {
                entity: "invoice".to_owned(),
                id: "01890a5d-ac96-774b-bcce-b302099a8057".to_owned(),
                fields,
                children: BTreeMap::new(),
            },
            entries: Vec::new(),
            meta_version: 1,
        },
    }
}

#[test]
fn a_record_is_stored_pending_then_applied_with_one_notice() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("inv");
    let topic = record_topic();
    let queued = outbox
        .enqueue_record(&w.submit(&conv, &topic, T0), invoice(true))
        .unwrap();
    assert_eq!(queued.withheld, vec!["internal_margin".to_owned()]);
    let batch = outbox
        .next_batch(&w.link_a, T0 + 1, 10, "system", None)
        .unwrap();
    assert!(!batch[0].payload.contains("internal_margin"));
    assert!(!batch[0].payload.contains("0.31"));

    let inbox = w.receiver();
    let outcome = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], T0 + 2)
        .unwrap();
    assert_eq!(outcome.status, ReceiveStatus::PendingApply);
    assert!(
        w.inbox.read_from(0, 10).unwrap().is_empty(),
        "no notice before the apply"
    );
    let effect = outbox
        .on_receipt_from(&w.link_a, &connection(&w.b), &outcome.receipts[0], T0 + 3)
        .unwrap();
    assert_eq!(effect, ReceiptEffect::Moved(MessageState::Acknowledged));

    // The apply path takes the pending record in the replica-apply shape.
    let pending = inbox.pending_records(10).unwrap();
    assert_eq!(pending.len(), 1);
    let source = SourceId::new("site-b").unwrap();
    let record = inbox.received_record(&pending[0].entry, &source).unwrap();
    assert_eq!(record.name.as_str(), "INV-0001");
    assert_eq!(record.version, 1);
    assert_eq!(record.seq, 1);
    assert_eq!(record.conversation.as_deref(), Some("inv"));

    let receipt = inbox
        .record_applied(&w.link_b, &batch[0].id, T0 + 4)
        .unwrap()
        .unwrap();
    assert!(
        inbox
            .record_applied(&w.link_b, &batch[0].id, T0 + 5)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        w.inbox.read_from(0, 10).unwrap().len(),
        1,
        "exactly one notice"
    );
    outbox
        .on_receipt_from(&w.link_a, &connection(&w.b), &receipt, T0 + 6)
        .unwrap();
    assert_eq!(outbox.state(&queued.id).unwrap(), MessageState::Applied);
    assert!(inbox.pending_records(10).unwrap().is_empty());
}

#[test]
fn a_record_the_apply_path_refuses_is_rejected_and_quarantined() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("inv");
    let topic = record_topic();
    let queued = outbox
        .enqueue_record(&w.submit(&conv, &topic, T0), invoice(false))
        .unwrap();
    let batch = outbox
        .next_batch(&w.link_a, T0 + 1, 10, "system", None)
        .unwrap();
    let inbox = w.receiver();
    inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], T0 + 2)
        .unwrap();
    let receipt = inbox
        .record_refused(&w.link_b, &batch[0].id, T0 + 3)
        .unwrap()
        .unwrap();
    outbox
        .on_receipt_from(&w.link_a, &connection(&w.b), &receipt, T0 + 4)
        .unwrap();
    assert_eq!(
        outbox.state(&queued.id).unwrap(),
        MessageState::Rejected(RejectCode::PayloadInvalid)
    );
    assert!(w.inbox.read_from(0, 10).unwrap().is_empty());
    assert_eq!(w.quarantine.held().len(), 1);
}

#[test]
fn a_record_with_a_field_outside_the_grant_is_refused_by_the_receiver() {
    let w = World::new();
    let topic = record_topic();
    // A peer that skips its own filter: signed, but the image carries a field the grant lacks.
    let payload = invoice(true).encode().unwrap();
    let message = spot_link_exchange::Message::sign(
        spot_link_exchange::MessageParts {
            id: spot_link_exchange::IdSource::next_id(&w.ids, T0),
            link: w.link_a.id,
            conversation: conversation("inv"),
            seq: 1,
            topic,
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload,
        },
        &w.signer_a,
    )
    .unwrap();
    let outcome = w
        .receiver()
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 1)
        .unwrap();
    assert_eq!(
        outcome.status,
        ReceiveStatus::Rejected(RejectCode::PayloadInvalid)
    );
    assert!(w.inbox.stored().unwrap().is_empty());
}

#[test]
fn a_record_topic_is_not_a_message_topic() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("inv");
    let topic = record_topic();
    let error = outbox
        .enqueue_message(&w.submit(&conv, &topic, T0), &body("x"))
        .unwrap_err();
    assert_eq!(error.code(), "link_exchange.wrong_kind");
}
