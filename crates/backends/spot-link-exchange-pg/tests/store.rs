//! The three ports on PostgreSQL, behaviour for behaviour against the in-memory reference:
//! outbox versions and sequence guard, events, inbox plans (store, park, unpark, reject), the
//! dedupe, restart persistence and tenant isolation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use spot_link_exchange::{
    EntryStatus, EventKind, Filler, InboxCommit, InboxStore, Message, MessageId, MessageParts,
    MessageState, NoticeDraft, NoticeFeed, OutboxEntry, OutboxStore, ParkedEntry, ProducerEvent,
    RejectCode, SeenKind,
};
use spot_tenant_link::LinkId;

fn outbox_entry(w: &World, conversation: &str, seq: u64) -> OutboxEntry {
    OutboxEntry {
        message: w.message(conversation, seq, T0),
        state: MessageState::Queued,
        state_at: T0,
        attempts: 0,
        last_sent_at: None,
        filler: None,
        version: 0,
    }
}

fn notice_for(w: &World, entry: &spot_link_exchange::InboxEntry) -> NoticeDraft {
    NoticeDraft {
        link: w.link_a.id,
        topic: entry.message.topic.clone(),
        message_id: entry.message.id,
        conversation: entry.message.conversation.clone(),
        source: w.a.clone(),
        received_at: entry.received_at,
    }
}

#[test]
fn the_outbox_guards_sequence_numbers_and_versions() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let store = e.exchange(1).outbox();

    store.insert(outbox_entry(&w, "c1", 1)).unwrap();
    // The sequence number must be the conversation's next.
    assert!(matches!(
        store.insert(outbox_entry(&w, "c1", 3)),
        Err(spot_link_exchange::Error::Conflict { .. })
    ));
    assert!(matches!(
        store.insert(outbox_entry(&w, "c1", 1)),
        Err(spot_link_exchange::Error::Conflict { .. })
    ));
    store.insert(outbox_entry(&w, "c1", 2)).unwrap();
    store.insert(outbox_entry(&w, "c2", 1)).unwrap();
    assert_eq!(
        store.last_seq(&w.link_a.id, &conversation("c1")).unwrap(),
        2
    );
    assert_eq!(
        store.last_seq(&w.link_a.id, &conversation("none")).unwrap(),
        0
    );

    // Versions: stored at 1, a stale update is refused, an update bumps.
    let live = store.live(Some(&w.link_a.id)).unwrap();
    assert_eq!(live.len(), 3);
    assert_eq!(
        live.iter().map(|e| e.message.seq).collect::<Vec<_>>(),
        vec![1, 2, 1],
        "ordered by conversation then sequence"
    );
    let mut first = live[0].clone();
    assert_eq!(first.version, 1);
    first.state = MessageState::Sent;
    first.state_at = T0 + 5;
    first.attempts = 1;
    store.update(first.clone()).unwrap();
    assert!(matches!(
        store.update(first.clone()),
        Err(spot_link_exchange::Error::Conflict { .. })
    ));
    let read = store.find(&first.message.id).unwrap().unwrap();
    assert_eq!(read.version, 2);
    assert_eq!(read.state, MessageState::Sent);
    // An unknown entry is NotFound, not a conflict.
    let mut ghost = outbox_entry(&w, "c9", 1);
    ghost.version = 1;
    assert!(matches!(
        store.update(ghost),
        Err(spot_link_exchange::Error::NotFound)
    ));
}

#[test]
fn a_settled_entry_leaves_the_live_list_and_its_filler_is_found_by_id() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let store = e.exchange(1).outbox();
    store.insert(outbox_entry(&w, "c1", 1)).unwrap();
    let mut entry = store.live(None).unwrap().remove(0);
    let filler_message = w.message("c1", 1, T0);
    entry.state = MessageState::Expired;
    entry.filler = Some(Filler {
        message: filler_message.clone(),
        acknowledged: false,
        last_sent_at: None,
    });
    store.update(entry.clone()).unwrap();
    // Expired but its filler is unconfirmed: still live, findable by either id.
    assert_eq!(store.live(None).unwrap().len(), 1);
    let by_filler = store.find(&filler_message.id).unwrap().unwrap();
    assert_eq!(by_filler.message.id, entry.message.id);

    let mut again = store.find(&entry.message.id).unwrap().unwrap();
    again.filler.as_mut().unwrap().acknowledged = true;
    store.update(again).unwrap();
    assert!(store.live(None).unwrap().is_empty());
    assert!(store.find(&entry.message.id).unwrap().is_some());
}

#[test]
fn producer_events_are_numbered_gaplessly_and_read_after_a_position() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let store = e.exchange(1).outbox();
    for n in 0..5u64 {
        let message = w.message("c1", n + 1, T0);
        store
            .push_event(ProducerEvent {
                message: message.id,
                link: w.link_a.id,
                conversation: message.conversation.clone(),
                seq: n + 1,
                topic: message.topic.clone(),
                kind: if n % 2 == 0 {
                    EventKind::Applied
                } else {
                    EventKind::Rejected(RejectCode::PayloadInvalid)
                },
                at: T0 + n,
            })
            .unwrap();
    }
    let all = store.events_from(0, 100).unwrap();
    assert_eq!(
        all.iter().map(|(p, _)| *p).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    let tail = store.events_from(3, 100).unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(
        tail[0].1.kind,
        EventKind::Rejected(RejectCode::PayloadInvalid)
    );
    assert_eq!(store.events_from(0, 2).unwrap().len(), 2);
}

#[test]
fn an_inbox_plan_stores_parks_unparks_and_rejects_atomically() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let ex = e.exchange(2);
    let inbox = ex.inbox();
    let feed = ex.notices();
    let conv = conversation("c1");

    // Nothing known yet.
    let view = inbox.conversation(&w.link_a.id, &conv).unwrap();
    assert_eq!((view.next_seq, view.parked.len()), (1, 0));

    // Message 3 arrives ahead of the gap and is parked.
    let m1 = w.message("c1", 1, T0);
    let m2 = w.message("c1", 2, T0);
    let m3 = w.message("c1", 3, T0);
    let mut park = InboxCommit::new(w.link_a.id, conv.clone(), 1);
    park.park.push(ParkedEntry {
        message: m3.clone(),
        parked_at: T0,
    });
    inbox.commit(park).unwrap();
    assert!(matches!(
        inbox.seen(&w.link_a.id, &m3.id).unwrap(),
        Some(SeenKind::Parked)
    ));
    assert_eq!(inbox.parked().unwrap().len(), 1);
    assert_eq!(
        inbox
            .conversation(&w.link_a.id, &conv)
            .unwrap()
            .parked
            .len(),
        1
    );

    // Message 1 and 2 arrive; the plan stores them, releases 3 and advances the cursor, and the
    // three notices commit with it.
    let mut plan = InboxCommit::new(w.link_a.id, conv.clone(), 1);
    plan.next_seq = 4;
    for m in [&m1, &m2, &m3] {
        let entry = w.entry(m.clone(), T0 + 10);
        plan.notices.push(notice_for(&w, &entry));
        plan.store.push(entry);
    }
    plan.unpark.push(m3.id);
    inbox.commit(plan).unwrap();
    assert!(inbox.parked().unwrap().is_empty());
    assert_eq!(inbox.conversation(&w.link_a.id, &conv).unwrap().next_seq, 4);
    for m in [&m1, &m2, &m3] {
        assert_eq!(
            inbox.seen(&w.link_a.id, &m.id).unwrap(),
            Some(SeenKind::Stored(EntryStatus::Applied))
        );
    }
    let stored = inbox.entry(&w.link_a.id, &m2.id).unwrap().unwrap();
    assert_eq!(stored.message.seq, 2);
    let notices = feed.read_from(0, 10).unwrap();
    assert_eq!(
        notices.iter().map(|n| n.cursor).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(notices[0].message_id, m1.id);

    // A stale plan (expected_next no longer matches) is a conflict and changes nothing.
    let mut stale = InboxCommit::new(w.link_a.id, conv.clone(), 1);
    stale.next_seq = 2;
    let m4 = w.message("c1", 1, T0);
    stale.store.push(w.entry(m4.clone(), T0));
    assert!(matches!(
        inbox.commit(stale),
        Err(spot_link_exchange::Error::Conflict { .. })
    ));
    assert!(inbox.seen(&w.link_a.id, &m4.id).unwrap().is_none());

    // A message already known cannot be planned again.
    let mut repeat = InboxCommit::new(w.link_a.id, conv.clone(), 4);
    repeat.next_seq = 5;
    repeat.store.push(w.entry(m1.clone(), T0));
    assert!(matches!(
        inbox.commit(repeat),
        Err(spot_link_exchange::Error::Conflict { .. })
    ));

    // A rejection is remembered with its code and moves nothing else.
    let bad = w.message("c1", 4, T0);
    let mut reject = InboxCommit::new(w.link_a.id, conv.clone(), 4);
    reject
        .rejected
        .push((bad.id, w.link_a.id, RejectCode::NotGranted));
    inbox.commit(reject).unwrap();
    assert_eq!(
        inbox.seen(&w.link_a.id, &bad.id).unwrap(),
        Some(SeenKind::Rejected(RejectCode::NotGranted))
    );
}

#[test]
fn a_parked_tombstone_slot_is_consumed_and_answered_as_applied() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let inbox = e.exchange(2).inbox();
    let conv = conversation("c1");
    let filler = w.message("c1", 2, T0);
    let mut park = InboxCommit::new(w.link_a.id, conv.clone(), 1);
    park.park.push(ParkedEntry {
        message: filler.clone(),
        parked_at: T0,
    });
    inbox.commit(park).unwrap();
    // The gap closes with message 1; the parked one leaves without being stored.
    let m1 = w.message("c1", 1, T0);
    let mut plan = InboxCommit::new(w.link_a.id, conv, 1);
    plan.next_seq = 3;
    plan.store.push(w.entry(m1, T0));
    plan.unpark.push(filler.id);
    inbox.commit(plan).unwrap();
    assert_eq!(
        inbox.seen(&w.link_a.id, &filler.id).unwrap(),
        Some(SeenKind::Stored(EntryStatus::Applied))
    );
}

#[test]
fn a_pending_record_finishes_once_and_its_notice_comes_with_it() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let ex = e.exchange(2);
    let inbox = ex.inbox();
    let conv = conversation("c1");
    let m1 = w.message("c1", 1, T0);
    let mut plan = InboxCommit::new(w.link_a.id, conv, 1);
    plan.next_seq = 2;
    let mut entry = w.entry(m1.clone(), T0);
    entry.status = EntryStatus::PendingApply;
    plan.store.push(entry.clone());
    inbox.commit(plan).unwrap();
    assert!(
        ex.notices().read_from(0, 10).unwrap().is_empty(),
        "no notice before apply"
    );

    let pending = inbox.pending_records(10).unwrap();
    assert_eq!(pending.len(), 1);
    let done = inbox
        .finish_record(
            &m1.id,
            &w.link_a.id,
            EntryStatus::Applied,
            Some(notice_for(&w, &entry)),
        )
        .unwrap()
        .unwrap();
    assert_eq!(done.status, EntryStatus::Applied);
    assert_eq!(ex.notices().read_from(0, 10).unwrap().len(), 1);
    assert_eq!(
        inbox.seen(&w.link_a.id, &m1.id).unwrap(),
        Some(SeenKind::Stored(EntryStatus::Applied))
    );
    // A second finish, or one for the wrong link, changes nothing.
    assert!(
        inbox
            .finish_record(
                &m1.id,
                &w.link_a.id,
                EntryStatus::Applied,
                Some(notice_for(&w, &entry))
            )
            .unwrap()
            .is_none()
    );
    assert!(inbox.pending_records(10).unwrap().is_empty());
    assert_eq!(ex.notices().read_from(0, 10).unwrap().len(), 1);
}

#[test]
fn the_notice_feed_is_at_least_once_and_acks_only_move_forward() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let ex = e.exchange(2);
    let inbox = ex.inbox();
    let feed = ex.notices();
    let conv = conversation("c1");
    let mut plan = InboxCommit::new(w.link_a.id, conv, 1);
    plan.next_seq = 4;
    for seq in 1..=3 {
        let entry = w.entry(w.message("c1", seq, T0), T0);
        plan.notices.push(notice_for(&w, &entry));
        plan.store.push(entry);
    }
    inbox.commit(plan).unwrap();

    assert_eq!(feed.acked("process-engine").unwrap(), 0);
    let first = feed
        .read_from(feed.acked("process-engine").unwrap(), 2)
        .unwrap();
    assert_eq!(first.len(), 2);
    // The consumer crashes before acking: the same notices come again.
    let again = feed
        .read_from(feed.acked("process-engine").unwrap(), 2)
        .unwrap();
    assert_eq!(first, again);
    feed.ack("process-engine", first[1].cursor).unwrap();
    feed.ack("process-engine", 1).unwrap();
    assert_eq!(
        feed.acked("process-engine").unwrap(),
        2,
        "an older ack changes nothing"
    );
    let rest = feed
        .read_from(feed.acked("process-engine").unwrap(), 10)
        .unwrap();
    assert_eq!(rest.iter().map(|n| n.cursor).collect::<Vec<_>>(), vec![3]);
    assert_eq!(feed.acked("someone-else").unwrap(), 0);
}

#[test]
fn state_survives_a_restart_and_tenants_see_only_their_own_rows() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let one = e.exchange(1);
    let two = e.exchange(2);
    one.outbox().insert(outbox_entry(&w, "c1", 1)).unwrap();
    let mut plan = InboxCommit::new(w.link_a.id, conversation("c1"), 1);
    plan.next_seq = 2;
    let entry = w.entry(w.message("c1", 1, T0), T0);
    plan.notices.push(notice_for(&w, &entry));
    plan.store.push(entry.clone());
    one.inbox().commit(plan).unwrap();

    // A fresh pool, as after a restart, reads the same rows.
    let restarted = e.restarted(1);
    assert_eq!(restarted.outbox().live(None).unwrap().len(), 1);
    assert!(
        restarted
            .inbox()
            .entry(&w.link_a.id, &entry.message.id)
            .unwrap()
            .is_some()
    );
    assert_eq!(restarted.notices().read_from(0, 10).unwrap().len(), 1);

    // Tenant 2's schema holds none of it, even asked for by id.
    assert!(two.outbox().live(None).unwrap().is_empty());
    assert!(
        two.inbox()
            .entry(&w.link_a.id, &entry.message.id)
            .unwrap()
            .is_none()
    );
    assert!(
        two.inbox()
            .seen(&w.link_a.id, &entry.message.id)
            .unwrap()
            .is_none()
    );
    assert!(two.notices().read_from(0, 10).unwrap().is_empty());
    assert_eq!(e.count(2, "spot_link_notice"), 0);
    assert_eq!(e.count(1, "spot_link_notice"), 1);
}

#[test]
fn two_links_may_hold_the_same_message_id_in_one_schema() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let inbox = e.exchange(1).inbox();
    // One id, two links: the dedupe key is (link, id), so the second link neither collides with
    // nor overwrites the first.
    let other = LinkId::new_v7(1_700_000_000_000, [21; 10]);
    let id = MessageId::new_v7(T0, [42; 10]);
    let signed = |link: LinkId| {
        Message::sign(
            MessageParts {
                id,
                link,
                conversation: conversation("both"),
                seq: 1,
                topic: message_topic(),
                schema_version: 1,
                sent_at: T0,
                expires_at: T0 + 3 * DAY,
                tombstone: false,
                replaces: None,
                payload: r#"{"message.body":{"text":"both"}}"#.to_owned(),
            },
            &w.signer_a,
        )
        .unwrap()
    };
    for link in [w.link_a.id, other] {
        let mut plan = InboxCommit::new(link, conversation("both"), 1);
        plan.next_seq = 2;
        plan.store.push(w.entry(signed(link), T0));
        inbox.commit(plan).unwrap();
    }
    for link in [w.link_a.id, other] {
        assert_eq!(
            inbox.seen(&link, &id).unwrap(),
            Some(SeenKind::Stored(EntryStatus::Applied))
        );
        assert_eq!(inbox.entry(&link, &id).unwrap().unwrap().message.link, link);
    }
    // An id neither link used is still unknown on both.
    let unused = MessageId::new_v7(T0, [43; 10]);
    assert!(inbox.seen(&w.link_a.id, &unused).unwrap().is_none());
    assert!(inbox.seen(&other, &unused).unwrap().is_none());
}

#[test]
fn an_entry_and_its_producer_event_settle_in_one_transaction() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let store = e.exchange(1).outbox();
    let mut entry = outbox_entry(&w, "settle", 1);
    store.insert(entry.clone()).unwrap();
    let stored = store.find(&entry.message.id).unwrap().unwrap();
    entry.version = stored.version;
    entry.state = MessageState::Expired;
    entry.state_at = T0 + DAY;
    let event = ProducerEvent {
        message: entry.message.id,
        link: entry.message.link,
        conversation: entry.message.conversation.clone(),
        seq: entry.message.seq,
        topic: entry.message.topic.clone(),
        kind: EventKind::Expired,
        at: T0 + DAY,
    };
    store.settle(entry.clone(), Some(event.clone())).unwrap();
    // One transaction: both rows are there.
    assert_eq!(
        store.find(&entry.message.id).unwrap().unwrap().state,
        MessageState::Expired
    );
    assert_eq!(store.events_from(0, 10).unwrap().len(), 1);

    // A losing settle (the entry already moved) writes neither the state nor a second event.
    let mut stale = entry.clone();
    stale.state = MessageState::Applied;
    assert!(matches!(
        store.settle(stale, Some(event)),
        Err(spot_link_exchange::Error::Conflict { .. })
    ));
    assert_eq!(
        store.find(&entry.message.id).unwrap().unwrap().state,
        MessageState::Expired
    );
    assert_eq!(store.events_from(0, 10).unwrap().len(), 1);
}

#[test]
fn a_purged_row_keeps_the_digest_and_signature_it_had() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let w = World::new();
    let inbox = e.exchange(1).inbox();
    let message = w.message("purge", 1, T0);
    let mut plan = InboxCommit::new(w.link_a.id, conversation("purge"), 1);
    plan.next_seq = 2;
    plan.store.push(w.entry(message.clone(), T0));
    inbox.commit(plan).unwrap();

    assert_eq!(inbox.purge_applied(T0, T0 + DAY, 10).unwrap(), 1);
    let stored = inbox.entry(&w.link_a.id, &message.id).unwrap().unwrap();
    assert_eq!(stored.message.payload, "{}");
    let marker = inbox.purged(&w.link_a.id, &message.id).unwrap().unwrap();
    assert_eq!(
        marker.digest,
        spot_link_exchange::PurgedPayload::digest_hex(&message.payload_digest())
    );
    assert_eq!(marker.signature, message.sender_signature);
    assert_eq!(marker.purged_at, T0 + DAY);
    // Once only, and an id that was never purged answers with nothing.
    assert_eq!(inbox.purge_applied(T0, T0 + 2 * DAY, 10).unwrap(), 0);
    let clean = w.message("clean", 1, T0);
    assert!(inbox.purged(&w.link_a.id, &clean.id).unwrap().is_none());
}
