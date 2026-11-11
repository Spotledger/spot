//! The notice suite (WP-PLT-08.T6): the `sys_user_notice` outbox topic `HookContext::notify_user`
//! commits becomes a `notification` event in the addressee's `user:<id>` room, carrying the
//! notice's own `data`; the ADR-25 boundary refuses every other shape (counted, never sent,
//! never logged); a redelivery keeps the event id; a resync reaches the room; and the topic
//! literal and the stored `{user, payload}` shape agree with the document engine's.

mod support;

use support::Must;

use std::sync::Arc;

use spot_core::SiteId;
use spot_events_contract::{Actor, EventMeta, PayloadDocument, Topic};
use spot_realtime::{AllowAll, RealtimeBus, Room, Subscription};
use spot_realtime_bridge::{
    BridgeError, FeedPump, NOTICE_EVENT, NOTICE_MAX_BYTES, RESYNC_KIND, TOPIC_NOTICE,
};
use spot_storage_contract::{FeedPosition, OutboxEvent, RecordId, Version};
use time::OffsetDateTime;

use support::{
    FakeFeed, FakeStore, FakeTelemetry, Log, bus_with, name, next_event, node, payload_with_data,
    pump, rx_now, site, stored_id, user_text, watcher,
};

/// The one site, bus, feed, store and pump every test drives.
struct Parts {
    bus: RealtimeBus,
    site: SiteId,
    feed: Arc<FakeFeed>,
    store: Arc<FakeStore>,
    pump: FeedPump<FakeFeed, FakeTelemetry>,
}

fn stand() -> Parts {
    let bus = bus_with(64, Arc::new(AllowAll));
    let site = site("acme.example");
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());
    let pump = pump(&site, &feed, &bus, &store, &telemetry, 100, false);
    Parts {
        bus,
        site,
        feed,
        store,
        pump,
    }
}

/// The stored payload document a notice carries, `{meta, data: {user, payload}}` — the shape
/// `spot_document`'s `notice(..)` writes.
fn notice_payload(user: &str, body: &serde_json::Value) -> String {
    payload_with_data(&format!(r#"{{"user":"{user}","payload":{body}}}"#))
}

/// One `sys_user_notice` record addressing `user` with `body`.
fn push_notice(parts: &Parts, seq: u64, user: &str, body: &serde_json::Value) {
    parts.feed.push_record(
        seq,
        TOPIC_NOTICE,
        "sales_invoice",
        42,
        None,
        &notice_payload(user, body),
    );
}

async fn run(parts: &Parts) -> spot_realtime_bridge::PumpOutcome {
    parts
        .pump
        .pump_once()
        .await
        .unwrap_or_else(|error| panic!("pump_once failed unexpectedly: {error:?}"))
}

/// Every event one subscription has buffered, in capture order, as `(room, kind, payload)`.
async fn captured(sub: &mut Subscription) -> Vec<(String, String, serde_json::Value)> {
    let mut out = Vec::new();
    while let Some(result) = rx_now(sub).await {
        let event = result.unwrap_or_else(|closed| panic!("watcher closed unexpectedly: {closed}"));
        out.push((
            event.room.as_str().to_owned(),
            event.kind.to_owned(),
            (*event.payload).clone(),
        ));
    }
    out
}

/// The `event_id` of each captured event.
fn event_ids(events: &[(String, String, serde_json::Value)]) -> Vec<String> {
    events
        .iter()
        .map(|(_, _, payload)| {
            payload["event_id"]
                .as_str()
                .unwrap_or_else(|| panic!("event_id missing in {payload}"))
                .to_owned()
        })
        .collect()
}

/// `count` fields of `NOTICE_MAX_BYTES / 10`-ish size each, every string short enough for the
/// value rule: what makes one body legal-but-large and the next one oversized.
fn wide_body(count: usize) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for index in 0..count {
        map.insert(format!("f{index:02}"), serde_json::json!("x".repeat(200)));
    }
    serde_json::Value::Object(map)
}

#[tokio::test]
async fn a_notice_reaches_the_addressees_room_with_its_data() {
    let parts = stand();
    let greta = user_text(9);
    let someone_else = user_text(10);
    let body = serde_json::json!({
        "event": "invoice_posted",
        "document": stored_id(501).to_string(),
        "amount": "12.50",
        "ids": [stored_id(601).to_string(), stored_id(602).to_string()],
        "urgent": true,
    });
    push_notice(&parts, 1, &greta, &body);

    let mut w_greta = watcher(&parts.bus, &parts.site, "w-greta");
    let mut w_other = watcher(&parts.bus, &parts.site, "w-other");
    w_greta.join(&Room::user(&greta).must(), None).await.must();
    w_other
        .join(&Room::user(&someone_else).must(), None)
        .await
        .must();

    let outcome = run(&parts).await;
    assert_eq!(outcome.mapping.outbox_published, 1);
    assert_eq!(outcome.mapping.outbox_notices_rejected, 0);
    assert_eq!(
        outcome.position.outbox,
        FeedPosition::new(1, 1),
        "the notice is consumed: the position advances past it"
    );

    let events = captured(&mut w_greta).await;
    assert_eq!(events.len(), 1, "one event for the addressee: {events:?}");
    assert_eq!(events[0].0, format!("user:{greta}"));
    assert_eq!(events[0].1, "notification");
    assert_eq!(
        events[0].2,
        serde_json::json!({
            "event_id": stored_id(801).to_string(),
            "event": NOTICE_EVENT,
            "data": body,
        }),
        "the notice rides as `data`, with its own `event` name and the outbox event's id"
    );
    assert!(
        captured(&mut w_other).await.is_empty(),
        "another user's room receives nothing"
    );
}

#[tokio::test]
async fn a_payload_field_named_user_cannot_redirect_delivery() {
    let parts = stand();
    let greta = user_text(9);
    let mallory = user_text(10);
    // The committed event addresses greta; the app's payload claims mallory.
    push_notice(
        &parts,
        1,
        &greta,
        &serde_json::json!({"user": mallory.clone(), "note": "hi"}),
    );

    let mut w_greta = watcher(&parts.bus, &parts.site, "w-greta");
    let mut w_mallory = watcher(&parts.bus, &parts.site, "w-mallory");
    w_greta.join(&Room::user(&greta).must(), None).await.must();
    w_mallory
        .join(&Room::user(&mallory).must(), None)
        .await
        .must();

    run(&parts).await;

    let events = captured(&mut w_greta).await;
    assert_eq!(events.len(), 1, "the committed addressee is delivered to");
    assert_eq!(events[0].0, format!("user:{greta}"));
    assert!(
        captured(&mut w_mallory).await.is_empty(),
        "a payload field cannot name the addressee"
    );
}

#[tokio::test]
async fn redelivery_after_a_crash_between_publish_and_save_keeps_the_event_id() {
    let parts = stand();
    let greta = user_text(9);
    push_notice(
        &parts,
        1,
        &greta,
        &serde_json::json!({"document": stored_id(501).to_string()}),
    );

    let mut w = watcher(&parts.bus, &parts.site, "w");
    w.join(&Room::user(&greta).must(), None).await.must();

    // Run 1: publishes the notice, then the position save fails — the crash.
    parts.store.fail_next_save();
    match parts.pump.pump_once().await {
        Err(BridgeError::PositionStore {
            while_doing: "save",
            ..
        }) => {}
        other => panic!("expected a save failure, got {other:?}"),
    }
    let first = captured(&mut w).await;

    // Run 2: the stored position is unchanged, so the notice redelivers.
    let second_run = run(&parts).await;
    assert_eq!(second_run.mapping.outbox_published, 1);
    let second = captured(&mut w).await;

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(
        first[0].0, second[0].0,
        "the redelivery targets the same room"
    );
    assert_eq!(
        event_ids(&first),
        event_ids(&second),
        "a duplicate carries the same event id"
    );
}

#[tokio::test]
async fn payloads_outside_the_boundary_are_refused_counted_and_not_sent() {
    let parts = stand();
    let greta = user_text(9);

    // The largest accepted shape: short strings, many fields, just inside the size rule.
    let largest_ok = wide_body(9);
    let encoded = serde_json::to_vec(&largest_ok)
        .unwrap_or_else(|error| panic!("the fixture body serialises: {error}"));
    assert!(
        encoded.len() <= NOTICE_MAX_BYTES,
        "the accepted fixture is inside the boundary: {} bytes",
        encoded.len()
    );
    push_notice(&parts, 1, &greta, &largest_ok);

    // Refused: over 2 KiB serialised (every single value legal).
    let oversize = wide_body(20);
    assert!(
        serde_json::to_vec(&oversize)
            .map(|bytes| bytes.len() > NOTICE_MAX_BYTES)
            .unwrap_or(false),
        "the oversize fixture is outside the boundary"
    );
    push_notice(&parts, 2, &greta, &oversize);
    // Refused: an object below the root.
    push_notice(
        &parts,
        3,
        &greta,
        &serde_json::json!({"document": {"id": stored_id(501).to_string()}}),
    );
    // Refused: the payload is not an object at all.
    push_notice(
        &parts,
        4,
        &greta,
        &serde_json::json!(["not", "an", "object"]),
    );
    // Refused: an array inside an array is not an array of scalars.
    push_notice(&parts, 5, &greta, &serde_json::json!({"ids": [["nested"]]}));
    // Refused: a string over 256 bytes.
    push_notice(
        &parts,
        6,
        &greta,
        &serde_json::json!({"note": "n".repeat(300)}),
    );

    let mut w = watcher(&parts.bus, &parts.site, "w");
    w.join(&Room::user(&greta).must(), None).await.must();

    let outcome = run(&parts).await;
    assert_eq!(
        outcome.mapping.outbox_notices_rejected, 5,
        "every refused entry is counted"
    );
    assert_eq!(
        outcome.mapping.outbox_published, 1,
        "only the payload inside the boundary is sent"
    );
    assert_eq!(
        outcome.position.outbox,
        FeedPosition::new(1, 6),
        "every entry is consumed, refused ones included"
    );
    let events = captured(&mut w).await;
    assert_eq!(events.len(), 1, "no refused payload is ever sent");
    assert_eq!(events[0].2["data"], largest_ok);

    // A second run re-reads nothing: a refusal is counted and logged once, not every run.
    let second = run(&parts).await;
    assert_eq!(second.mapping.outbox_notices_rejected, 0);
    assert_eq!(second.mapping.outbox_published, 0);
    assert!(captured(&mut w).await.is_empty());
}

#[tokio::test]
async fn a_notice_without_a_valid_user_is_skipped() {
    let parts = stand();
    let greta = user_text(9);
    let good_body = r#"{"payload":{"note":"hi"}}"#;
    // No `user` at all.
    parts.feed.push_record(
        1,
        TOPIC_NOTICE,
        "sales_invoice",
        42,
        None,
        &payload_with_data(good_body),
    );
    // `user` is not a record id.
    parts.feed.push_record(
        2,
        TOPIC_NOTICE,
        "sales_invoice",
        42,
        None,
        &payload_with_data(r#"{"user":"not-a-record-id","payload":{"note":"hi"}}"#),
    );
    // `user` is not even text.
    parts.feed.push_record(
        3,
        TOPIC_NOTICE,
        "sales_invoice",
        42,
        None,
        &payload_with_data(r#"{"user":7,"payload":{"note":"hi"}}"#),
    );
    // The payload document does not parse.
    parts
        .feed
        .push_record(4, TOPIC_NOTICE, "sales_invoice", 42, None, "not json");

    let mut w = watcher(&parts.bus, &parts.site, "w");
    w.join(&Room::user(&greta).must(), None).await.must();

    let outcome = run(&parts).await;
    assert_eq!(outcome.mapping.outbox_notices_rejected, 4);
    assert_eq!(outcome.mapping.outbox_published, 0);
    assert!(
        captured(&mut w).await.is_empty(),
        "a malformed notice is never half-sent"
    );
}

#[tokio::test]
async fn a_resync_reaches_the_notice_room() {
    let parts = stand();
    let greta = user_text(9);
    push_notice(&parts, 1, &greta, &serde_json::json!({"note": "hi"}));
    run(&parts).await;

    let saved = parts
        .store
        .saved()
        .unwrap_or_else(|| panic!("the position was saved"));
    assert_eq!(
        saved
            .fed_outbox
            .users
            .iter()
            .cloned()
            .collect::<Vec<String>>(),
        vec![greta.clone()],
        "the notice's room is remembered for a later resync"
    );

    // Retention sweeps past the stored position: the pump can vouch for nothing it has read.
    parts.feed.evict_through(Log::Outbox, 2);
    let mut w = watcher(&parts.bus, &parts.site, "w");
    w.join(&Room::user(&greta).must(), None).await.must();

    let outcome = run(&parts).await;
    assert!(outcome.mapping.outbox_resynced, "the outbox resynced");
    let event = next_event(&mut w).await;
    assert_eq!(event.room.as_str(), format!("user:{greta}"));
    assert_eq!(event.kind, RESYNC_KIND, "the room is told to refetch");
}

/// The outbox event `spot_document`'s `notice(..)` writes: a [`PayloadDocument`] whose `data`
/// is `{user, payload}`, stored through `to_outbox_event` exactly as the engine stores it.
fn engine_notice_event(user: RecordId, body: &serde_json::Value) -> OutboxEvent {
    let document = PayloadDocument {
        meta: EventMeta {
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            actor: Actor::System {
                component: name("engine"),
            },
            command_id: None,
            causation_id: None,
            correlation_id: user,
            trace_id: None,
        },
        data: serde_json::json!({"user": user.to_string(), "payload": body}),
    };
    document
        .to_outbox_event(
            stored_id(801),
            &Topic::any(TOPIC_NOTICE).must(),
            node("sales_invoice", 42),
            Some(Version(1)),
            1,
        )
        .must()
}

#[tokio::test]
async fn the_engine_owns_the_notice_topic_and_its_stored_shape_maps() {
    assert_eq!(
        TOPIC_NOTICE,
        spot_document::engine::TOPIC_NOTICE,
        "this crate's copy of the topic literal must equal the engine's"
    );

    let greta = stored_id(9);
    let body = serde_json::json!({
        "event": "invoice_posted",
        "document": stored_id(501).to_string(),
    });
    let event = engine_notice_event(greta, &body);
    assert_eq!(
        spot_document::engine::notice_from_event(&event).must(),
        Some((greta, body.clone())),
        "the engine's own reader accepts the event this pump maps"
    );

    let parts = stand();
    parts.feed.push_record(
        1,
        event.topic.as_str(),
        "sales_invoice",
        42,
        None,
        &event.payload,
    );
    let mut w = watcher(&parts.bus, &parts.site, "w");
    w.join(&Room::user(&user_text(9)).must(), None).await.must();

    let outcome = run(&parts).await;
    assert_eq!(outcome.mapping.outbox_published, 1);
    let events = captured(&mut w).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, format!("user:{}", user_text(9)));
    assert_eq!(
        events[0].2,
        serde_json::json!({
            "event_id": stored_id(801).to_string(),
            "event": NOTICE_EVENT,
            "data": body,
        })
    );
}
