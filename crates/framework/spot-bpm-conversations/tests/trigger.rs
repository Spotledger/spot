//! The message-arrived starter: the Case id it derives and the identity check that keeps a
//! colliding message out of another conversation's Case.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use serde_json::json;
use spot_bpm_conversations::{ConversationRecord, ConversationStore, EngineCaseStarter, codes};
use spot_bpm_engine::{InMemoryCaseStore, InMemoryTimerPort};
use spot_bpm_model::NodeId;
use spot_bpm_triggers::{CaseStarter, StartInput, StartKey, StartOutcome};
use spot_link_exchange::{Conversation, MessageId};
use spot_tenant_link::LinkId;

fn message_id(text: &str) -> MessageId {
    let mut entropy = [0u8; 10];
    for (slot, byte) in entropy.iter_mut().zip(text.as_bytes()) {
        *slot = *byte;
    }
    MessageId::new_v7(T0, entropy)
}

fn key_for(id: MessageId) -> StartKey {
    StartKey::new("appr_flow", NodeId::new("start").unwrap(), id.to_string())
}

/// The Case id a start key derives, as text.
fn derived(key: &StartKey) -> String {
    EngineCaseStarter::<InMemoryCaseStore, InMemoryTimerPort>::case_id(key)
        .unwrap()
        .as_str()
        .to_owned()
}

fn input_for(id: MessageId, conversation: &Conversation, link: LinkId) -> StartInput {
    StartInput {
        version: 1,
        variables: json!({
            "bpm": {
                "conv": { "request_approve": {
                    "id": conversation.to_string(),
                    "link": link.to_string(),
                } },
                "entry": {
                    "contract": "request_approve",
                    "message": "request",
                    "message_id": id.to_string(),
                    "received_at_ms": T0,
                },
            },
            "received": { "message": { "reference": "R1", "amount_minor": 5 } },
        }),
        initiator: None,
    }
}

/// A conversation record on the approver's side, so `bind` has somewhere to write the Case.
fn conversation(w: &World) -> Conversation {
    let conversation = Conversation::new("c_collision").unwrap();
    w.b.conversations
        .create(ConversationRecord::new(
            w.b.link_id(),
            conversation.clone(),
            &contract(),
            "approver",
            "requester",
        ))
        .unwrap();
    conversation
}

fn bound_case(w: &World, conversation: &Conversation) -> Option<String> {
    w.b.conversations
        .load(&w.b.link_id(), conversation)
        .unwrap()
        .unwrap()
        .case
}

#[test]
fn the_case_id_of_a_start_key_is_a_truncated_sha256() {
    let key = key_for(message_id("one"));
    let text = derived(&key);
    // `m` plus 128 bits of hex, so a peer cannot steer its message onto another Case.
    assert_eq!(text.len(), 33, "{text}");
    assert!(text.starts_with('m'), "{text}");
    assert!(
        text[1..]
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{text}"
    );
    // The derivation is pure: the same key is the same id on every redelivery.
    assert_eq!(derived(&key), text);
    // Another message id is another Case.
    assert_ne!(derived(&key_for(message_id("two"))), text);
}

#[test]
fn a_start_the_case_store_does_not_hold_creates_its_case() {
    let w = World::new();
    let b = w.b.boot();
    let id = message_id("fresh");
    let conversation = conversation(&w);
    let key = key_for(id);
    let starter = EngineCaseStarter::new(b.engine.clone(), w.b.conversations.clone());
    let outcome =
        block_on(starter.start(key.clone(), input_for(id, &conversation, w.b.link_id()))).unwrap();
    let StartOutcome::Started { case } = outcome else {
        panic!("a fresh start starts: {outcome:?}");
    };
    assert_eq!(case, derived(&key));
    assert_eq!(
        bound_case(&w, &conversation).as_deref(),
        Some(case.as_str())
    );
    let record =
        w.b.conversations
            .load(&w.b.link_id(), &conversation)
            .unwrap()
            .unwrap();
    assert!(record.consumed.contains(&id));
}

/// A collision forced by planting a Case under the very id the start key derives, started by a
/// *different* message. Binding the conversation to it would hand this peer's `cancel` the
/// victim's compensation, so the start must be refused.
#[test]
fn a_forced_case_id_collision_is_refused_instead_of_binding_the_wrong_conversation() {
    let w = World::new();
    let b = w.b.boot();
    let id = message_id("victim");
    let conversation = conversation(&w);
    let key = key_for(id);
    let case = derived(&key);
    // The victim's Case: the same id, started by another message.
    let victim = message_id("attacker");
    b.start_text(
        &case,
        APPROVER,
        json!({ "bpm": { "entry": { "message_id": victim.to_string() } } }),
        T0,
    );
    let starter = EngineCaseStarter::new(b.engine.clone(), w.b.conversations.clone());
    let error = block_on(starter.start(key, input_for(id, &conversation, w.b.link_id())))
        .expect_err("a colliding start is refused");
    let text = error.to_string();
    assert!(text.contains(codes::CASE_COLLISION), "{text}");
    // The conversation was not bound to the other message's Case, and nothing was consumed.
    assert!(bound_case(&w, &conversation).is_none());
    let record =
        w.b.conversations
            .load(&w.b.link_id(), &conversation)
            .unwrap()
            .unwrap();
    assert!(!record.consumed.contains(&id));
    // Nothing was even attempted under the planted Case: it still belongs to the other message.
    let started_by = b
        .case(&case)
        .variables
        .pointer("/bpm/entry/message_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    assert_eq!(started_by, Some(victim.to_string()));
}

#[test]
fn an_existing_case_started_by_the_same_message_is_bound_and_answered_as_existing() {
    let w = World::new();
    let b = w.b.boot();
    let id = message_id("same");
    let conversation = conversation(&w);
    let key = key_for(id);
    let case = derived(&key);
    b.start_text(
        &case,
        APPROVER,
        json!({ "bpm": { "entry": { "message_id": id.to_string() } } }),
        T0,
    );
    let starter = EngineCaseStarter::new(b.engine.clone(), w.b.conversations.clone());
    let outcome =
        block_on(starter.start(key, input_for(id, &conversation, w.b.link_id()))).unwrap();
    assert_eq!(outcome, StartOutcome::Existing { case: case.clone() });
    assert_eq!(
        bound_case(&w, &conversation).as_deref(),
        Some(case.as_str())
    );
}
