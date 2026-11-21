//! The real `Outbox` and `Inbox` of `spot-link-exchange` over the PostgreSQL stores: a full round
//! trip with a gap, a restart in the middle of a batch, and the retention sweep.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::*;
use spot_link_exchange::{
    EventKind, Inbox, InboxStore, Lifetimes, MemoryQuarantine, Message, MessageState, NoticeFeed,
    Outbox, OutboxStore, ReceiveStatus, Retention, Submit,
};
use spot_link_exchange_pg::{PgExchange, RetentionSweep, SweepConfig, SweepHost};
use spot_tenant_link::{ExchangePayload, FieldName, FieldValue};

fn payload(text: &str) -> ExchangePayload {
    let mut p = ExchangePayload::new();
    p.insert(
        FieldName::new("message.body").unwrap(),
        FieldValue::Text(text.to_owned()),
    );
    p
}

fn enqueue(w: &World, ex: &PgExchange, conversation_name: &str, text: &str, now: u64, days: u32) {
    let store = ex.outbox();
    let outbox = Outbox::new(
        &store,
        &w.audit_a,
        &w.signer_a,
        &w.ids,
        Lifetimes::new(Retention::from_days(days).unwrap()),
    );
    let conv = conversation(conversation_name);
    let topic = message_topic();
    outbox
        .enqueue_message(
            &Submit {
                link: &w.link_a,
                conversation: &conv,
                topic: &topic,
                schema_version: 1,
                now,
                actor: "user:alice",
                oracle: None,
            },
            &payload(text),
        )
        .unwrap();
}

fn sender<'a>(w: &'a World, store: &'a dyn OutboxStore) -> Outbox<'a> {
    Outbox::new(
        store,
        &w.audit_a,
        &w.signer_a,
        &w.ids,
        Lifetimes::new(Retention::from_days(3).unwrap()),
    )
}

#[test]
fn a_conversation_arriving_out_of_order_is_applied_in_order_and_confirmed() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let (a, b) = (e.exchange(1), e.exchange(2));
    let quarantine = MemoryQuarantine::new();
    for n in 1..=3 {
        enqueue(&w, &a, "orders", &format!("m{n}"), T0 + n, 3);
    }
    let a_store = a.outbox();
    let outbox = sender(&w, &a_store);
    let batch = outbox
        .next_batch(&w.link_a, T0 + 100, 10, "system", None)
        .unwrap();
    assert_eq!(
        batch.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    // Deliver 3, 1, 2: 3 parks, 1 applies, 2 applies and releases 3.
    let b_store = b.inbox();
    let inbox = Inbox::new(&b_store, &w.audit_b, &quarantine, &w.signer_b);
    let third = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[2], T0 + 200)
        .unwrap();
    assert_eq!(third.status, ReceiveStatus::Parked);
    assert!(b.notices().read_from(0, 10).unwrap().is_empty());
    let first = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[0], T0 + 201)
        .unwrap();
    assert_eq!(first.status, ReceiveStatus::Applied);
    let second = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[1], T0 + 202)
        .unwrap();
    assert_eq!(second.status, ReceiveStatus::Applied);
    assert_eq!(
        second.receipts.len(),
        2,
        "the receipt of the released message rides along"
    );

    let notices = b.notices().read_from(0, 10).unwrap();
    assert_eq!(
        notices.iter().map(|n| n.message_id).collect::<Vec<_>>(),
        batch.iter().map(|m| m.id).collect::<Vec<_>>(),
        "notices come in conversation order"
    );
    assert_eq!(
        notices.iter().map(|n| n.cursor).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );

    // A repeat is answered, not applied twice.
    let repeat = inbox
        .receive_from(&w.link_b, &connection(&w.a), &batch[1], T0 + 300)
        .unwrap();
    assert_eq!(repeat.status, ReceiveStatus::Duplicate);
    assert_eq!(b.notices().read_from(0, 10).unwrap().len(), 3);

    // The receipts close the loop on the sender.
    for receipt in first
        .receipts
        .iter()
        .chain(&second.receipts)
        .chain(&third.receipts)
    {
        outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), receipt, T0 + 400)
            .unwrap();
    }
    let states: Vec<_> = a_store.live(None).unwrap();
    assert!(states.is_empty(), "nothing is left to deliver: {states:?}");
    let events = outbox.events_from(0, 10).unwrap();
    assert_eq!(events.len(), 3);
    assert!(events.iter().all(|(_, e)| e.kind == EventKind::Applied));
}

#[test]
fn a_restart_in_the_middle_of_a_batch_redelivers_and_dedupes() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let (a, b) = (e.exchange(1), e.exchange(2));
    let quarantine = MemoryQuarantine::new();
    for n in 1..=5 {
        enqueue(&w, &a, "orders", &format!("m{n}"), T0 + n, 3);
    }
    let a_store = a.outbox();
    let batch: Vec<Message> = sender(&w, &a_store)
        .next_batch(&w.link_a, T0 + 100, 10, "system", None)
        .unwrap();

    // The receiver takes two and dies.
    {
        let store = b.inbox();
        let inbox = Inbox::new(&store, &w.audit_b, &quarantine, &w.signer_b);
        for m in &batch[..2] {
            assert_eq!(
                inbox
                    .receive_from(&w.link_b, &connection(&w.a), m, T0 + 200)
                    .unwrap()
                    .status,
                ReceiveStatus::Applied
            );
        }
    }
    // A new process, a new pool: the sender hands the whole batch over again.
    let b2 = e.restarted(2);
    let store = b2.inbox();
    let inbox = Inbox::new(&store, &w.audit_b, &quarantine, &w.signer_b);
    let statuses: Vec<_> = batch
        .iter()
        .map(|m| {
            inbox
                .receive_from(&w.link_b, &connection(&w.a), m, T0 + 300)
                .unwrap()
                .status
        })
        .collect();
    assert_eq!(
        statuses,
        vec![
            ReceiveStatus::Duplicate,
            ReceiveStatus::Duplicate,
            ReceiveStatus::Applied,
            ReceiveStatus::Applied,
            ReceiveStatus::Applied
        ]
    );
    assert_eq!(e.count(2, "spot_link_inbox"), 5);
    let cursors: Vec<_> = b2
        .notices()
        .read_from(0, 100)
        .unwrap()
        .into_iter()
        .map(|n| n.cursor)
        .collect();
    assert_eq!(
        cursors,
        vec![1, 2, 3, 4, 5],
        "one notice per message, however often it came"
    );
}

fn sweep_for(
    w: &World,
    ex: &PgExchange,
    config: SweepConfig,
) -> (RetentionSweep, Arc<spot_tenant_link::MemoryAuditLog>) {
    let audit = Arc::new(spot_tenant_link::MemoryAuditLog::new());
    let signer = Arc::new(spot_tenant_link::SeedSigner::from_seed(
        [1u8; 32],
        w.a.clone(),
    ));
    let host = SweepHost {
        audit: audit.clone(),
        signer,
        ids: Arc::new(Counter::default()),
        quarantine: Arc::new(MemoryQuarantine::new()),
        lifetimes: Lifetimes::new(Retention::from_days(1).unwrap()),
    };
    (RetentionSweep::new(ex.clone(), host, config), audit)
}

#[test]
fn the_sweep_expires_what_never_got_through_and_notifies_the_producer() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let a = e.exchange(1);
    enqueue(&w, &a, "orders", "never delivered", T0, 1);
    let (sweep, _) = sweep_for(&w, &a, SweepConfig::default());

    // Before the lifetime ends nothing happens.
    assert_eq!(sweep.run(T0 + DAY / 2).unwrap().expired, 0);
    // After it, the message is Expired and the producer has an event.
    let report = sweep.run(T0 + 2 * DAY).unwrap();
    assert_eq!(report.expired, 1);
    let store = a.outbox();
    let entry = store.live(None).unwrap().remove(0);
    assert_eq!(entry.state, MessageState::Expired);
    assert!(entry.filler.is_some(), "a tombstone keeps the slot");
    let events = store.events_from(0, 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1.kind, EventKind::Expired);
    // Running again does not expire it twice.
    assert_eq!(sweep.run(T0 + 3 * DAY).unwrap().expired, 0);
    assert_eq!(store.events_from(0, 10).unwrap().len(), 1);
}

#[test]
fn the_sweep_purges_delivered_payloads_after_the_grace_and_keeps_the_metadata() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let (a, b) = (e.exchange(1), e.exchange(2));
    let quarantine = MemoryQuarantine::new();
    enqueue(&w, &a, "orders", "secret terms", T0, 30);
    let a_store = a.outbox();
    let outbox = sender(&w, &a_store);
    let message = outbox
        .next_batch(&w.link_a, T0 + 10, 10, "system", None)
        .unwrap()
        .remove(0);
    let b_store = b.inbox();
    let inbox = Inbox::new(&b_store, &w.audit_b, &quarantine, &w.signer_b);
    let outcome = inbox
        .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 20)
        .unwrap();
    for receipt in &outcome.receipts {
        outbox
            .on_receipt_from(&w.link_a, &connection(&w.b), receipt, T0 + 30)
            .unwrap();
    }
    let id = message.id;
    let config = SweepConfig {
        outbox_grace_ms: 7 * DAY,
        inbox_keep_ms: Some(30 * DAY),
        batch: 100,
    };
    let (sweep, _) = sweep_for(&w, &a, config);

    // Inside the grace period the payload stays.
    assert_eq!(sweep.run(T0 + 6 * DAY).unwrap().outbox_purged, 0);
    assert_ne!(a_store.find(&id).unwrap().unwrap().message.payload, "{}");
    // After it, the payload is emptied and the state, ids and times remain.
    let report = sweep.run(T0 + 8 * DAY).unwrap();
    assert_eq!(report.outbox_purged, 1);
    let kept = a_store.find(&id).unwrap().unwrap();
    assert_eq!(kept.message.payload, "{}");
    assert_eq!(kept.state, MessageState::Applied);
    assert_eq!(kept.message.id, id);
    // Purging is once only.
    assert_eq!(sweep.run(T0 + 9 * DAY).unwrap().outbox_purged, 0);

    // The receiver keeps the payload until its own keep period passes (this sweep runs on the
    // sender's tenant, so use the receiver's exchange for the inbox side).
    let (b_sweep, _) = sweep_for(&w, &b, config);
    assert_eq!(b_sweep.run(T0 + 10 * DAY).unwrap().inbox_purged, 0);
    assert_eq!(b_sweep.run(T0 + 31 * DAY).unwrap().inbox_purged, 1);
    let entry = b_store.entry(&w.link_a.id, &id).unwrap().unwrap();
    assert_eq!(entry.message.payload, "{}");
    assert_eq!(e.count(2, "spot_link_notice"), 1, "the notice is untouched");
}
