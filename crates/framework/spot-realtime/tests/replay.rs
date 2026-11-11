//! Replay for reconnecting clients: exact resume, buffer-length and age limits.

use std::sync::Arc;
use std::time::Duration;

use spot_core::{ManualClock, SiteId, SystemClock};
use spot_realtime::{
    AllowAll, BusConfig, Event, JoinOutcome, Principal, RealtimeBus, Room, Subscription,
};

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("test site name should be valid: {e}"))
}

fn room(name: &str) -> Room {
    Room::doctype(name).unwrap_or_else(|e| panic!("valid room: {e}"))
}

async fn recv(sub: &mut Subscription) -> Event {
    tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .map_or_else(
            |_| panic!("recv timed out"),
            |res| res.unwrap_or_else(|c| panic!("subscription closed unexpectedly: {c}")),
        )
}

#[tokio::test]
async fn resume_after_n_replays_exactly_the_events_after_n() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");

    // Publish while nobody is subscribed: only the replay buffer remembers them.
    for i in 1..=10u64 {
        bus.publish(&site, &feed, "doc_updated", serde_json::json!(i));
    }

    // A client that saw up to seq 7 resumes: it gets 8, 9, 10 — nothing before or after.
    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    let outcome = sub
        .join(&feed, Some(7))
        .await
        .unwrap_or_else(|e| panic!("resume join: {e}"));
    assert_eq!(outcome, JoinOutcome::Replayed { count: 3 });
    for i in 8..=10u64 {
        let event = recv(&mut sub).await;
        assert_eq!(
            event.seq, i,
            "replay delivers exactly the missed events in order"
        );
    }

    // Live events continue from the same seq.
    bus.publish(&site, &feed, "doc_updated", serde_json::json!(11));
    let event = recv(&mut sub).await;
    assert_eq!(event.seq, 11);
}

#[tokio::test]
async fn resume_after_the_latest_seq_replays_nothing() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");
    let last = bus.publish(&site, &feed, "doc_updated", serde_json::json!(1));
    bus.publish(&site, &feed, "doc_updated", serde_json::json!(2));

    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    // Resume from seq 2 (the latest): nothing is after it. JoinOutcome stays plain Joined.
    assert!(last >= 1);
    let outcome = sub
        .join(&feed, Some(2))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(outcome, JoinOutcome::Joined);
    let nothing = tokio::time::timeout(Duration::from_millis(100), sub.recv())
        .await
        .is_ok_and(|res| res.is_ok());
    assert!(!nothing, "resuming at the head replays nothing");
}

#[tokio::test]
async fn a_resume_point_older_than_the_replay_length_is_resync_required() {
    let bus = RealtimeBus::new(
        BusConfig {
            replay_len: 5,
            ..BusConfig::default()
        },
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");
    for i in 1..=20u64 {
        bus.publish(&site, &feed, "doc_updated", serde_json::json!(i));
    }
    // Only seq 16..=20 are replayable; a client at seq 12 missed dropped events.
    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    let outcome = sub
        .join(&feed, Some(12))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(
        outcome,
        JoinOutcome::ResyncRequired,
        "room must still be joined"
    );
    // Joined: future events arrive after the resync.
    bus.publish(&site, &feed, "doc_updated", serde_json::json!(21));
    let event = recv(&mut sub).await;
    assert_eq!(event.seq, 21);

    // A resume point inside the buffer still replays exactly.
    let mut sub2 = bus.subscribe(site, Principal::user("bob"));
    let outcome = sub2
        .join(&feed, Some(18))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(outcome, JoinOutcome::Replayed { count: 3 }); // 19, 20, 21
}

#[tokio::test]
async fn a_resume_point_beyond_the_room_is_resync_required() {
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");
    let seq = bus.publish(&site, &feed, "doc_updated", serde_json::json!(1));

    // No client can hold a seq this room never handed out: a resume point beyond the
    // room's counter means the room was reset under the client — it was freed and
    // recreated, or is from an older incarnation. Refetch, never a silent jump.
    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    let outcome = sub
        .join(&feed, Some(seq + 1))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(
        outcome,
        JoinOutcome::ResyncRequired,
        "a beyond-the-head resume point cannot pass as gap-free"
    );

    // The room is still joined: live events arrive right away.
    let next = bus.publish(&site, &feed, "doc_updated", serde_json::json!(2));
    let event = recv(&mut sub).await;
    assert_eq!(
        event.seq, next,
        "joined: the live stream continues without a gap"
    );
}

/// The reviewer's probe, as a regression test: empty a room, let the global sweep free
/// it, create it again. The second incarnation must never reuse a seq, and the old
/// incarnation's resume point must be `ResyncRequired` — not silently "joined".
#[tokio::test]
async fn a_freed_room_never_reuses_a_seq_and_old_resumes_resync() {
    use spot_core::Clock;

    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");

    // First incarnation: one event, seen by a subscriber who then leaves.
    let mut sub = bus.subscribe(site.clone(), Principal::user("alice"));
    sub.join(&feed, None)
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    let last = bus.publish(&site, &feed, "doc_updated", serde_json::json!(1));
    recv(&mut sub).await;
    drop(sub);
    assert_eq!(
        bus.stats().rooms,
        1,
        "replayability keeps the emptied room alive while its replay is young"
    );

    // The replay ages out entirely; the sweep frees the emptied room.
    clock.advance(Duration::from_secs(6 * 60)); // replay_max_age is 5 minutes
    assert_eq!(
        bus.stats().rooms,
        0,
        "an emptied room with aged-out replay must be freed"
    );

    // Second incarnation: the next publish resurrects the room — counting strictly
    // above everything the first incarnation handed out, never at 1 again.
    let again = bus.publish(&site, &feed, "doc_updated", serde_json::json!(2));
    assert!(
        again > last,
        "a recreated room must never reuse a seq: got {again} <= {last}"
    );

    // A reconnect carrying the old incarnation's last seq cannot be made gap-free:
    // resync (the client refetches), not a silent skip.
    let mut reconnected = bus.subscribe(site.clone(), Principal::user("bob"));
    let outcome = reconnected
        .join(&feed, Some(last))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(outcome, JoinOutcome::ResyncRequired);

    // Still joined: live events arrive in the new incarnation's counting.
    let seq = bus.publish(&site, &feed, "doc_updated", serde_json::json!(3));
    let event = recv(&mut reconnected).await;
    assert_eq!(event.seq, seq, "the live stream continues after the resync");
}

/// The reviewer's probe, kept verbatim as a regression test: publish 1..=12 on a manual
/// clock, advance past `replay_max_age` so the sweep frees the room, publish once more —
/// the recreated room counts one above the bus's high-water mark — and a client resuming at `Some(10)`
/// must NOT get a silent `Ok(Joined)`.
#[tokio::test]
async fn the_lead_probe_resuming_across_a_freed_room_is_never_a_silent_joined() {
    use spot_core::Clock;

    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");

    for i in 1..=12u64 {
        bus.publish(&site, &feed, "doc_updated", serde_json::json!(i));
    }
    clock.advance(Duration::from_secs(6 * 60)); // older than the 5-minute replay age
    assert_eq!(bus.stats().rooms, 0, "the aged-out room was freed");
    bus.publish(&site, &feed, "doc_updated", serde_json::json!(13));

    let mut sub = bus.subscribe(site, Principal::user("alice"));
    let outcome = sub
        .join(&feed, Some(10))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_ne!(
        outcome,
        JoinOutcome::Joined,
        "resuming at seq 10 across a freed-and-recreated room must never be a silent Joined"
    );
    assert_eq!(
        outcome,
        JoinOutcome::ResyncRequired,
        "and it must be resync, the only sound answer for an unrecoverable resume point"
    );
}

#[tokio::test]
async fn a_resume_point_older_than_replay_max_age_is_resync_required() {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let bus = RealtimeBus::new(
        BusConfig {
            replay_max_age: Duration::from_secs(60),
            ..BusConfig::default()
        },
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let feed = room("Item");

    // Two events at t=0; after 90s they are older than replay_max_age. A third is fresh.
    bus.publish(&site, &feed, "old1", serde_json::json!(1));
    bus.publish(&site, &feed, "old2", serde_json::json!(2));
    clock.advance(Duration::from_secs(90));
    assert_eq!(bus.publish(&site, &feed, "fresh", serde_json::json!(3)), 3);

    // A client that saw only seq 1 missed seq 2 — and seq 2 has aged out of the buffer:
    // the gap is unrecoverable, so the bus says refetch.
    let mut behind = bus.subscribe(site.clone(), Principal::user("alice"));
    let outcome = behind
        .join(&feed, Some(1))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(outcome, JoinOutcome::ResyncRequired);

    // A client that saw up to seq 2 missed only the fresh event 3, still in the buffer:
    // age of *other* events does not force a resync when nothing missed is lost.
    let mut at_edge = bus.subscribe(site, Principal::user("bob"));
    let outcome = at_edge
        .join(&feed, Some(2))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(outcome, JoinOutcome::Replayed { count: 1 });
    let event = recv(&mut at_edge).await;
    assert_eq!((event.seq, event.kind.as_str()), (3, "fresh"));
}

/// The review's integration flow: room B publishes up to N, room A publishes once (its
/// handout moves the bus-wide mark to N+1), both rooms age out and are freed, and B's
/// re-creation must seed one above everything the bus has handed out — greater than B's
/// own N — so a client resuming at B's old head N reads `ResyncRequired`, never a silent
/// `Joined` at a seq a former incarnation already handed out.
#[tokio::test]
async fn a_freed_room_reseeds_one_above_every_seq_the_bus_handed_out() {
    use spot_core::Clock;

    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let bus = RealtimeBus::new(
        BusConfig::default(),
        Arc::clone(&clock) as Arc<dyn Clock>,
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let b = room("B");
    let a = room("A");

    let n = 10u64;
    for i in 1..=n {
        assert_eq!(
            bus.publish(&site, &b, "doc_updated", serde_json::json!(i)),
            i
        );
    }
    assert_eq!(
        bus.publish(&site, &a, "doc_updated", serde_json::json!(11)),
        n + 1,
        "A's first handout continues from the bus-wide mark"
    );

    // The replay buffers age out on the manual clock and both empty rooms are freed.
    clock.advance(Duration::from_secs(6 * 60));
    assert_eq!(bus.stats().rooms, 0, "the aged-out rooms were freed");

    // B's re-creation counts one above the highest seq the bus has handed out (11),
    // which is greater than B's own old head N as well — never from 1 again.
    let recreated = bus.publish(&site, &b, "doc_updated", serde_json::json!(n + 2));
    assert_eq!(
        recreated,
        n + 2,
        "a freed room must reseed one above the bus's highest handed-out seq, got {recreated}"
    );

    // The client that saw B's old head at N must not slip silently into the fresh
    // incarnation.
    let mut sub = bus.subscribe(site, Principal::user("alice"));
    let outcome = sub
        .join(&b, Some(n))
        .await
        .unwrap_or_else(|e| panic!("join: {e}"));
    assert_eq!(outcome, JoinOutcome::ResyncRequired);
}
