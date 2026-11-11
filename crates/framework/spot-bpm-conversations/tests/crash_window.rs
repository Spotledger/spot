//! The crash windows between stores that cannot commit together: the inbox and the conversation
//! record (a follow-up judged against a record one message behind), and the outbox and the
//! record (a message sent but not written down).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use spot_bpm_conversations::{ConversationStore, Direction, codes};
use spot_dag_exec::RunStatus;
use spot_link_exchange::{OutboxStore, ReceiveStatus, RejectCode};

/// A record one message behind must not refuse the message that follows it.
///
/// The inbox commits a message and the gate writes the conversation record afterwards. A crash
/// between the two leaves the record one message behind. Judged against that record, the
/// cancellation that follows a request looks like a message that opens a conversation, which the
/// contract does not allow — it is refused and the refusal is remembered, so the conversation can
/// never continue. With the notice feed the gate first records what the inbox already holds.
#[test]
fn a_record_one_message_behind_does_not_refuse_the_follow_up() {
    for repair in [false, true] {
        let mut w = World::with_halves(Half::Watcher);
        w.a.half = Half::Canceller;
        let a = w.a.boot();
        a.start_request("req1", "REF-1", T0);
        a.settle(T0);
        let b = w.b.boot();
        let messages = queued(&w.a, T0 + SECOND, 2);
        assert_eq!(messages.len(), 2, "a request and a cancellation");
        // The request reaches the inbox; the record write is the one the crash lost.
        assert_eq!(
            deliver_unrecorded(&b, &w.a, &messages[0], T0 + SECOND),
            ReceiveStatus::Applied
        );
        assert!(
            w.b.conversations.records().is_empty(),
            "the record is one message behind"
        );

        let status = if repair {
            deliver(&b, &w.a, &messages[1], T0 + 2 * SECOND)
        } else {
            deliver_stale(&b, &w.a, &messages[1], T0 + 2 * SECOND)
        };

        let (link, conversation) = (w.b.link_id(), messages[1].conversation.clone());
        if repair {
            assert_eq!(
                status,
                ReceiveStatus::Applied,
                "with the feed it is in order"
            );
            assert!(w.b.quarantine.held().is_empty());
            let record =
                w.b.conversations
                    .load(&link, &conversation)
                    .unwrap()
                    .unwrap();
            let said: Vec<&str> = record
                .events
                .iter()
                .map(|event| event.message.as_str())
                .collect();
            assert_eq!(
                said,
                ["request", "cancel"],
                "the predecessor was recorded first"
            );
            assert_eq!(record.events[0].direction, Direction::Received);
        } else {
            assert_eq!(
                status,
                ReceiveStatus::Rejected(RejectCode::PayloadInvalid),
                "without it, the follow-up is refused"
            );
            let held = w.b.quarantine.held();
            assert_eq!(held.len(), 1);
            assert_eq!(held[0].message.id, messages[1].id);
            assert!(
                w.b.conversations.records().is_empty(),
                "nothing was applied, so nothing was recorded"
            );
        }
    }
}

/// A message is in the outbox but not in the record: the crash window between the send and the
/// record write. The peer's reply is then judged against a history that never sent anything, so
/// it is refused as out of order and the refusal is pinned — the conversation can never finish.
/// Recording the send from the outbox before judging fixes it.
#[test]
fn a_send_the_record_lost_is_recorded_from_the_outbox_before_the_reply_is_judged() {
    for repair in [true, false] {
        let w = World::new();
        let a = w.a.boot();
        a.start_request("req1", "REF-1", T0);
        a.settle(T0);
        let (link, conversation) = conversation_for_case(&w.a, "req1");
        let before =
            w.a.conversations
                .load(&link, &conversation)
                .unwrap()
                .unwrap();
        assert_eq!(before.events.len(), 1, "the request was sent");
        assert_eq!(w.a.outbox.live(None).unwrap().len(), 1, "and is queued");

        // The crash hit after the sender took the message and before the record was written.
        spot_bpm_conversations::update(&*w.a.conversations, &link, &conversation, |record| {
            record.events.clear();
            record.counts.clear();
            if !repair {
                // Nothing is left to repair from: the decision is gone as well.
                record.intents.clear();
            }
            Ok(())
        })
        .unwrap();

        // The peer takes the request and answers it.
        let b = w.b.boot();
        carry(&w.a, &b, T0 + SECOND);
        b.advance(T0 + 2 * SECOND);
        let status = deliver(
            &a,
            &w.b,
            &queued(&w.b, T0 + 3 * SECOND, 1)[0],
            T0 + 3 * SECOND,
        );

        if repair {
            assert_eq!(status, ReceiveStatus::Applied, "the reply is in order");
            assert!(w.a.quarantine.held().is_empty());
            assert!(w.a.gate_audit.rows().is_empty(), "nothing was refused");
            let record =
                w.a.conversations
                    .load(&link, &conversation)
                    .unwrap()
                    .unwrap();
            let said: Vec<(&str, Direction)> = record
                .events
                .iter()
                .map(|event| (event.message.as_str(), event.direction))
                .collect();
            assert_eq!(
                said,
                vec![
                    ("request", Direction::Sent),
                    ("approve", Direction::Received)
                ],
                "the send was recorded from the outbox, in order"
            );
            // And the request finishes on its normal path.
            a.advance(T0 + 4 * SECOND);
            assert_eq!(a.case("req1").run.status, RunStatus::Completed);
        } else {
            assert_eq!(
                status,
                ReceiveStatus::Rejected(RejectCode::OutOfOrderExpired),
                "with nothing to record, the valid reply is refused as out of order"
            );
            assert_eq!(w.a.quarantine.held().len(), 1);
            assert_eq!(w.a.gate_audit.codes(), vec![codes::OUT_OF_ORDER]);
        }
    }
}
