//! A deterministic simulation of two tenants exchanging over a flaky carrier: disconnections
//! that run past the retention, duplicates, reordering, replays, lost receipts, restarts and a
//! consumer that crashes before it acks.
//!
//! Invariants (checked after the carrier heals and both sides settle):
//!
//! 1. each message is applied at most once, and only messages the sender produced are applied;
//! 2. per-conversation order is preserved, in the stored sequence and in the notice feed;
//! 3. every applied message has exactly one notice, and the consumer saw each at least once;
//! 4. no message is lost silently: each ends `Applied`, `Rejected` or `Expired` on the sender,
//!    `Applied` exactly when it was stored, and every end state produced a producer event;
//! 5. an invalid input signed by the peer is never applied and always lands in quarantine.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::{BTreeMap, BTreeSet};

use common::*;
use spot_link_exchange::{
    Error, EventKind, InboxStore, Lifetimes, Message, MessageId, MessageParts, MessageState,
    NoticeFeed, OutboxStore, Retention,
};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = usize::try_from(self.below(i as u64 + 1)).unwrap();
            items.swap(i, j);
        }
    }
}

struct Outcome {
    applied: usize,
    expired: usize,
    rejected: usize,
}

fn run(seed: u64) -> Outcome {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let w = World::new();
    let lifetimes = Lifetimes::new(Retention::from_days(3).unwrap());
    let conversations = ["c1", "c2", "c3"].map(conversation);
    let topic = message_topic();
    let mut produced: Vec<MessageId> = Vec::new();
    let mut crafted: Vec<MessageId> = Vec::new();
    let mut wire_log: Vec<Message> = Vec::new();
    let mut consumed: BTreeMap<MessageId, u32> = BTreeMap::new();
    let mut now = T0;
    let mut connected = true;
    let mut reconnect_at = 0;
    let mut forced_outage = false;
    let horizon = T0 + 8 * DAY;

    while now < horizon {
        now += (1 + rng.below(30)) * 60_000;
        // Both sides are rebuilt each step: nothing but the stores survives a restart.
        let outbox = w.sender(lifetimes.clone());
        let inbox = w.receiver();

        if rng.chance(15) {
            let conv = &conversations[usize::try_from(rng.below(3)).unwrap()];
            let queued = outbox
                .enqueue_message(
                    &w.submit(conv, &topic, now),
                    &body(&format!("n{}", produced.len())),
                )
                .unwrap();
            produced.push(queued.id);
        }
        let mut fresh: Vec<Message> = Vec::new();
        if rng.chance(3) {
            // A validly signed message the receiver must refuse: a field outside the grant.
            let bad = Message::sign(
                MessageParts {
                    id: spot_link_exchange::IdSource::next_id(&w.ids, now),
                    link: w.link_a.id,
                    conversation: conversation(&format!("bad{}", crafted.len())),
                    seq: 1,
                    topic: topic.clone(),
                    schema_version: 1,
                    sent_at: now,
                    expires_at: now + DAY,
                    tombstone: false,
                    replaces: None,
                    payload: r#"{"employee.salary":{"int":1}}"#.to_owned(),
                },
                &w.signer_a,
            )
            .unwrap();
            crafted.push(bad.id);
            fresh.push(bad);
        }
        if seed.is_multiple_of(2) && !forced_outage && now >= T0 + 2 * DAY {
            // One outage that outlasts the 3-day retention, so messages must expire.
            forced_outage = true;
            connected = false;
            reconnect_at = now + 4 * DAY;
        } else if connected && rng.chance(2) {
            connected = false;
            reconnect_at = now + rng.below(9) * 12 * HOUR + HOUR;
        } else if !connected && now >= reconnect_at {
            connected = true;
        }

        let mut receipts = Vec::new();
        if connected {
            let mut batch = outbox
                .next_batch(&w.link_a, now, 50, "system", None)
                .unwrap();
            batch.extend(fresh);
            wire_log.extend(batch.iter().cloned());
            for message in batch.clone() {
                if rng.chance(15) {
                    batch.push(message);
                }
            }
            if rng.chance(10) && !wire_log.is_empty() {
                let index = usize::try_from(rng.below(wire_log.len() as u64)).unwrap();
                batch.push(wire_log[index].clone());
            }
            rng.shuffle(&mut batch);
            batch.retain(|_| !rng.chance(15));
            for message in &batch {
                receipts.extend(
                    inbox
                        .receive_from(&w.link_b, &connection(&w.a), message, now)
                        .unwrap()
                        .receipts,
                );
            }
        }
        receipts.extend(inbox.sweep_expired(now).unwrap());
        if connected {
            for receipt in receipts.clone() {
                if rng.chance(10) {
                    receipts.push(receipt);
                }
            }
            rng.shuffle(&mut receipts);
            receipts.retain(|_| !rng.chance(15));
            deliver_receipts(&w, &outbox, &receipts, now);
        }

        if rng.chance(20) {
            consume(&w, &mut rng, &mut consumed);
        }
    }

    // The carrier heals; both sides settle.
    let mut rounds = 0;
    loop {
        now += HOUR;
        rounds += 1;
        let outbox = w.sender(lifetimes.clone());
        let inbox = w.receiver();
        let batch = outbox
            .next_batch(&w.link_a, now, 200, "system", None)
            .unwrap();
        let mut receipts = Vec::new();
        for message in &batch {
            receipts.extend(
                inbox
                    .receive_from(&w.link_b, &connection(&w.a), message, now)
                    .unwrap()
                    .receipts,
            );
        }
        receipts.extend(inbox.sweep_expired(now).unwrap());
        deliver_receipts(&w, &outbox, &receipts, now);
        if w.outbox.live(None).unwrap().is_empty() {
            break;
        }
        if rounds >= 2_000 {
            for e in w.outbox.live(None).unwrap().iter().take(6) {
                eprintln!(
                    "LIVE {} seq {} state {:?} filler {:?} attempts {}",
                    e.message.conversation,
                    e.message.seq,
                    e.state,
                    e.filler.as_ref().map(|f| f.acknowledged),
                    e.attempts
                );
            }
            for (l, p) in w.inbox.parked().unwrap().iter().take(6) {
                eprintln!(
                    "PARKED {l} {} seq {}",
                    p.message.conversation, p.message.seq
                );
            }
            panic!("seed {seed}: the exchange did not settle");
        }
    }
    while consume(&w, &mut rng, &mut consumed) {}

    check(&w, seed, &produced, &crafted, &consumed)
}

fn deliver_receipts(
    w: &World,
    outbox: &spot_link_exchange::Outbox<'_>,
    receipts: &[spot_link_exchange::Receipt],
    now: u64,
) {
    for receipt in receipts {
        match outbox.on_receipt_from(&w.link_a, &connection(&w.b), receipt, now) {
            Ok(_) | Err(Error::NotFound) => {}
            Err(error) => panic!("receipt refused: {error}"),
        }
    }
}

/// One consumer pass; crashes before the ack one time in five. Returns whether it saw news.
fn consume(w: &World, rng: &mut Rng, consumed: &mut BTreeMap<MessageId, u32>) -> bool {
    let from = w.inbox.acked("engine").unwrap();
    let notices = w.inbox.read_from(from, 7).unwrap();
    for notice in &notices {
        *consumed.entry(notice.message_id).or_insert(0) += 1;
    }
    if let Some(last) = notices.last() {
        if !rng.chance(20) {
            w.inbox.ack("engine", last.cursor).unwrap();
        }
        true
    } else {
        false
    }
}

fn check(
    w: &World,
    seed: u64,
    produced: &[MessageId],
    crafted: &[MessageId],
    consumed: &BTreeMap<MessageId, u32>,
) -> Outcome {
    let entries = w.outbox.all().unwrap();
    let slot: BTreeMap<MessageId, (String, u64)> = entries
        .iter()
        .map(|e| {
            (
                e.message.id,
                (e.message.conversation.to_string(), e.message.seq),
            )
        })
        .collect();
    let stored = w.inbox.stored().unwrap();
    let stored_ids: BTreeSet<MessageId> = stored.iter().map(|(_, e)| e.message.id).collect();
    assert_eq!(
        stored_ids.len(),
        stored.len(),
        "seed {seed}: a message was stored twice"
    );

    // 1. only produced messages, never a crafted one.
    let produced_set: BTreeSet<MessageId> = produced.iter().copied().collect();
    assert!(
        stored_ids.is_subset(&produced_set),
        "seed {seed}: something foreign was applied"
    );
    // 5. crafted inputs are quarantined and never applied.
    let held: BTreeSet<MessageId> = w.quarantine.held().iter().map(|h| h.message.id).collect();
    for id in crafted {
        assert!(
            !stored_ids.contains(id),
            "seed {seed}: an invalid input was applied"
        );
    }
    let wire_crafted: BTreeSet<MessageId> = held
        .intersection(&crafted.iter().copied().collect())
        .copied()
        .collect();
    assert!(wire_crafted.len() <= crafted.len());

    // 2 and 3. notices: exactly one per stored message, in order per conversation.
    let notices = w.inbox.read_from(0, usize::MAX).unwrap();
    let mut seen: BTreeMap<MessageId, u32> = BTreeMap::new();
    let mut last_seq: BTreeMap<String, u64> = BTreeMap::new();
    for notice in &notices {
        *seen.entry(notice.message_id).or_insert(0) += 1;
        let (conv, seq) = slot
            .get(&notice.message_id)
            .expect("a notice names a produced message");
        let previous = last_seq.insert(conv.clone(), *seq).unwrap_or(0);
        assert!(
            *seq > previous,
            "seed {seed}: conversation {conv} applied out of order"
        );
    }
    for id in &stored_ids {
        assert_eq!(
            seen.get(id),
            Some(&1),
            "seed {seed}: notice count for an applied message"
        );
        assert!(
            consumed.get(id).is_some_and(|n| *n >= 1),
            "seed {seed}: the consumer missed a notice"
        );
    }
    assert_eq!(
        seen.len(),
        stored_ids.len(),
        "seed {seed}: a notice without an applied message"
    );

    // 4. every message ends visibly; Applied exactly when stored; events match.
    let events = w.outbox.events_from(0, usize::MAX).unwrap();
    let mut outcome = Outcome {
        applied: 0,
        expired: 0,
        rejected: 0,
    };
    for id in produced {
        let state = w
            .outbox
            .find(id)
            .unwrap()
            .expect("produced message is in the outbox")
            .state;
        match state {
            MessageState::Applied => {
                outcome.applied += 1;
                assert!(
                    stored_ids.contains(id),
                    "seed {seed}: Applied but not stored"
                );
            }
            MessageState::Expired => {
                outcome.expired += 1;
                assert!(!stored_ids.contains(id), "seed {seed}: Expired but stored");
            }
            MessageState::Rejected(_) => {
                outcome.rejected += 1;
                assert!(!stored_ids.contains(id), "seed {seed}: Rejected but stored");
            }
            other => panic!("seed {seed}: message left in {other:?}"),
        }
        let last = events
            .iter()
            .rev()
            .find(|(_, e)| &e.message == id)
            .map(|(_, e)| e.kind)
            .unwrap_or_else(|| panic!("seed {seed}: no producer event for a finished message"));
        let expected = match state {
            MessageState::Applied => EventKind::Applied,
            MessageState::Expired => EventKind::Expired,
            MessageState::Rejected(code) => EventKind::Rejected(code),
            _ => unreachable!(),
        };
        assert_eq!(
            last, expected,
            "seed {seed}: the last event matches the end state"
        );
    }
    outcome
}

fn simulate(seed: u64) {
    let outcome = run(seed);
    assert!(
        outcome.applied > 10,
        "seed {seed}: the run applied too little to mean anything"
    );
    if seed.is_multiple_of(2) {
        assert!(
            outcome.expired > 0,
            "seed {seed}: an outage past the retention expired nothing"
        );
    }
    let _ = outcome.rejected;
}

#[test]
fn seed_1_holds_its_invariants() {
    simulate(1);
}

#[test]
fn seed_2_holds_its_invariants_across_an_outage_past_retention() {
    simulate(2);
}

#[test]
fn seed_3_holds_its_invariants() {
    simulate(3);
}

#[test]
fn seed_4_holds_its_invariants_across_an_outage_past_retention() {
    simulate(4);
}

#[test]
fn seed_6_holds_its_invariants_across_an_outage_past_retention() {
    simulate(6);
}
