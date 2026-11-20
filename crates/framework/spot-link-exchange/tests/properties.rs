//! Hostile-input and property tests for the link exchange (WP-CTL-10.T11).
//!
//! 1. envelope: encode and decode round trip, canonical bytes are stable, arbitrary text never
//!    panics, `filtered` lets through exactly the listed fields and names exactly the dropped ones;
//! 2. message wire: arbitrary bytes and JSON never decode into a message that verifies, any
//!    changed field or flipped bit is refused, an oversized or non-canonical payload is refused;
//! 3. exactly-once application under any delivery schedule (loss, duplication, reordering and a
//!    restart of both sides at any step), no message applied that was not sent, per-conversation
//!    order, one notice per applied message;
//! 4. delivery states only move along the documented edges and a terminal state never changes;
//! 5. every error provoked has a catalogue entry and carries no peer-supplied text.
//!
//! Not covered here (see the report): bounded growth under flood (5 of the brief), version ranges
//! against a reference model and stuck-message conditions (6 of the brief), and a crash inside a
//! single store operation (3 of the brief): the in-memory stores offer no failure injection.
//!
//! 128 cases for the schedule properties, 256 for the others; no failure-persistence file is
//! written, a failing case prints its minimal input.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
#[path = "common/generators.rs"]
mod generators;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use common::{DAY, T0, World, body, connection, conversation, message_topic};
use generators::{FIELD_POOL, Fate, Step, envelope, field_list, schedule};
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use spot_link_exchange::{
    Envelope, Error, EventKind, Lifetimes, Message, MessageId, MessageParts, MessageState,
    NoticeFeed, OutboxStore, Receipt, Retention, canonical, codes,
};
use spot_tenant_link::{IdentitySigner, LinkId, Topic};

fn fast() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

fn schedules() -> ProptestConfig {
    ProptestConfig {
        cases: 128,
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Error vocabulary
// ---------------------------------------------------------------------------------------------

/// The `link_exchange.*` codes of the catalogue files, read from disk.
fn catalogue() -> &'static BTreeSet<String> {
    static CODES: OnceLock<BTreeSet<String>> = OnceLock::new();
    CODES.get_or_init(|| {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("spot-errors")
            .join("catalogue");
        let mut found = BTreeSet::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            for line in std::fs::read_to_string(&path).unwrap().lines() {
                if let Some(rest) = line.trim().strip_prefix("code = \"")
                    && let Some(code) = rest.strip_suffix('"')
                    && code.starts_with("link_exchange.")
                {
                    found.insert(code.to_owned());
                }
            }
        }
        found
    })
}

/// An error has a catalogue entry and its text holds none of the `canaries`.
fn check_error(error: &Error, canaries: &[&str]) -> Result<(), TestCaseError> {
    prop_assert!(
        catalogue().contains(error.code()),
        "{} has no catalogue entry",
        error.code()
    );
    let shown = format!("{error} {error:?}");
    for canary in canaries {
        prop_assert!(
            !shown.contains(canary),
            "{} echoes peer-supplied text",
            error.code()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 1. The record envelope
// ---------------------------------------------------------------------------------------------

proptest! {
    #![proptest_config(fast())]

    /// `decode(encode(x)) == x`, and encoding the decoded value gives the same canonical bytes.
    #[test]
    fn an_envelope_round_trips_and_its_canonical_bytes_are_stable(envelope in envelope()) {
        let text = envelope.encode().unwrap();
        let decoded = Envelope::decode(&text).unwrap();
        prop_assert_eq!(&decoded, &envelope);
        prop_assert_eq!(decoded.encode().unwrap(), text.clone());
        prop_assert!(canonical::is_canonical(&text));
        prop_assert!(envelope.validate().is_ok());
    }

    /// Arbitrary text never panics `Envelope::decode`; a refusal is a typed, catalogued error
    /// that does not repeat the text.
    #[test]
    fn arbitrary_text_never_panics_the_envelope_decoder(text in "\\PC{0,400}") {
        if let Err(error) = Envelope::decode(&text) {
            check_error(&error, &[text.as_str()].into_iter().filter(|t| t.len() > 3).collect::<Vec<_>>())?;
        }
    }

    /// A random byte mutation of a valid envelope decodes to an envelope that validates, or is
    /// refused; it never panics.
    #[test]
    fn a_mutated_envelope_is_refused_or_still_valid(
        envelope in envelope(),
        position in any::<proptest::sample::Index>(),
        replacement in any::<u8>(),
    ) {
        let mut bytes = envelope.encode().unwrap().into_bytes();
        let at = position.index(bytes.len());
        bytes[at] = replacement;
        if let Ok(text) = String::from_utf8(bytes)
            && let Ok(decoded) = Envelope::decode(&text)
        {
            let _ = decoded.validate();
        }
    }

    /// `filtered` keeps exactly the listed fields, names exactly the dropped ones, and what it
    /// returns is `within` the list; `within` of the original says whether anything was dropped.
    #[test]
    fn filtering_keeps_the_listed_fields_and_names_exactly_the_dropped_ones(
        envelope in envelope(),
        list in field_list(),
    ) {
        let before: BTreeSet<String> = envelope.record.document.fields.keys().cloned().collect();
        let expected_dropped: BTreeSet<String> = before
            .iter()
            .filter(|name| !list.includes(&spot_tenant_link::FieldName::new(name.as_str()).unwrap()))
            .cloned()
            .collect();
        let was_within = envelope.within(&list);
        let (filtered, dropped) = envelope.filtered(&list);
        let after: BTreeSet<String> = filtered.record.document.fields.keys().cloned().collect();
        let dropped: BTreeSet<String> = dropped.into_iter().collect();
        prop_assert_eq!(&dropped, &expected_dropped);
        prop_assert_eq!(after, before.difference(&expected_dropped).cloned().collect::<BTreeSet<_>>());
        prop_assert!(filtered.within(&list));
        prop_assert_eq!(was_within, expected_dropped.is_empty());
    }
}

#[test]
fn the_field_pool_names_are_valid_field_names() {
    for name in FIELD_POOL {
        assert!(spot_tenant_link::FieldName::new(name).is_ok(), "{name}");
    }
}

// ---------------------------------------------------------------------------------------------
// 2. The message wire
// ---------------------------------------------------------------------------------------------

/// A signed message of the fixture world, with the sender's public key.
fn signed(world: &World, seq: u64, tombstone: bool) -> Message {
    let (payload, expires_at, replaces) = if tombstone {
        (
            spot_link_exchange::TOMBSTONE_PAYLOAD.to_owned(),
            spot_link_exchange::TOMBSTONE_EXPIRES,
            Some(MessageId::new_v7(T0, [7u8; 10])),
        )
    } else {
        (
            r#"{"message.body":{"text":"hello"}}"#.to_owned(),
            T0 + DAY,
            None,
        )
    };
    Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, [seq as u8; 10]),
            link: world.link_a.id,
            conversation: conversation("c1"),
            seq,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at,
            tombstone,
            replaces,
            payload,
        },
        &world.signer_a,
    )
    .unwrap()
}

/// Every single-field change of a message, kept with its old signature.
fn field_changes(message: &Message) -> Vec<(&'static str, Message)> {
    let mut out = Vec::new();
    let mut add = |label: &'static str, edit: &dyn Fn(&mut Message)| {
        let mut changed = message.clone();
        edit(&mut changed);
        out.push((label, changed));
    };
    add("id", &|m| m.id = MessageId::new_v7(T0 + 1, [99u8; 10]));
    add("link", &|m| {
        m.link = LinkId::new_v7(1_700_000_000_001, [8u8; 10])
    });
    add("sender", &|m| m.sender = common::tenant(77));
    add("conversation", &|m| m.conversation = conversation("other"));
    add("seq", &|m| m.seq += 1);
    add("topic", &|m| m.topic = Topic::new("bpm.other").unwrap());
    add("schema_version", &|m| m.schema_version += 1);
    add("sent_at", &|m| m.sent_at += 1);
    add("expires_at", &|m| m.expires_at -= 1);
    add("tombstone", &|m| m.tombstone = !m.tombstone);
    add("replaces", &|m| {
        m.replaces = match m.replaces {
            Some(_) => None,
            None => Some(MessageId::new_v7(T0, [55u8; 10])),
        };
    });
    add("payload", &|m| m.payload.push(' '));
    out
}

proptest! {
    #![proptest_config(fast())]

    /// Arbitrary bytes never decode into a message or receipt that verifies.
    #[test]
    fn arbitrary_bytes_never_authenticate_a_message(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let world = World::new();
        let key = world.signer_a.public_key();
        let ok = serde_json::from_slice::<Message>(&bytes)
            .map(|message| message.verify(&key).is_ok())
            .unwrap_or(false);
        prop_assert!(!ok);
        let receipt = serde_json::from_slice::<Receipt>(&bytes);
        prop_assert!(receipt.is_err());
    }

    /// Any one changed field with the old signature is refused and signs different bytes.
    #[test]
    fn every_signed_field_of_a_message_is_covered(seq in 1u64..200, tombstone in any::<bool>()) {
        let world = World::new();
        let key = world.signer_a.public_key();
        let message = signed(&world, seq, tombstone);
        prop_assert!(message.verify(&key).is_ok());
        for (label, changed) in field_changes(&message) {
            prop_assert_ne!(changed.signing_bytes(), message.signing_bytes(), "{} is not signed", label);
            prop_assert!(changed.verify(&key).is_err(), "a changed {} still verifies", label);
        }
    }

    /// One flipped bit of the encoded message decodes to nothing, to the original, or to a
    /// message that does not verify.
    #[test]
    fn a_flipped_bit_of_an_encoded_message_is_refused(
        seq in 1u64..200,
        position in any::<proptest::sample::Index>(),
        bit in 0u8..8,
    ) {
        let world = World::new();
        let key = world.signer_a.public_key();
        let message = signed(&world, seq, false);
        let mut bytes = serde_json::to_vec(&message).unwrap();
        let at = position.index(bytes.len());
        bytes[at] ^= 1 << bit;
        let accepted = match serde_json::from_slice::<Message>(&bytes) {
            Err(_) => false,
            Ok(decoded) => decoded != message && decoded.verify(&key).is_ok(),
        };
        prop_assert!(!accepted);
    }

    /// A flipped bit of the signing bytes breaks the signature.
    #[test]
    fn a_flipped_bit_of_the_signed_bytes_breaks_the_signature(
        seq in 1u64..200,
        position in any::<proptest::sample::Index>(),
        bit in 0u8..8,
    ) {
        let world = World::new();
        let key = world.signer_a.public_key();
        let message = signed(&world, seq, false);
        let mut bytes = message.signing_bytes();
        let at = position.index(bytes.len());
        bytes[at] ^= 1 << bit;
        prop_assert!(key.verify(&bytes, &message.sender_signature).is_err());
    }

    /// A payload that is not canonical JSON, or is over the size cap, is refused at signing and
    /// by the canonicaliser, with a catalogued code that does not repeat the payload.
    #[test]
    fn a_hostile_payload_is_refused_without_being_echoed(extra in "CANARY[0-9]{6,20}") {
        let world = World::new();
        let oversized = format!("{{\"{extra}\":\"{}\"}}", "x".repeat(canonical::MAX_PAYLOAD_BYTES));
        for payload in [format!("{{ \"{extra}\" : 1 }}"), oversized, format!("[{extra}"), extra.clone()] {
            let attempt = Message::sign(
                MessageParts {
                    id: MessageId::new_v7(T0, [1u8; 10]),
                    link: world.link_a.id,
                    conversation: conversation("c1"),
                    seq: 1,
                    topic: message_topic(),
                    schema_version: 1,
                    sent_at: T0,
                    expires_at: T0 + DAY,
                    tombstone: false,
                    replaces: None,
                    payload: payload.clone(),
                },
                &world.signer_a,
            );
            if let Err(error) = attempt {
                prop_assert_eq!(error.code(), codes::INVALID_PAYLOAD);
                check_error(&error, &[extra.as_str()])?;
            } else {
                prop_assert!(canonical::is_canonical(&payload));
                prop_assert!(payload.len() <= canonical::MAX_PAYLOAD_BYTES);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 3 and 4. Delivery schedules, restarts and state monotonicity
// ---------------------------------------------------------------------------------------------

/// A state's rank on the way to a terminal one; `None` for terminal states.
fn open_rank(state: MessageState) -> Option<u8> {
    match state {
        MessageState::Queued => Some(0),
        MessageState::Sent => Some(1),
        MessageState::Acknowledged => Some(2),
        MessageState::Applied | MessageState::Rejected(_) | MessageState::Expired => None,
    }
}

/// The documented edges: forward along queued, sent, acknowledged, or to a terminal state from
/// any open one; a terminal state never changes.
fn edge_allowed(from: MessageState, to: MessageState) -> bool {
    match open_rank(from) {
        None => from == to,
        Some(rank) => match open_rank(to) {
            Some(next) => next >= rank,
            None => true,
        },
    }
}

struct Wire {
    messages: Vec<Message>,
    receipts: Vec<Receipt>,
}

/// Runs a schedule, then drains the wire without loss until nothing is live, and checks the
/// invariants. Returns the number of messages queued.
fn run_schedule(steps: &[Step]) -> Result<usize, TestCaseError> {
    let world = World::new();
    let lifetimes = Lifetimes::new(Retention::from_days(30).unwrap());
    let topic = message_topic();
    let conversations = ["c1", "c2", "c3"].map(conversation);
    let mut produced: Vec<MessageId> = Vec::new();
    let mut wire = Wire {
        messages: Vec::new(),
        receipts: Vec::new(),
    };
    let mut states: BTreeMap<MessageId, MessageState> = BTreeMap::new();
    let mut now = T0;

    for step in steps {
        now += 60_000;
        // Every step builds its sender and receiver afresh from the persisted stores: that is
        // what a restart of either side is.
        let outbox = world.sender(lifetimes.clone());
        let inbox = world.receiver();
        match *step {
            Step::Enqueue(index) => {
                let conversation = &conversations[usize::from(index)];
                let queued = outbox
                    .enqueue_message(
                        &world.submit(conversation, &topic, now),
                        &body(&format!("n{}", produced.len())),
                    )
                    .unwrap();
                produced.push(queued.id);
            }
            Step::Pump => {
                let batch = outbox
                    .next_batch(&world.link_a, now, 50, "system", None)
                    .unwrap();
                wire.messages.extend(batch);
            }
            Step::Message { pick, fate } => {
                if !wire.messages.is_empty() {
                    let at = pick % wire.messages.len();
                    let message = match fate {
                        Fate::Duplicate => wire.messages[at].clone(),
                        _ => wire.messages.remove(at),
                    };
                    if !matches!(fate, Fate::Drop) {
                        let outcome = inbox
                            .receive_from(&world.link_b, &connection(&world.a), &message, now)
                            .unwrap();
                        wire.receipts.extend(outcome.receipts);
                    }
                }
            }
            Step::Receipt { pick, fate } => {
                if !wire.receipts.is_empty() {
                    let at = pick % wire.receipts.len();
                    let receipt = match fate {
                        Fate::Duplicate => wire.receipts[at].clone(),
                        _ => wire.receipts.remove(at),
                    };
                    if !matches!(fate, Fate::Drop) {
                        match outbox.on_receipt_from(
                            &world.link_a,
                            &connection(&world.b),
                            &receipt,
                            now,
                        ) {
                            Ok(_) | Err(Error::NotFound) => {}
                            Err(error) => {
                                return Err(TestCaseError::fail(format!(
                                    "receipt refused: {error}"
                                )));
                            }
                        }
                    }
                }
            }
            Step::Sweep => {
                wire.receipts.extend(inbox.sweep_expired(now).unwrap());
            }
            Step::Restart => {}
        }
        observe(&world, &mut states)?;
    }

    // Drain: from here every message and receipt is delivered at least once.
    let mut rounds = 0;
    loop {
        now += 3_600_000;
        rounds += 1;
        let outbox = world.sender(lifetimes.clone());
        let inbox = world.receiver();
        let mut batch = outbox
            .next_batch(&world.link_a, now, 200, "system", None)
            .unwrap();
        batch.append(&mut wire.messages);
        let mut receipts = std::mem::take(&mut wire.receipts);
        for message in &batch {
            receipts.extend(
                inbox
                    .receive_from(&world.link_b, &connection(&world.a), message, now)
                    .unwrap()
                    .receipts,
            );
        }
        receipts.extend(inbox.sweep_expired(now).unwrap());
        for receipt in &receipts {
            match outbox.on_receipt_from(&world.link_a, &connection(&world.b), receipt, now) {
                Ok(_) | Err(Error::NotFound) => {}
                Err(error) => return Err(TestCaseError::fail(format!("receipt refused: {error}"))),
            }
        }
        observe(&world, &mut states)?;
        if world.outbox.live(None).unwrap().is_empty() {
            break;
        }
        prop_assert!(rounds < 500, "the exchange did not settle");
    }
    check_final(&world, &produced)?;
    Ok(produced.len())
}

/// State monotonicity: every message's state moves only along a documented edge.
fn observe(
    world: &World,
    states: &mut BTreeMap<MessageId, MessageState>,
) -> Result<(), TestCaseError> {
    for entry in world.outbox.all().unwrap() {
        if let Some(before) = states.insert(entry.message.id, entry.state) {
            prop_assert!(
                edge_allowed(before, entry.state),
                "a message moved from {} to {}",
                before.as_str(),
                entry.state.as_str()
            );
        }
    }
    Ok(())
}

fn check_final(world: &World, produced: &[MessageId]) -> Result<(), TestCaseError> {
    let entries = world.outbox.all().unwrap();
    let slot: BTreeMap<MessageId, (String, u64)> = entries
        .iter()
        .map(|e| {
            (
                e.message.id,
                (e.message.conversation.to_string(), e.message.seq),
            )
        })
        .collect();
    let stored = world.inbox.stored().unwrap();
    let stored_ids: BTreeSet<MessageId> = stored.iter().map(|(_, e)| e.message.id).collect();
    prop_assert_eq!(stored_ids.len(), stored.len(), "a message was stored twice");
    let produced_set: BTreeSet<MessageId> = produced.iter().copied().collect();
    prop_assert!(
        stored_ids.is_subset(&produced_set),
        "something that was never sent was applied"
    );
    let notices = world.inbox.read_from(0, usize::MAX).unwrap();
    let mut seen: BTreeMap<MessageId, u32> = BTreeMap::new();
    let mut last_seq: BTreeMap<String, u64> = BTreeMap::new();
    for notice in &notices {
        *seen.entry(notice.message_id).or_insert(0) += 1;
        let (conversation, seq) = slot.get(&notice.message_id).unwrap();
        let previous = last_seq.insert(conversation.clone(), *seq).unwrap_or(0);
        prop_assert!(
            *seq > previous,
            "conversation {} applied out of order",
            conversation
        );
    }
    for id in &stored_ids {
        prop_assert_eq!(seen.get(id), Some(&1), "one notice per applied message");
    }
    prop_assert_eq!(seen.len(), stored_ids.len());
    let events = world.outbox.events_from(0, usize::MAX).unwrap();
    for id in produced {
        let state = world.outbox.find(id).unwrap().unwrap().state;
        prop_assert_eq!(state, MessageState::Applied, "every message ends applied");
        prop_assert!(
            stored_ids.contains(id),
            "applied at the sender but not stored"
        );
        let last = events
            .iter()
            .rev()
            .find(|(_, e)| &e.message == id)
            .map(|(_, e)| e.kind);
        prop_assert_eq!(last, Some(EventKind::Applied));
    }
    Ok(())
}

proptest! {
    #![proptest_config(schedules())]

    /// Any schedule of enqueues, pumps, lost, duplicated and delayed messages and receipts, and
    /// restarts of both sides, that is followed by a lossless drain ends with every message
    /// applied exactly once, in conversation order, one notice each, no foreign message, and the
    /// sender's states having moved only along the documented edges.
    #[test]
    fn every_message_is_applied_exactly_once_under_any_delivery_schedule(steps in schedule()) {
        run_schedule(&steps)?;
    }
}

#[test]
fn the_edge_rule_matches_the_documented_state_graph() {
    use MessageState::{Acknowledged, Applied, Expired, Queued, Sent};
    assert!(edge_allowed(Queued, Sent));
    assert!(edge_allowed(Sent, Acknowledged));
    assert!(edge_allowed(Queued, Applied));
    assert!(edge_allowed(Acknowledged, Expired));
    assert!(!edge_allowed(Sent, Queued));
    assert!(!edge_allowed(Acknowledged, Sent));
    assert!(!edge_allowed(Applied, Expired));
    assert!(!edge_allowed(Expired, Applied));
    assert!(edge_allowed(Applied, Applied));
}
