//! A deterministic simulation of two tenants, each with its own engine, talking over a flaky
//! carrier: kill-restarts of either side at random points (including inside the engine's step),
//! duplicated, reordered and lost deliveries, lost receipts, an outage that outlasts the wait
//! limit (the timeout branch), and rogue out-of-order messages.
//!
//! Invariants, checked once the carrier heals and both sides settle:
//!
//! 1. each side's request reaches the same outcome state as the other, or one of them says
//!    `needs attention` (the requester that gave up while the approver answered);
//! 2. no side effect is performed twice, and each side's request is started once;
//! 3. each real message is applied at most once, and no rogue message is applied or left unheld;
//! 4. the requester's request is queued once, whatever was replayed.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::*;
use spot_bpm_contract::OutcomeKind;
use spot_bpm_conversations::{ConversationStore, Direction};
use spot_bpm_engine::ScriptedFaults;
use spot_link_exchange::{MessageId, Outbox};

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

/// One carrier pass from `from` to `to`: duplicates, reordering, loss and lost receipts.
fn pass(rng: &mut Rng, from: &Side, to: &Booted<'_>, now: u64) {
    let ids = Counter::default();
    let outbox = Outbox::new(
        &*from.outbox,
        &*from.audit,
        &*from.signer,
        &ids,
        from.lifetimes.clone(),
    );
    let mut batch = outbox
        .next_batch(&from.link(), now, 50, "system", None)
        .unwrap();
    for message in batch.clone() {
        if rng.chance(25) {
            batch.push(message);
        }
    }
    rng.shuffle(&mut batch);
    for message in batch {
        if rng.chance(15) {
            continue; // lost on the way
        }
        deliver_with(to, from, &message, now, !rng.chance(20));
    }
}

/// Runs one side's turn: a restart (a rebuild from the durable state, sometimes with a crash
/// injected inside the engine) and then its driver and engine.
fn turn(rng: &mut Rng, side: &Side, now: u64) {
    if rng.chance(15) {
        let faults = Arc::new(ScriptedFaults::after(rng.below(14)));
        let crashing = side.boot_faulty(faults);
        let _ = crashing.advance_result(now);
    }
    side.boot().advance(now);
}

/// What a run did, so the test can see the simulation exercised what it claims to.
struct Stats {
    expired: bool,
    rogue: usize,
}

fn run(seed: u64) -> Stats {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let w = World::new();
    w.a.boot().start_request("req1", "REF-1", T0);

    let mut now = T0;
    let horizon = T0 + 5 * HOUR;
    let mut connected = true;
    let mut reconnect_at = 0;
    let mut rogue: Vec<MessageId> = Vec::new();
    let mut rogue_sent = 0;

    while now < horizon {
        now += (1 + rng.below(40)) * 10 * SECOND;
        if connected && rng.chance(4) {
            connected = false;
            reconnect_at = now + rng.below(4) * HOUR + 10 * 60 * SECOND;
        } else if !connected && now >= reconnect_at {
            connected = true;
        }
        if rng.chance(50) {
            turn(&mut rng, &w.a, now);
        }
        if rng.chance(50) {
            turn(&mut rng, &w.b, now);
        }
        // A misbehaving requester repeats its request while the conversation is open.
        if rogue_sent < 2 && rng.chance(8) {
            let records = w.a.conversations.records();
            if let Some(record) = records.first() {
                rogue.push(rogue_send(
                    &w.a,
                    &record.conversation,
                    "bpm.request",
                    1,
                    Some("request_approve@1/request"),
                    &[("reference", text("R2")), ("amount_minor", int(1))],
                    now + rogue_sent,
                ));
                rogue_sent += 1;
            }
        }
        if connected {
            pass(&mut rng, &w.a, &w.b.boot(), now);
            pass(&mut rng, &w.b, &w.a.boot(), now);
        }
    }

    // Heal: a reliable carrier, no more crashes, enough time for every resend.
    for _ in 0..40 {
        now += 2 * 60 * SECOND;
        w.a.boot().advance(now);
        w.b.boot().advance(now);
        carry(&w.a, &w.b.boot(), now);
        carry(&w.b, &w.a.boot(), now);
    }
    w.a.boot().advance(now);
    w.b.boot().advance(now);

    let label = format!("seed {seed}");
    let (link, conversation) = conversation_for_case(&w.a, "req1");
    let a =
        w.a.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();
    let b =
        w.b.conversations
            .load(&link, &conversation)
            .unwrap()
            .unwrap();

    // 1. The same outcome, or a side that says it needs attention.
    // Only an ending the contract itself calls "needs attention" excuses a mismatch; the
    // marker alone (which any refusal can raise) does not.
    let needs_attention = |r: &spot_bpm_conversations::ConversationRecord| {
        r.ending
            .as_ref()
            .is_some_and(|e| e.kind == OutcomeKind::NeedsAttention)
    };
    let (ending_a, ending_b) = (a.ending.clone(), b.ending.clone());
    assert!(
        ending_a.is_some(),
        "{label}: the requester's conversation ended"
    );
    assert!(
        ending_b.is_some(),
        "{label}: the approver's conversation ended"
    );
    assert!(
        ending_a.as_ref().map(|e| &e.outcome) == ending_b.as_ref().map(|e| &e.outcome)
            || needs_attention(&a)
            || needs_attention(&b),
        "{label}: {ending_a:?} vs {ending_b:?}"
    );
    assert_eq!(
        w.a.boot().case("req1").run.status,
        spot_dag_exec::RunStatus::Completed,
        "{label}"
    );

    // 2. No effect twice; each request started once.
    for side in [&w.a, &w.b] {
        assert!(side.ledger.duplicates().is_empty(), "{label}");
    }
    assert_eq!(w.a.ledger.total(), 1, "{label}: one requester effect");
    assert_eq!(
        w.b.ledger.total(),
        2,
        "{label}: the approver's two effects, once"
    );
    let cases_b: BTreeSet<String> =
        w.b.ledger
            .keys()
            .iter()
            .map(|key| key.split(':').next().unwrap().to_owned())
            .collect();
    assert_eq!(cases_b.len(), 1, "{label}: one approver request");

    // 3. Applied at most once; rogue messages held, never applied.
    let stored_b = w.b.inbox.stored().unwrap();
    let ids_b: BTreeSet<_> = stored_b.iter().map(|(_, e)| e.message.id).collect();
    assert_eq!(ids_b.len(), stored_b.len(), "{label}");
    let real_b = stored_b
        .iter()
        .filter(|(_, e)| !e.message.tombstone)
        .count();
    assert_eq!(
        real_b, 1,
        "{label}: exactly the request was applied at the approver"
    );
    for id in &rogue {
        assert!(!ids_b.contains(id), "{label}: a rogue message was applied");
        let held = w.b.quarantine.held();
        assert!(
            held.iter().any(|h| &h.message.id == id),
            "{label}: a rogue message was not held"
        );
    }
    let held_ids: BTreeSet<_> = w.b.quarantine.held().iter().map(|h| h.message.id).collect();
    assert_eq!(
        held_ids.len(),
        w.b.quarantine.held().len(),
        "{label}: held once each"
    );
    let received_a = a
        .events
        .iter()
        .filter(|e| e.direction == Direction::Received)
        .count();
    assert!(received_a <= 1, "{label}: at most one answer was taken in");

    // 4. One request queued, however often it was replayed.
    let requests: Vec<_> =
        w.a.outbox
            .all()
            .unwrap()
            .into_iter()
            .filter(|e| !e.message.tombstone && rogue.iter().all(|r| *r != e.message.id))
            .collect();
    assert_eq!(requests.len(), 1, "{label}: one request queued");
    Stats {
        expired: a.ending.as_ref().is_some_and(|e| e.outcome == "expired"),
        rogue: rogue.len(),
    }
}

#[test]
fn two_tenants_complete_a_conversation_through_restarts_duplicates_reordering_and_outages() {
    let mut expired = 0;
    let mut approved = 0;
    let mut rogue = 0;
    for seed in 1..=24 {
        let stats = run(seed);
        if stats.expired {
            expired += 1;
        } else {
            approved += 1;
        }
        rogue += stats.rogue;
    }
    assert!(approved > 0, "some runs complete normally");
    assert!(
        expired > 0,
        "some runs outlast the wait limit and take the timeout branch"
    );
    assert!(rogue > 0, "some runs send an out-of-order message");
}
