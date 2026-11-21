//! Conformance: every incoming message is held to the contract before the inbox takes it, and
//! every outgoing one before it leaves.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use serde_json::json;
use spot_bpm_conversations::{ConformantInbox, ConversationStore, GateAudit, ReceiveError, codes};
use spot_link_exchange::{
    Conversation, Inbox, InboxStore, Message, MessageId, MessageParts, MessageState, OutboxStore,
    ReceiveStatus, RejectCode,
};

fn conv(name: &str) -> Conversation {
    Conversation::new(name).unwrap()
}

/// A requester that has sent its request, delivered to the approver, who has not answered yet.
fn started() -> (World, Conversation) {
    let w = World::new();
    {
        let (a, b) = (w.a.boot(), w.b.boot());
        a.start_request("req1", "REF-1", T0);
        a.settle(T0);
        assert_eq!(carry(&w.a, &b, T0 + SECOND).len(), 1);
    }
    let (_, conversation) = conversation_for_case(&w.a, "req1");
    (w, conversation)
}

fn rejection_of(w: &World, id: &MessageId) -> RejectCode {
    match w.a.outbox.find(id).unwrap().unwrap().state {
        MessageState::Rejected(code) => code,
        other => panic!("expected a rejection, found {other:?}"),
    }
}

#[test]
fn an_out_of_order_message_is_rejected_with_a_reason_and_quarantined() {
    let (w, conversation) = started();
    let b = w.b.boot();
    // The requester sends a second request: after a request only a cancel may follow.
    let rogue = rogue_send(
        &w.a,
        &conversation,
        "bpm.request",
        1,
        Some("request_approve@1/request"),
        &[("reference", text("R2")), ("amount_minor", int(5))],
        T0 + 2 * SECOND,
    );
    assert_eq!(carry(&w.a, &b, T0 + 3 * SECOND).len(), 1);

    // The sender learns the reason as a code on its message.
    assert_eq!(rejection_of(&w, &rogue), RejectCode::OutOfOrderExpired);
    // The receiver holds the input for a person, and never applied it.
    let held = w.b.quarantine.held();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].message.id, rogue);
    assert_eq!(held[0].code, RejectCode::OutOfOrderExpired);
    assert!(w.b.inbox.entry(&w.b.link().id, &rogue).unwrap().is_none());
    // Nothing the request does depends on it: no marker, and the conversation is intact.
    let record =
        w.b.conversations
            .load(&w.b.link_id(), &conversation)
            .unwrap()
            .unwrap();
    assert_eq!(record.events.len(), 1);
    assert!(record.attention.is_none());

    // The stream is not blocked: the slot is filled and the real conversation goes on.
    carry(&w.a, &b, T0 + 4 * SECOND);
    b.advance(T0 + 5 * SECOND);
    let a = w.a.boot();
    carry(&w.b, &a, T0 + 6 * SECOND);
    a.advance(T0 + 7 * SECOND);
    assert_eq!(w.a.ledger.total(), 1);
    assert_eq!(w.b.ledger.total(), 2);
}

#[test]
fn every_kind_of_violation_is_rejected_with_its_reason_and_quarantined() {
    use spot_tenant_link::FieldValue;
    type Case = (
        &'static str,
        &'static str,
        u32,
        Option<&'static str>,
        Vec<(&'static str, FieldValue)>,
        RejectCode,
    );
    let ok = || vec![("reference", text("R1")), ("amount_minor", int(5))];
    let cases: Vec<Case> = vec![
        (
            "an unknown message",
            "bpm.request",
            1,
            Some("request_approve@1/frobnicate"),
            ok(),
            RejectCode::PayloadInvalid,
        ),
        (
            "no header",
            "bpm.request",
            1,
            None,
            ok(),
            RejectCode::PayloadInvalid,
        ),
        (
            "a malformed header",
            "bpm.request",
            1,
            Some("nonsense"),
            ok(),
            RejectCode::PayloadInvalid,
        ),
        (
            "an unknown contract",
            "bpm.request",
            1,
            Some("other@1/request"),
            ok(),
            RejectCode::VersionUnsupported,
        ),
        (
            "an unknown contract version",
            "bpm.request",
            1,
            Some("request_approve@9/request"),
            ok(),
            RejectCode::VersionUnsupported,
        ),
        (
            "another schema version",
            "bpm.request",
            2,
            Some("request_approve@1/request"),
            ok(),
            RejectCode::VersionUnsupported,
        ),
        (
            "the wrong topic",
            "bpm.decision",
            1,
            Some("request_approve@1/request"),
            vec![("reference", text("R1"))],
            RejectCode::PayloadInvalid,
        ),
        (
            "a field the message does not have",
            "bpm.request",
            1,
            Some("request_approve@1/request"),
            vec![
                ("reference", text("R1")),
                ("amount_minor", int(5)),
                ("reason", text("why")),
            ],
            RejectCode::PayloadInvalid,
        ),
        (
            "a missing required field",
            "bpm.request",
            1,
            Some("request_approve@1/request"),
            vec![("reference", text("R1"))],
            RejectCode::PayloadInvalid,
        ),
        (
            "a field of the wrong type",
            "bpm.request",
            1,
            Some("request_approve@1/request"),
            vec![("reference", text("R1")), ("amount_minor", text("5"))],
            RejectCode::PayloadInvalid,
        ),
        (
            "a text that is too long",
            "bpm.request",
            1,
            Some("request_approve@1/request"),
            vec![
                ("reference", text(&"x".repeat(65))),
                ("amount_minor", int(5)),
            ],
            RejectCode::PayloadInvalid,
        ),
        (
            "a message of the other role",
            "bpm.decision",
            1,
            Some("request_approve@1/approve"),
            vec![("reference", text("R1"))],
            RejectCode::PayloadInvalid,
        ),
    ];
    for (what, topic, schema, header, fields, expected) in cases {
        let w = World::new();
        let b = w.b.boot();
        let id = rogue_send(&w.a, &conv("rogue1"), topic, schema, header, &fields, T0);
        carry(&w.a, &b, T0 + SECOND);
        assert_eq!(rejection_of(&w, &id), expected, "{what}");
        let held = w.b.quarantine.held();
        assert_eq!(held.len(), 1, "{what}");
        assert_eq!(held[0].code, expected, "{what}");
        assert!(
            w.b.inbox.entry(&w.b.link().id, &id).unwrap().is_none(),
            "{what}"
        );
        assert!(
            w.b.conversations
                .load(&w.b.link_id(), &conv("rogue1"))
                .unwrap()
                .is_none(),
            "{what}: a refused message opens no conversation"
        );
    }
}

#[test]
fn a_message_that_cannot_open_a_conversation_is_refused_as_such() {
    // A `cancel` is a valid message but not an entry message.
    let w = World::new();
    let b = w.b.boot();
    let id = rogue_send(
        &w.a,
        &conv("rogue1"),
        "bpm.request",
        1,
        Some("request_approve@1/cancel"),
        &[("reference", text("R1"))],
        T0,
    );
    carry(&w.a, &b, T0 + SECOND);
    assert_eq!(rejection_of(&w, &id), RejectCode::PayloadInvalid);
    assert_eq!(w.b.quarantine.held().len(), 1);
}

#[test]
fn a_redelivered_rejection_gets_the_same_answer_and_one_quarantine_entry() {
    let w = World::new();
    let b = w.b.boot();
    let id = rogue_send(
        &w.a,
        &conv("rogue1"),
        "bpm.request",
        1,
        Some("nonsense"),
        &[("reference", text("R"))],
        T0,
    );
    let batch = carry(&w.a, &b, T0 + SECOND);
    for n in 2..5 {
        assert_eq!(
            deliver(&b, &w.a, &batch[0], T0 + n * SECOND),
            ReceiveStatus::Rejected(RejectCode::PayloadInvalid)
        );
    }
    assert_eq!(w.b.quarantine.held().len(), 1);
    assert_eq!(rejection_of(&w, &id), RejectCode::PayloadInvalid);
}

#[test]
fn a_pinned_contract_version_cannot_be_switched_mid_conversation() {
    let (w, conversation) = started();
    let b = w.b.boot();
    let id = rogue_send(
        &w.a,
        &conversation,
        "bpm.request",
        1,
        Some("request_approve@2/cancel"),
        &[("reference", text("R1"))],
        T0 + 2 * SECOND,
    );
    carry(&w.a, &b, T0 + 3 * SECOND);
    assert_eq!(rejection_of(&w, &id), RejectCode::VersionUnsupported);
    let record =
        w.b.conversations
            .load(&w.b.link_id(), &conversation)
            .unwrap()
            .unwrap();
    assert_eq!(record.contract_version, 1);
}

#[test]
fn a_failure_that_blocks_the_path_marks_the_request_and_one_that_does_not_leaves_it_alone() {
    // The requester waits for `approve`. A broken `approve` blocks it.
    let (w, conversation) = started();
    let a = w.a.boot();
    let bad = rogue_send(
        &w.b,
        &conversation,
        "bpm.decision",
        1,
        Some("request_approve@1/approve"),
        &[],
        T0 + 2 * SECOND,
    );
    carry(&w.b, &a, T0 + 3 * SECOND);
    assert_eq!(w.a.quarantine.held().len(), 1);
    assert_eq!(w.a.quarantine.held()[0].message.id, bad);
    let record =
        w.a.conversations
            .load(&w.a.link_id(), &conversation)
            .unwrap()
            .unwrap();
    assert_eq!(
        record.attention.as_ref().unwrap().code,
        codes::PAYLOAD_INVALID
    );
    let view = a
        .driver
        .view(
            &w.a.link_id(),
            &conversation,
            at(T0 + 4 * SECOND),
            &Default::default(),
        )
        .unwrap();
    assert!(view.needs_attention.is_some());
    assert!(view.conditions.iter().any(|c| c.code == "NEEDS_ATTENTION"));

    // An unrelated broken message, in a fresh world, marks nothing.
    let (w, conversation) = started();
    let a = w.a.boot();
    rogue_send(
        &w.b,
        &conversation,
        "bpm.decision",
        1,
        Some("request_approve@1/frobnicate"),
        &[],
        T0 + 2 * SECOND,
    );
    carry(&w.b, &a, T0 + 3 * SECOND);
    assert_eq!(w.a.quarantine.held().len(), 1);
    let record =
        w.a.conversations
            .load(&w.a.link_id(), &conversation)
            .unwrap()
            .unwrap();
    assert!(
        record.attention.is_none(),
        "the refusal blocks nothing the request waits for"
    );
}

#[test]
fn a_message_that_does_not_verify_gets_no_receipt_and_no_quarantine_entry() {
    let (w, conversation) = started();
    let b = w.b.boot();
    // Signed by the approver but presented as coming from the requester's link.
    let forged = Message::sign(
        MessageParts {
            id: spot_link_exchange::MessageId::new_v7(T0 + 2 * SECOND, [9; 10]),
            link: w.a.link_id(),
            conversation: conversation.clone(),
            seq: 2,
            topic: spot_tenant_link::Topic::new("bpm.request").unwrap(),
            schema_version: 1,
            sent_at: T0 + 2 * SECOND,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: spot_link_exchange::canonicalize(
                r#"{"bpm.message":{"text":"request_approve@1/cancel"},"reference":{"text":"R"}}"#,
            )
            .unwrap(),
        },
        &*w.b.signer,
    )
    .unwrap();
    let side = &w.b;
    let inbox = Inbox::new(&*side.inbox, &*side.audit, &*side.quarantine, &*side.signer);
    let gate = ConformantInbox::new(
        &inbox,
        &*side.inbox,
        &*side.quarantine,
        &*side.signer,
        &b.runtime,
    );
    let result = gate.receive_from(
        &side.link(),
        &connection(&w.a.party),
        &forged,
        T0 + 3 * SECOND,
    );
    assert!(matches!(result, Err(ReceiveError::Exchange { .. })));
    assert!(w.b.quarantine.held().is_empty());
    assert!(
        w.b.inbox
            .entry(&w.b.link().id, &forged.id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_send_is_validated_against_the_contract_before_anything_is_queued() {
    let secret = "TOPSECRET-VALUE";
    let cases = vec![
        (
            "an extra field",
            json!({"reference": "R1", "amount_minor": 1, "evil": secret}),
        ),
        (
            "a missing required field",
            json!({"amount_minor": 1, "note": secret}),
        ),
        (
            "a field of the wrong type",
            json!({"reference": secret, "amount_minor": secret}),
        ),
        (
            "a text that is too long",
            json!({"reference": secret.repeat(8), "amount_minor": 1}),
        ),
        ("a float", json!({"reference": "R1", "amount_minor": 1.5})),
        ("no payload at all", json!({})),
    ];
    for (what, out) in cases {
        let w = World::new();
        let a = w.a.boot();
        a.start_with("req1", json!({ "bpm": { "out": out } }), T0);
        a.settle(T0);
        let case = a.case("req1");
        let node = spot_dag::NodeId::new("send_request").unwrap();
        let failure = case.attempt(&node).unwrap().failure.clone().unwrap();
        assert!(
            failure.contains("bpm_conversations.payload_invalid"),
            "{what}: {failure}"
        );
        assert!(
            !failure.contains(secret),
            "{what}: the reason echoes a value: {failure}"
        );
        assert!(
            !failure.contains("evil"),
            "{what}: the reason echoes a field name: {failure}"
        );
        assert!(
            w.a.outbox.live(None).unwrap().is_empty(),
            "{what}: nothing was queued"
        );
    }
}

#[test]
fn a_send_without_a_payload_variable_fails_and_queues_nothing() {
    let w = World::new();
    let a = w.a.boot();
    a.start_with("req1", json!({}), T0);
    a.settle(T0);
    let node = spot_dag::NodeId::new("send_request").unwrap();
    let failure = a
        .case("req1")
        .attempt(&node)
        .unwrap()
        .failure
        .clone()
        .unwrap();
    assert!(failure.contains(codes::PAYLOAD_MISSING), "{failure}");
    assert!(w.a.outbox.live(None).unwrap().is_empty());
}

#[test]
fn a_field_the_link_does_not_grant_stops_the_send_rather_than_dropping_it() {
    // A link whose grant lacks a contract field: the message cannot cross whole.
    let w = World::new();
    {
        let mut link = w.a.link.write().unwrap();
        for grant in &mut link.grants {
            grant.fields = spot_tenant_link::FieldList::only([
                spot_tenant_link::FieldName::new("bpm.message").unwrap(),
                spot_tenant_link::FieldName::new("reference").unwrap(),
            ]);
        }
    }
    let a = w.a.boot();
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    let node = spot_dag::NodeId::new("send_request").unwrap();
    let failure = a
        .case("req1")
        .attempt(&node)
        .unwrap()
        .failure
        .clone()
        .unwrap();
    assert!(failure.contains(codes::FIELD_NOT_GRANTED), "{failure}");
    assert!(w.a.outbox.live(None).unwrap().is_empty());
}

#[test]
fn a_rejected_send_takes_the_error_branch_when_the_node_waits_for_delivery() {
    use spot_bpm_conversations::WaitFor;
    // The approver has no route for the contract, so it refuses the requester as a stranger.
    let mut w = World::new();
    w.a.wait_for = WaitFor::Applied;
    w.b.route_known = false;
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    carry(&w.a, &b, T0 + SECOND);
    a.advance(T0 + 2 * SECOND);
    let case = a.case("req1");
    let node = spot_dag::NodeId::new("send_request").unwrap();
    let failure = case.attempt(&node).unwrap().failure.clone().unwrap();
    assert!(failure.contains(codes::REJECTED), "{failure}");
    assert_eq!(w.b.quarantine.held().len(), 1);
}

/// The requester with a retry on its send node, so a send that failed can be tried again.
fn retrying_requester() -> String {
    REQUESTER.replace(
        "message = \"request\"\n",
        "message = \"request\"\nretry_attempts = 3\nretry_delay_secs = 5\n",
    )
}

fn restrict_grants(side: &Side, names: &[&str]) -> Vec<spot_tenant_link::Grant> {
    let mut link = side.link.write().unwrap();
    let saved = link.grants.clone();
    for grant in &mut link.grants {
        grant.fields = spot_tenant_link::FieldList::only(
            names
                .iter()
                .map(|name| spot_tenant_link::FieldName::new(*name).unwrap()),
        );
    }
    saved
}

#[test]
fn a_send_the_grant_refuses_leaves_the_record_as_it_was_and_a_retry_after_the_fix_sends() {
    let w = World::new();
    let saved = restrict_grants(&w.a, &["bpm.message", "reference"]);
    let a = w.a.boot();
    a.start_text(
        "req1",
        &retrying_requester(),
        json!({ "bpm": { "out": { "reference": "REF-1", "amount_minor": 12_500 } } }),
        T0,
    );
    a.settle(T0);
    let record = w.a.conversations.records().pop().unwrap();
    let conversation = record.conversation.clone();
    // The refused send recorded nothing: no sent message, no step taken, no ending, no case.
    assert!(record.events.is_empty(), "{record:?}");
    assert!(record.counts.is_empty());
    assert!(record.intents.is_empty());
    assert!(record.ending.is_none());
    assert!(record.case.is_none(), "{:?}", record.case);
    assert!(w.a.outbox.live(None).unwrap().is_empty());

    // The grant is fixed; the engine's retry of the node sends the request once.
    w.a.link.write().unwrap().grants = saved;
    a.advance(T0 + 30 * SECOND);
    assert_eq!(w.a.outbox.live(None).unwrap().len(), 1);
    let record =
        w.a.conversations
            .load(&w.a.link_id(), &conversation)
            .unwrap()
            .unwrap();
    assert_eq!(record.events.len(), 1);
    assert_eq!(record.events[0].message, "request");
}

#[test]
fn a_final_message_the_grant_refuses_does_not_end_the_conversation_locally() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", T0);
    a.settle(T0);
    carry(&w.a, &b, T0 + SECOND);
    // The approver's grant no longer covers the answer's fields.
    restrict_grants(&w.b, &["bpm.message"]);
    b.advance(T0 + 2 * SECOND);
    let record = w.b.conversations.records().pop().unwrap();
    // The answer was never queued, so the record holds only the request that came in, and the
    // conversation has not ended: the peer was not told anything.
    assert!(w.b.outbox.live(None).unwrap().is_empty());
    assert_eq!(record.events.len(), 1, "{record:?}");
    assert_eq!(record.events[0].message, "request");
    assert!(record.ending.is_none(), "{:?}", record.ending);
}

/// A second request from the requester, which the contract does not allow after the first, in a
/// payload the inbox would refuse before reading it.
fn rogue_message(w: &World, conversation: &Conversation, payload: String) -> Message {
    let mut message = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0 + 2 * SECOND, [7; 10]),
            link: w.a.link_id(),
            conversation: conversation.clone(),
            seq: 2,
            topic: spot_tenant_link::Topic::new("bpm.request").unwrap(),
            schema_version: 1,
            sent_at: T0 + 2 * SECOND,
            expires_at: T0 + DAY,
            tombstone: false,
            replaces: None,
            payload: "{}".to_owned(),
        },
        &*w.a.signer,
    )
    .unwrap();
    // A peer that signs whatever it likes: the honest constructor would refuse this payload.
    message.payload = payload;
    message.sender_signature =
        spot_tenant_link::IdentitySigner::sign(&*w.a.signer, &message.signing_bytes());
    message
}

fn gate_receive(w: &World, message: &Message) -> ReceiveStatus {
    let b = w.b.boot();
    let side = &w.b;
    let inbox = Inbox::new(&*side.inbox, &*side.audit, &*side.quarantine, &*side.signer);
    let gate = ConformantInbox::new(
        &inbox,
        &*side.inbox,
        &*side.quarantine,
        &*side.signer,
        &b.runtime,
    );
    gate.receive_from(
        &side.link(),
        &connection(&w.a.party),
        message,
        T0 + 3 * SECOND,
    )
    .unwrap()
    .status
}

#[test]
fn a_payload_the_inbox_would_refuse_unread_is_never_parsed_by_the_gate() {
    let valid = r#"{"bpm.message":{"text":"request_approve@1/request"},"reference":{"text":"R2"}}"#;
    // Not canonical: whitespace. The contract would call this an out-of-order message; the
    // inbox's own check comes first and calls it an invalid payload.
    let (w, conversation) = started();
    let loose = rogue_message(&w, &conversation, valid.replace(',', ", "));
    assert_eq!(
        gate_receive(&w, &loose),
        ReceiveStatus::Rejected(RejectCode::PayloadInvalid)
    );
    assert_eq!(w.b.quarantine.held().len(), 1);

    // Oversized: a valid request padded past the limit.
    let (w, conversation) = started();
    let padding = "x".repeat(spot_link_exchange::MAX_PAYLOAD_BYTES);
    let huge = rogue_message(
        &w,
        &conversation,
        valid.replace("\"R2\"", &format!("\"{padding}\"")),
    );
    assert_eq!(
        gate_receive(&w, &huge),
        ReceiveStatus::Rejected(RejectCode::PayloadInvalid)
    );
    assert_eq!(w.b.quarantine.held().len(), 1);
}

/// A refusal is a decision, and every decision leaves one row: the reason code and the ids it
/// belongs to, never a payload value or a name the peer chose.
#[test]
fn every_gate_refusal_writes_one_audit_row_with_its_reason_and_ids() {
    let (w, conversation) = started();
    let b = w.b.boot();
    let rogue = rogue_send(
        &w.a,
        &conversation,
        "bpm.request",
        1,
        Some("request_approve@1/request"),
        &[("reference", text("R2")), ("amount_minor", int(5))],
        T0 + 2 * SECOND,
    );
    let batch = carry(&w.a, &b, T0 + 3 * SECOND);
    let rows = w.b.gate_audit.rows();
    assert_eq!(rows.len(), 1, "one row per refusal");
    assert_eq!(rows[0].code, codes::OUT_OF_ORDER);
    assert_eq!(rows[0].link, w.b.link_id());
    assert_eq!(rows[0].conversation, conversation);
    assert_eq!(rows[0].message_id, rogue);
    assert_eq!(rows[0].at_ms, T0 + 3 * SECOND);
    // Nothing the peer sent is in the row: no payload value, no field name it invented.
    let row = format!("{:?}", rows[0]);
    assert!(!row.contains("R2"), "{row}");

    // A redelivery is answered from the cache: the same reason, and no second row.
    assert_eq!(
        deliver(&b, &w.a, &batch[0], T0 + 4 * SECOND),
        ReceiveStatus::Rejected(RejectCode::OutOfOrderExpired)
    );
    assert_eq!(w.b.gate_audit.rows().len(), 1);
}

/// The link's own chain is the trail a reviewer reads, so the adapter writes the reason code
/// there beside the link id.
#[test]
fn the_link_trail_records_a_refusal_with_its_reason_code() {
    let (w, conversation) = started();
    let before = w.b.audit.entries().unwrap().len();
    let audit = spot_bpm_conversations::LinkAudit::new(w.b.audit.clone());
    audit
        .refused(&spot_bpm_conversations::Refusal {
            link: w.b.link_id(),
            conversation: conversation.clone(),
            message_id: MessageId::new_v7(T0, [3; 10]),
            code: codes::OUT_OF_ORDER,
            at_ms: T0 + 5 * SECOND,
        })
        .unwrap();
    let entries = w.b.audit.entries().unwrap();
    assert_eq!(entries.len(), before + 1);
    let last = entries.last().unwrap();
    assert_eq!(last.kind, "tenant_link.refused");
    assert_eq!(last.subject.as_ref().unwrap().id, w.b.link_id().to_string());
    // The reason travels as the record's fixed detail.
    let payload = format!("{:?}", last.payload);
    assert!(payload.contains(codes::OUT_OF_ORDER), "{payload}");
}
