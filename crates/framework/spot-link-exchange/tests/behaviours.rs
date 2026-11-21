//! WP-CTL-10.T12: the behaviours of the exchange the T11 property suite left out: an inbound flood
//! stops at the configured bound and the drops are counted, schema versions inside the grant's
//! range are accepted and outside refused, a stuck message is retried with growing intervals and
//! then ends instead of being retried forever, a crash inside a store operation leaves no
//! half-recorded message after restart, and every transition leaves exactly one notice or event.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::*;
use spot_link_exchange::{
    Conversation, Error, EventKind, InboxConfig, InboxStore, Lifetimes, MemoryOutbox, Message,
    MessageId, MessageParts, MessageState, NoticeFeed, Outbox, OutboxConfig, OutboxEntry,
    OutboxStore, ProducerEvent, ReceiptOutcome, ReceiveStatus, RejectCode, Retention,
};
use spot_tenant_link::LinkId;

fn signed(
    w: &World,
    entropy: u8,
    conversation: &str,
    seq: u64,
    version: u32,
    text: &str,
) -> Message {
    Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, [entropy; 10]),
            link: w.link_a.id,
            conversation: spot_link_exchange::Conversation::new(conversation).unwrap(),
            seq,
            topic: message_topic(),
            schema_version: version,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: format!(r#"{{"message.body":{{"text":"{text}"}}}}"#),
        },
        &w.signer_a,
    )
    .unwrap()
}

// --- (1) an inbound flood stops at the bound and the drops are counted ---------------------

#[test]
fn an_inbound_flood_stops_at_the_bound_and_the_drops_are_counted() {
    const CAP: usize = 5;
    const SENT: usize = 40;
    let w = World::new();
    let inbox = w.receiver().with_config(InboxConfig {
        max_parked_per_link: CAP,
        ..InboxConfig::default()
    });
    let mut parked = 0usize;
    let mut dropped = 0usize;
    for i in 0..SENT {
        // Sequence 2 of a fresh conversation: a gap at 1, so each one would be parked.
        let message = signed(
            &w,
            u8::try_from(i + 1).unwrap(),
            &format!("flood-{i}"),
            2,
            1,
            "x",
        );
        let outcome = inbox
            .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 1)
            .unwrap();
        match outcome.status {
            ReceiveStatus::Parked => parked += 1,
            ReceiveStatus::Rejected(RejectCode::CapacityExceeded) => {
                dropped += 1;
                assert_eq!(
                    outcome.receipts[0].outcome,
                    ReceiptOutcome::Rejected(RejectCode::CapacityExceeded)
                );
            }
            other => panic!("unexpected status {other:?}"),
        }
        // The store never holds more than the bound, whatever arrives.
        assert!(w.inbox.usage(&w.link_b.id).unwrap().parked <= CAP as u64);
    }
    assert_eq!(parked, CAP);
    assert_eq!(dropped, SENT - CAP);
    assert_eq!(w.inbox.usage(&w.link_b.id).unwrap().parked, CAP as u64);
    // Every drop is counted: each one is held once in the quarantine for the link.
    assert_eq!(w.quarantine.count(&w.link_b.id), dropped);
    assert!(w.inbox.stored().unwrap().is_empty());
}

// --- (2) schema versions inside the range are accepted, outside refused ----------------------

#[test]
fn schema_versions_inside_the_grants_range_are_accepted_and_outside_refused() {
    // The fixture grant accepts versions 1 to 2.
    let w = World::new();
    let inbox = w.receiver();
    for (n, version) in (1u32..=5).enumerate() {
        let message = signed(
            &w,
            u8::try_from(n + 1).unwrap(),
            &format!("v{version}"),
            1,
            version,
            "x",
        );
        let outcome = inbox
            .receive_from(&w.link_b, &connection(&w.a), &message, T0 + 1)
            .unwrap();
        if (1..=2).contains(&version) {
            assert_eq!(outcome.status, ReceiveStatus::Applied, "version {version}");
        } else {
            assert_eq!(
                outcome.status,
                ReceiveStatus::Rejected(RejectCode::VersionUnsupported),
                "version {version}"
            );
        }
    }
    // The sender refuses the same versions before sending.
    let outbox = w.sender(Lifetimes::default());
    let conv = conversation("sender-versions");
    let topic = message_topic();
    for version in 1u32..=5 {
        let mut request = w.submit(&conv, &topic, T0);
        request.schema_version = version;
        let result = outbox.enqueue_message(&request, &body("x"));
        if (1..=2).contains(&version) {
            result.unwrap();
        } else {
            assert_eq!(
                result.unwrap_err().code(),
                "link_exchange.schema_unsupported"
            );
        }
    }
}

// --- (3) a stuck message is retried with growing intervals and then ends ---------------------

fn sends_until_over(backoff: u32) -> (usize, MessageState) {
    let w = World::new();
    let lifetimes = Lifetimes::new(Retention::from_days(1).unwrap());
    let outbox = w
        .sender(lifetimes)
        .with_config(OutboxConfig {
            resend_after_ms: 60_000,
            ..OutboxConfig::default()
        })
        .with_backoff(backoff);
    let conv = conversation("stuck");
    let enq = outbox
        .enqueue_message(&w.submit(&conv, &message_topic(), T0), &body("x"))
        .unwrap();
    let mut sends = 0;
    let mut now = T0;
    // The peer never answers. Step a minute at a time for three days.
    for _ in 0..(3 * 24 * 60) {
        sends += outbox
            .next_batch(&w.link_a, now, 10, "system", None)
            .unwrap()
            .len();
        now += 60_000;
    }
    (sends, outbox.state(&enq.id).unwrap())
}

#[test]
fn a_stuck_message_is_retried_with_growing_intervals_then_ends_and_is_not_retried_forever() {
    let (fixed, fixed_state) = sends_until_over(0);
    let (backed_off, backed_state) = sends_until_over(6);
    // Both end Expired at the end of the one-day retention, not Sent forever.
    assert_eq!(fixed_state, MessageState::Expired);
    assert_eq!(backed_state, MessageState::Expired);
    // A fixed interval resends every minute for a day; the backed-off one far less, but still
    // more than once.
    assert!(fixed >= 1_000, "{fixed}");
    assert!(
        backed_off > 1 && backed_off < fixed / 10,
        "{backed_off} vs {fixed}"
    );
}

#[test]
fn the_backoff_intervals_double_up_to_the_cap() {
    let w = World::new();
    let outbox = w
        .sender(Lifetimes::default())
        .with_config(OutboxConfig {
            resend_after_ms: 1_000,
            ..OutboxConfig::default()
        })
        .with_backoff(2);
    let conv = conversation("doubling");
    outbox
        .enqueue_message(&w.submit(&conv, &message_topic(), T0), &body("x"))
        .unwrap();
    let mut sent_at = Vec::new();
    let mut now = T0;
    for _ in 0..20_000 {
        if !outbox
            .next_batch(&w.link_a, now, 10, "system", None)
            .unwrap()
            .is_empty()
        {
            sent_at.push(now);
        }
        now += 100;
        if sent_at.len() == 6 {
            break;
        }
    }
    let gaps: Vec<u64> = sent_at.windows(2).map(|p| p[1] - p[0]).collect();
    // 1 s, 2 s, then capped at 4 s.
    assert_eq!(gaps, vec![1_000, 2_000, 4_000, 4_000, 4_000]);
}

// --- (4) a crash inside a store operation leaves nothing half recorded ------------------------

/// An outbox store that fails its `crash_at`-th call, either before the operation happens or
/// after it did (the answer was lost), then keeps failing: the process is gone.
struct Crashing<'a> {
    inner: &'a MemoryOutbox,
    calls: AtomicUsize,
    crash_at: usize,
    after: bool,
}

impl Crashing<'_> {
    fn gate<T>(
        &self,
        op: impl FnOnce() -> spot_link_exchange::Result<T>,
    ) -> spot_link_exchange::Result<T> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n > self.crash_at {
            return Err(Error::StoreFailed { reason: "crashed" });
        }
        if n == self.crash_at {
            if self.after {
                let _ = op();
            }
            return Err(Error::StoreFailed { reason: "crashed" });
        }
        op()
    }
}

impl OutboxStore for Crashing<'_> {
    fn insert(&self, entry: OutboxEntry) -> spot_link_exchange::Result<()> {
        self.gate(|| self.inner.insert(entry))
    }
    fn find(&self, id: &MessageId) -> spot_link_exchange::Result<Option<OutboxEntry>> {
        self.gate(|| self.inner.find(id))
    }
    fn update(&self, entry: OutboxEntry) -> spot_link_exchange::Result<()> {
        self.gate(|| self.inner.update(entry))
    }
    fn last_seq(
        &self,
        link: &LinkId,
        conversation: &Conversation,
    ) -> spot_link_exchange::Result<u64> {
        self.gate(|| self.inner.last_seq(link, conversation))
    }
    fn live(&self, link: Option<&LinkId>) -> spot_link_exchange::Result<Vec<OutboxEntry>> {
        self.gate(|| self.inner.live(link))
    }
    fn settle(
        &self,
        entry: OutboxEntry,
        event: Option<ProducerEvent>,
    ) -> spot_link_exchange::Result<()> {
        self.gate(|| self.inner.settle(entry, event))
    }
    fn push_event(&self, event: ProducerEvent) -> spot_link_exchange::Result<()> {
        self.gate(|| self.inner.push_event(event))
    }
    fn events_from(
        &self,
        after: u64,
        limit: usize,
    ) -> spot_link_exchange::Result<Vec<(u64, ProducerEvent)>> {
        self.gate(|| self.inner.events_from(after, limit))
    }
}

/// One run: three messages, delivered and answered, with the sender's store crashing at call
/// `crash_at`. Then a restart over the persisted stores and a lossless drain.
fn crash_run(crash_at: usize, after: bool) {
    let w = World::new();
    let lifetimes = || Lifetimes::new(Retention::from_days(1).unwrap());
    let conv = conversation("crash");
    let mut ids: Vec<MessageId> = Vec::new();
    {
        let crashing = Crashing {
            inner: &w.outbox,
            calls: AtomicUsize::new(0),
            crash_at,
            after,
        };
        let outbox = Outbox::new(&crashing, &w.audit_a, &w.signer_a, &w.ids, lifetimes());
        'life: {
            for n in 0..3 {
                match outbox.enqueue_message(&w.submit(&conv, &message_topic(), T0 + n), &body("m"))
                {
                    Ok(enq) => ids.push(enq.id),
                    Err(_) => break 'life,
                }
            }
            let Ok(batch) = outbox.next_batch(&w.link_a, T0 + 10, 10, "system", None) else {
                break 'life;
            };
            for message in &batch {
                let Ok(outcome) =
                    w.receiver()
                        .receive_from(&w.link_b, &connection(&w.a), message, T0 + 11)
                else {
                    break 'life;
                };
                for receipt in &outcome.receipts {
                    if outbox
                        .on_receipt_from(&w.link_a, &connection(&w.b), receipt, T0 + 12)
                        .is_err()
                    {
                        break 'life;
                    }
                }
            }
        }
    }
    // The restart: a fresh sender over the persisted store, everything delivered losslessly.
    let outbox = Outbox::new(&w.outbox, &w.audit_a, &w.signer_a, &w.ids, lifetimes());
    let mut now = T0 + HOUR;
    for _ in 0..50 {
        let batch = outbox
            .next_batch(&w.link_a, now, 100, "system", None)
            .unwrap();
        if batch.is_empty() {
            // Nothing is due yet: let the resend interval pass.
            now += HOUR;
            if w.outbox.live(None).unwrap().is_empty() {
                break;
            }
            continue;
        }
        for message in &batch {
            let outcome = w
                .receiver()
                .receive_from(&w.link_b, &connection(&w.a), message, now)
                .unwrap();
            for receipt in &outcome.receipts {
                outbox
                    .on_receipt_from(&w.link_a, &connection(&w.b), receipt, now)
                    .unwrap();
            }
        }
        now += HOUR;
    }
    // Invariants after recovery.
    let events = w.outbox.events_from(0, 1_000).unwrap();
    let mut per_message: BTreeMap<MessageId, usize> = BTreeMap::new();
    for (_, event) in &events {
        *per_message.entry(event.message).or_default() += 1;
    }
    let applied_stored = w.inbox.stored().unwrap();
    let mut seen_ids: Vec<MessageId> = applied_stored.iter().map(|(_, e)| e.message.id).collect();
    seen_ids.sort();
    seen_ids.dedup();
    assert_eq!(
        seen_ids.len(),
        applied_stored.len(),
        "a message was stored twice (crash {crash_at}, after {after})"
    );
    for id in &ids {
        let entry = w
            .outbox
            .find(id)
            .unwrap()
            .expect("an acknowledged enqueue survives a crash");
        // No half-recorded message: a terminal entry has exactly one producer event.
        match entry.state {
            MessageState::Applied | MessageState::Expired | MessageState::Rejected(_) => {
                assert_eq!(
                    per_message.get(id).copied().unwrap_or(0),
                    1,
                    "{:?} (crash {crash_at}, after {after})",
                    entry.state
                );
            }
            other => assert!(other.is_open(), "{other:?}"),
        }
    }
    // An event never exists for a message the store does not know.
    for (_, event) in &events {
        assert!(w.outbox.find(&event.message).unwrap().is_some());
        assert!(matches!(
            event.kind,
            EventKind::Applied | EventKind::Expired | EventKind::Rejected(_)
        ));
    }
}

#[test]
fn a_crash_at_any_store_operation_leaves_no_half_recorded_message() {
    for crash_at in 1..=40 {
        crash_run(crash_at, false);
        crash_run(crash_at, true);
    }
}

// --- (5) exactly one notice or event per transition -------------------------------------------

#[test]
fn every_transition_leaves_exactly_one_notice_or_event() {
    let w = World::new();
    let outbox = w.sender(Lifetimes::new(Retention::from_days(1).unwrap()));
    let conv = conversation("notices");
    let mut ids = Vec::new();
    for n in 0..4 {
        ids.push(
            outbox
                .enqueue_message(&w.submit(&conv, &message_topic(), T0 + n), &body("m"))
                .unwrap()
                .id,
        );
    }
    let inbox = w.receiver();
    // Deliver every message twice (a duplicate), answer every receipt twice.
    for round in 0..2u64 {
        for message in outbox
            .next_batch(&w.link_a, T0 + 100 + round * 100_000, 10, "system", None)
            .unwrap()
        {
            for _ in 0..2 {
                let outcome = inbox
                    .receive_from(
                        &w.link_b,
                        &connection(&w.a),
                        &message,
                        T0 + 200 + round * 100_000,
                    )
                    .unwrap();
                for receipt in &outcome.receipts {
                    for _ in 0..2 {
                        outbox
                            .on_receipt_from(
                                &w.link_a,
                                &connection(&w.b),
                                receipt,
                                T0 + 300 + round * 100_000,
                            )
                            .unwrap();
                    }
                }
            }
        }
    }
    // The receiver: one notice per applied message, in order, none for a duplicate.
    let notices = w.inbox.read_from(0, 100).unwrap();
    assert_eq!(notices.len(), ids.len());
    let cursors: Vec<u64> = notices.iter().map(|n| n.cursor).collect();
    assert_eq!(cursors, (1..=ids.len() as u64).collect::<Vec<_>>());
    let mut notified: Vec<MessageId> = notices.iter().map(|n| n.message_id).collect();
    notified.sort();
    let mut expected = ids.clone();
    expected.sort();
    assert_eq!(notified, expected);
    // The sender: every message reached Applied and its producer event exists exactly once.
    let events = outbox.events_from(0, 100).unwrap();
    for id in &ids {
        assert_eq!(outbox.state(id).unwrap(), MessageState::Applied);
        let mine: Vec<_> = events.iter().filter(|(_, e)| e.message == *id).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].1.kind, EventKind::Applied);
    }
    // A message that expires unanswered gets exactly one event too, however often it is swept.
    let lonely = outbox
        .enqueue_message(
            &w.submit(&conversation("lonely"), &message_topic(), T0),
            &body("m"),
        )
        .unwrap()
        .id;
    for step in 0..5u64 {
        outbox.expire_due(T0 + 2 * DAY + step).unwrap();
    }
    let events = outbox.events_from(0, 100).unwrap();
    assert_eq!(
        events.iter().filter(|(_, e)| e.message == lonely).count(),
        1
    );
}
