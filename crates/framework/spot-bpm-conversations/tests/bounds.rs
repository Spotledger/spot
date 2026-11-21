//! The bounds that keep a partner from growing this tenant's state without limit: the rejection
//! cache, and the per-link limits on conversations and quarantined inputs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Barrier, Mutex};

use common::*;
use spot_bpm_conversations::record::MAX_REMEMBERED_REJECTIONS;
use spot_bpm_conversations::{
    ConversationStore, LinkLimits, MemoryConversationStore, RejectionCache, codes,
};
use spot_link_exchange::{
    Conversation, InputQuarantine, MemoryQuarantine, Message, MessageId, MessageParts, OutboxStore,
    QuarantinedInput, RejectCode,
};
use spot_tenancy::TenantId;
use spot_tenant_link::{LinkId, Party, SeedSigner, Topic};

fn conv(name: &str) -> Conversation {
    Conversation::new(name).unwrap()
}

fn link(byte: u8) -> LinkId {
    LinkId::new_v7(1_700_000_000_000, [byte; 10])
}

/// The same message id on another link is another message: the answer a redelivery gets is the
/// one *this* link decided, so the cache is keyed by `(link, id)`.
#[test]
fn a_rejection_belongs_to_its_link_as_well_as_its_message() {
    let store = MemoryConversationStore::new();
    let (here, other) = (link(1), link(2));
    let id = MessageId::new_v7(T0, [9; 10]);
    store
        .note_rejection(here, id, RejectCode::OutOfOrderExpired)
        .unwrap();
    assert_eq!(
        store.rejection(&here, &id).unwrap(),
        Some(RejectCode::OutOfOrderExpired)
    );
    assert_eq!(
        store.rejection(&other, &id).unwrap(),
        None,
        "an id says nothing across links"
    );
}

/// A peer can send bad messages for ever; the cache forgets the oldest instead of growing.
#[test]
fn the_rejection_cache_is_bounded_and_forgets_the_oldest() {
    let mut cache = RejectionCache::new(4);
    for n in 0..6u64 {
        cache.remember(
            link(1),
            MessageId::new_v7(T0 + n, [u8::try_from(n).unwrap(); 10]),
            RejectCode::PayloadInvalid,
        );
    }
    assert_eq!(cache.len(), 4);
    assert_eq!(
        cache.get(&link(1), &MessageId::new_v7(T0, [0; 10])),
        None,
        "the oldest were forgotten"
    );
    assert_eq!(
        cache.get(&link(1), &MessageId::new_v7(T0 + 5, [5; 10])),
        Some(RejectCode::PayloadInvalid),
        "the newest are remembered"
    );

    // The store every backend uses keeps the same bound, not only a whole link's worth.
    let store = MemoryConversationStore::new();
    for n in 0..u64::try_from(MAX_REMEMBERED_REJECTIONS).unwrap() + 10 {
        let mut entropy = [0u8; 10];
        entropy[2..].copy_from_slice(&n.to_be_bytes());
        store
            .note_rejection(
                link(1),
                MessageId::new_v7(T0 + n, entropy),
                RejectCode::PayloadInvalid,
            )
            .unwrap();
    }
    assert_eq!(store.remembered_rejections(), MAX_REMEMBERED_REJECTIONS);
}

/// A partner may not open conversations without limit: past the link's bound the message is
/// refused with its own code, audited, and no conversation is created for it.
#[test]
fn a_link_may_not_open_endless_conversations() {
    let mut w = World::new();
    w.b.limits = std::sync::Arc::new(LinkLimits::with(2, 512));
    let b = w.b.boot();
    // Two fresh conversations, each opened by a valid entry message.
    for (n, name) in ["c_one", "c_two"].into_iter().enumerate() {
        let at = T0 + u64::try_from(n).unwrap() * SECOND;
        rogue_send(
            &w.a,
            &conv(name),
            "bpm.request",
            1,
            Some("request_approve@1/request"),
            &[("reference", text("R1")), ("amount_minor", int(5))],
            at,
        );
    }
    let messages = carry(&w.a, &b, T0 + 3 * SECOND);
    assert_eq!(messages.len(), 2);
    assert_eq!(w.b.conversations.records().len(), 2);

    // The third would open another one: refused by the bound, not by the contract.
    let at = T0 + 4 * SECOND;
    let third = rogue_send(
        &w.a,
        &conv("c_three"),
        "bpm.request",
        1,
        Some("request_approve@1/request"),
        &[("reference", text("R1")), ("amount_minor", int(5))],
        at,
    );
    let batch = carry(&w.a, &b, T0 + 5 * SECOND);
    assert_eq!(batch.len(), 1);
    assert_eq!(
        w.a.outbox.find(&third).unwrap().unwrap().state,
        spot_link_exchange::MessageState::Rejected(RejectCode::PayloadInvalid)
    );
    assert_eq!(w.b.conversations.records().len(), 2, "no third record");
    assert_eq!(
        w.b.gate_audit.codes(),
        vec![codes::LINK_CONVERSATION_LIMIT],
        "the bound refusal is audited with its own code"
    );
    assert_eq!(w.b.gate_audit.rows()[0].message_id, third);

    // A conversation the link already holds is not affected by the bound.
    let at = T0 + 6 * SECOND;
    rogue_send(
        &w.a,
        &conv("c_one"),
        "bpm.request",
        1,
        Some("request_approve@1/request"),
        &[("reference", text("R1")), ("amount_minor", int(5))],
        at,
    );
    let refusals = w.b.gate_audit.rows().len();
    carry(&w.a, &b, T0 + 7 * SECOND);
    assert!(
        w.b.gate_audit.rows().len() > refusals,
        "the second request in an existing conversation is judged by the contract"
    );
    assert_ne!(
        w.b.gate_audit.codes().last(),
        Some(&codes::LINK_CONVERSATION_LIMIT)
    );
}

/// A partner may not fill the quarantine either: past the link's bound the refusal is still
/// answered and audited, and the input is dropped rather than held.
#[test]
fn a_link_may_not_fill_the_quarantine() {
    let mut w = World::new();
    w.b.limits = std::sync::Arc::new(LinkLimits::with(64, 1));
    let b = w.b.boot();
    for (n, name) in ["q_one", "q_two"].into_iter().enumerate() {
        let at = T0 + u64::try_from(n).unwrap() * SECOND;
        // A malformed header: refused by the contract, whatever else the message says.
        rogue_send(
            &w.a,
            &conv(name),
            "bpm.request",
            1,
            Some("nonsense"),
            &[("reference", text("R"))],
            at,
        );
    }
    let batch = carry(&w.a, &b, T0 + 3 * SECOND);
    assert_eq!(batch.len(), 2);
    assert_eq!(w.b.quarantine.held().len(), 1, "the bound holds one input");
    assert_eq!(
        w.b.gate_audit.codes(),
        vec![
            codes::HEADER_INVALID,
            codes::HEADER_INVALID,
            codes::LINK_QUARANTINE_LIMIT
        ],
        "each refusal is written down, and so is the drop the bound forced"
    );
    // The peer still learns why each message was refused.
    for message in &batch {
        assert_eq!(
            w.a.outbox.find(&message.id).unwrap().unwrap().state,
            spot_link_exchange::MessageState::Rejected(RejectCode::PayloadInvalid)
        );
    }
    assert!(
        !w.b.limits
            .may_hold(&w.b.link_id(), w.b.quarantine_port())
            .unwrap(),
        "the count is the link's, and it is the quarantine's own: the bound is spent"
    );
    assert_eq!(
        w.b.quarantine.held_count(&w.b.link_id()).unwrap(),
        1,
        "the port answers the count the bound was read from"
    );
}

/// A quarantine that cannot say how many inputs it holds (a backend that predates the count, or
/// a store that is down): `held_count` falls back to the port's default, which says so.
#[derive(Default)]
struct UncountableQuarantine {
    /// What it was asked to hold, so a case can prove nothing was.
    holds: Mutex<Vec<MessageId>>,
}

impl InputQuarantine for UncountableQuarantine {
    fn hold(&self, input: QuarantinedInput) -> spot_link_exchange::Result<()> {
        self.holds.lock().unwrap().push(input.message.id);
        Ok(())
    }
}

/// A quarantine that counts but leaves [`InputQuarantine::holds`] at the port's default, so a case
/// can prove the bound still applies to an input it cannot vouch for.
#[derive(Default)]
struct CountingQuarantine {
    held: Mutex<Vec<QuarantinedInput>>,
}

impl InputQuarantine for CountingQuarantine {
    fn hold(&self, input: QuarantinedInput) -> spot_link_exchange::Result<()> {
        let mut held = self
            .held
            .lock()
            .map_err(|_| spot_link_exchange::Error::QuarantineFailed)?;
        if !held.iter().any(|h| h.message.id == input.message.id) {
            held.push(input);
        }
        Ok(())
    }

    fn held_count(&self, link: &LinkId) -> spot_link_exchange::Result<usize> {
        Ok(self
            .held
            .lock()
            .map(|held| held.iter().filter(|input| input.link == *link).count())
            .unwrap_or(0))
    }
}

/// A refused input for `link`, built the way the exchange builds one: a signed message with a
/// canonical payload. `n` makes it a distinct message id.
fn refused_input(link: LinkId, n: u64) -> QuarantinedInput {
    let sender = Party::Tenant(TenantId::new_v7(1_700_000_000_000, [5; 10]));
    let signer = SeedSigner::from_seed([5u8; 32], sender);
    let mut entropy = [0u8; 10];
    entropy[..8].copy_from_slice(&n.to_be_bytes());
    let message = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, entropy),
            link,
            conversation: Conversation::new("q_held").unwrap(),
            seq: n + 1,
            topic: Topic::new("bpm.request").unwrap(),
            schema_version: 1,
            sent_at: T0,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: "{\"reference\":\"R1\"}".to_owned(),
        },
        &signer,
    )
    .unwrap();
    QuarantinedInput {
        link,
        message,
        code: RejectCode::PayloadInvalid,
        at: T0,
    }
}

/// The count is the quarantine's, so an input an operator released stops occupying a slot and the
/// next refusal is held again: a partner that had a bad run is not locked out for ever.
#[test]
fn releasing_a_held_input_frees_its_slot() {
    let quarantine = MemoryQuarantine::new();
    let limits = LinkLimits::with(64, 2);
    let link = link(1);
    assert!(limits.hold(&quarantine, refused_input(link, 0)).unwrap());
    let second = refused_input(link, 1);
    assert!(limits.hold(&quarantine, second.clone()).unwrap());
    assert!(
        !limits.hold(&quarantine, refused_input(link, 2)).unwrap(),
        "two inputs fill a bound of two"
    );
    assert_eq!(quarantine.count(&link), 2);

    let released = quarantine.release(&second.message.id).unwrap();
    assert_eq!(released.link, link);
    assert_eq!(quarantine.count(&link), 1, "a released input is no slot");
    assert!(
        limits.hold(&quarantine, refused_input(link, 3)).unwrap(),
        "the freed slot is usable again"
    );
    assert_eq!(quarantine.count(&link), 2);
}

/// A process that restarts keeps no count of its own, so it finds the quarantine full: the bound
/// is the quarantine's state, not the process's memory.
#[test]
fn a_restarted_bound_sees_the_quarantine_it_starts_over() {
    let quarantine = MemoryQuarantine::new();
    let link = link(1);
    let before = LinkLimits::with(64, 2);
    for n in 0..2 {
        assert!(before.hold(&quarantine, refused_input(link, n)).unwrap());
    }
    let after = LinkLimits::with(64, 2);
    assert!(!after.may_hold(&link, &quarantine).unwrap());
    assert!(
        !after.hold(&quarantine, refused_input(link, 9)).unwrap(),
        "a fresh bound over a full quarantine holds nothing"
    );
    assert_eq!(quarantine.count(&link), 2);
}

/// A quarantine that cannot count fails closed: the input is dropped instead of held, and a
/// caller that must decide is told the count is unknown rather than "nothing held". The answer is
/// the crate's own `Error`, so a caller handles one error type on this port.
#[test]
fn a_count_the_quarantine_cannot_give_fails_closed() {
    let quarantine = UncountableQuarantine::default();
    let limits = LinkLimits::new();
    let link = link(1);
    let unknown = limits.may_hold(&link, &quarantine).unwrap_err();
    assert!(
        matches!(
            unknown,
            spot_link_exchange::Error::QuarantineCountUnavailable
        ),
        "the count failure is a variant of the crate's `Error`"
    );
    assert_eq!(unknown.code(), "link_exchange.quarantine_count_unavailable");
    assert!(
        !limits.hold(&quarantine, refused_input(link, 0)).unwrap(),
        "an unknown count is treated as full"
    );
    assert!(
        quarantine.holds.lock().unwrap().is_empty(),
        "a refused input is dropped, never held on an unknown count"
    );
}

/// The bound is per link: one partner spends its own slots only, and cannot spend another's.
#[test]
fn one_link_cannot_spend_another_links_slots() {
    let quarantine = MemoryQuarantine::new();
    let limits = LinkLimits::with(64, 1);
    let (partner, other) = (link(1), link(2));
    assert!(limits.hold(&quarantine, refused_input(partner, 0)).unwrap());
    assert!(
        !limits.hold(&quarantine, refused_input(partner, 1)).unwrap(),
        "the busy partner is at its bound"
    );
    assert!(
        limits.may_hold(&other, &quarantine).unwrap(),
        "the other partner still has its own slot"
    );
    assert!(limits.hold(&quarantine, refused_input(other, 1)).unwrap());
    assert_eq!(quarantine.count(&partner), 1);
    assert_eq!(quarantine.count(&other), 1);
}

/// A re-hold of a message the quarantine already has takes no second slot (the port is
/// idempotent on the message id), so a redelivery cannot fill a link's bound by itself.
#[test]
fn holding_the_same_message_twice_takes_one_slot() {
    let quarantine = MemoryQuarantine::new();
    let limits = LinkLimits::with(64, 2);
    let link = link(1);
    let input = refused_input(link, 0);
    assert!(limits.hold(&quarantine, input.clone()).unwrap());
    assert!(
        limits.hold(&quarantine, input).unwrap(),
        "a repeat finds room and holds nothing new"
    );
    assert_eq!(quarantine.count(&link), 1);
    assert!(limits.hold(&quarantine, refused_input(link, 1)).unwrap());
    assert!(
        !limits.hold(&quarantine, refused_input(link, 2)).unwrap(),
        "one message still cost one slot, and the bound is two"
    );
}

/// Two receipts for one link at `max_held - 1` cannot both take the last slot: the count and the
/// hold are one step under the link's lock. A run repeats the race so a lost lock shows up.
#[test]
fn the_last_slot_of_a_link_is_taken_by_one_receiver_only() {
    for round in 0..32u64 {
        let quarantine = Arc::new(MemoryQuarantine::new());
        let limits = Arc::new(LinkLimits::with(64, 2));
        let link = link(1);
        assert!(
            limits
                .hold(&*quarantine, refused_input(link, round * 4))
                .unwrap(),
            "one slot is already spent"
        );
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for n in 1..3u64 {
            let quarantine = quarantine.clone();
            let limits = limits.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                limits
                    .hold(&*quarantine, refused_input(link, round * 4 + n))
                    .unwrap()
            }));
        }
        let mut held = 0;
        for handle in handles {
            if handle.join().unwrap() {
                held += 1;
            }
        }
        assert_eq!(
            held, 1,
            "exactly one of two concurrent receivers takes the last slot"
        );
        assert_eq!(quarantine.count(&link), 2);
    }
}

/// A re-hold of an input the quarantine already holds is answered as held even when the link is at
/// its bound: it occupies the slot it already has, so it takes no second one and is not dropped by
/// a bound it is already inside. A different input at the same bound is still refused.
#[test]
fn a_re_hold_at_the_bound_takes_no_second_slot() {
    let quarantine = MemoryQuarantine::new();
    let limits = LinkLimits::with(64, 1);
    let link = link(1);
    let input = refused_input(link, 0);
    assert!(limits.hold(&quarantine, input.clone()).unwrap());
    assert!(
        !limits.may_hold(&link, &quarantine).unwrap(),
        "one input fills a bound of one"
    );
    assert!(
        limits.hold(&quarantine, input.clone()).unwrap(),
        "a re-hold of an input the quarantine already holds is answered as held"
    );
    assert_eq!(quarantine.count(&link), 1, "and takes no second slot");
    assert!(
        quarantine.holds(&link, &input.message.id).unwrap(),
        "the input is there exactly once"
    );
    assert!(
        !limits.hold(&quarantine, refused_input(link, 1)).unwrap(),
        "a different input at the bound is still refused"
    );
    assert_eq!(quarantine.count(&link), 1);
}

/// A quarantine whose `holds` is the port's default cannot vouch for an input, so the count and
/// the bound apply to it as before: at the bound nothing more is held, not even a re-hold.
#[test]
fn a_default_holds_leaves_the_bound_in_charge() {
    let quarantine = CountingQuarantine::default();
    let limits = LinkLimits::with(64, 1);
    let link = link(1);
    let input = refused_input(link, 0);
    assert!(
        !quarantine.holds(&link, &input.message.id).unwrap(),
        "the default answers \"not held\""
    );
    assert!(limits.hold(&quarantine, input.clone()).unwrap());
    assert!(
        !limits.hold(&quarantine, input).unwrap(),
        "an input the quarantine cannot vouch for is counted against the bound as before"
    );
    assert!(
        !limits.hold(&quarantine, refused_input(link, 1)).unwrap(),
        "and a new input at the bound is refused"
    );
    assert_eq!(quarantine.held_count(&link).unwrap(), 1);
}

/// The count is only consulted for refusals: a quarantine that cannot count still lets a valid
/// message through, and a refusal is dropped (audited) with its receipt still on the way back.
#[test]
fn a_quarantine_that_cannot_count_never_blocks_a_valid_message() {
    let mut w = World::new();
    let port = Arc::new(UncountableQuarantine::default());
    w.b.quarantine_override = Some(port.clone());
    w.b.limits = Arc::new(LinkLimits::with(2, 1));
    let b = w.b.boot();
    rogue_send(
        &w.a,
        &conv("q_ok"),
        "bpm.request",
        1,
        Some("request_approve@1/request"),
        &[("reference", text("R1")), ("amount_minor", int(5))],
        T0,
    );
    let batch = carry(&w.a, &b, T0 + SECOND);
    assert_eq!(batch.len(), 1);
    assert_eq!(
        w.a.outbox.find(&batch[0].id).unwrap().unwrap().state,
        spot_link_exchange::MessageState::Applied,
        "a valid message is applied whatever the quarantine can count"
    );
    assert!(w.b.gate_audit.codes().is_empty(), "nothing was refused");

    // A malformed header is refused: the count is unknown, so the input is dropped, the drop is
    // audited, and the sender still learns why.
    let at = T0 + 2 * SECOND;
    let bad = rogue_send(
        &w.a,
        &conv("q_bad"),
        "bpm.request",
        1,
        Some("nonsense"),
        &[("reference", text("R"))],
        at,
    );
    carry(&w.a, &b, T0 + 3 * SECOND);
    assert_eq!(
        w.b.gate_audit.codes(),
        vec![codes::HEADER_INVALID, codes::LINK_QUARANTINE_LIMIT],
        "the refusal is written down, and so is the drop the unknown count forced"
    );
    assert_eq!(
        w.a.outbox.find(&bad).unwrap().unwrap().state,
        spot_link_exchange::MessageState::Rejected(RejectCode::PayloadInvalid),
        "the sender still gets its rejection receipt"
    );
    assert!(
        port.holds.lock().unwrap().is_empty(),
        "a quarantine that cannot count is never asked to hold"
    );
}
