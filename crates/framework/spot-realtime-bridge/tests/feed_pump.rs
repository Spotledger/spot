//! The feed suite (WP-PLT-08.T3): mapping of committed change-feed and outbox entries to
//! room events — payloads, ordering per document, at-least-once redelivery with stable
//! event ids, resync on a gap, no field values anywhere, the position lifecycle, the lag
//! metric, and the error paths the pump must surface.

mod support;

use support::Must;

use std::sync::Arc;

use spot_core::SiteId;
use spot_realtime::{AllowAll, Room, Subscription};
use spot_realtime_bridge::{
    BridgeError, FeedPump, PositionStore, PumpConfig, PumpOutcome, RESYNC_KIND,
};
use spot_storage_contract::{ChangeOp, FeedPosition};

use support::{
    FakeFeed, FakeStore, FakeTelemetry, Log, TelemetryRecord, bus_with, changed_fields_payload,
    empty_data_payload, guard, identity_payload, next_event, node, pump, rename_payload, rx_now,
    site, stored_id, user_text, watcher,
};

fn site1() -> SiteId {
    site("acme.example")
}

fn stand() -> (RealtimeParts, FeedPump<FakeFeed, FakeTelemetry>) {
    let bus = bus_with(64, Arc::new(AllowAll));
    let site = site1();
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());
    let part = RealtimeParts {
        bus,
        site,
        feed,
        store,
        telemetry,
    };
    let p = pump(
        &part.site,
        &part.feed,
        &part.bus,
        &part.store,
        &part.telemetry,
        100,
        false,
    );
    (part, p)
}

struct RealtimeParts {
    bus: spot_realtime::RealtimeBus,
    site: SiteId,
    feed: Arc<FakeFeed>,
    store: Arc<FakeStore>,
    telemetry: Arc<FakeTelemetry>,
}

/// Every event the watchers captured, in capture order, as `(room, kind, payload)`; a
/// watcher that was closed by the bus fails the test.
async fn captured(subs: &mut [&mut Subscription]) -> Vec<(String, String, serde_json::Value)> {
    let mut out = Vec::new();
    for sub in subs {
        while let Some(result) = rx_now(sub).await {
            let event =
                result.unwrap_or_else(|closed| panic!("watcher closed unexpectedly: {closed}"));
            out.push((
                event.room.as_str().to_owned(),
                event.kind.to_owned(),
                (*event.payload).clone(),
            ));
        }
    }
    out
}

/// The error side of a pump run that must fail; names the value when it is `Ok`.
async fn pump_error(p: &FeedPump<FakeFeed, FakeTelemetry>) -> spot_realtime_bridge::BridgeError {
    match p.pump_once().await {
        Err(error) => error,
        Ok(outcome) => panic!("expected an error, got a successful run: {outcome:?}"),
    }
}

async fn run(pump: &FeedPump<FakeFeed, FakeTelemetry>) -> PumpOutcome {
    pump.pump_once()
        .await
        .unwrap_or_else(|error| panic!("pump_once failed unexpectedly: {error:?}"))
}

#[tokio::test]
async fn a_change_fans_out_to_its_doc_and_doctype_rooms() {
    let (part, p) = stand();
    let invoice_id = stored_id(42).to_string();
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 7);

    let mut w_doc = watcher(&part.bus, &part.site, "w-doc");
    let mut w_dt = watcher(&part.bus, &part.site, "w-dot");
    w_doc
        .join(&Room::document("sales_invoice", &invoice_id).must(), None)
        .await
        .must();
    w_dt.join(&Room::doctype("sales_invoice").must(), None)
        .await
        .must();

    let outcome = run(&p).await;
    assert_eq!(outcome.mapping.changes_published, 1);
    assert_eq!(outcome.mapping.room_events, 2, "one change feeds two rooms");
    assert_eq!(outcome.position.changes, FeedPosition::new(1, 1));

    let events = captured(&mut [&mut w_doc, &mut w_dt]).await;
    let rooms: Vec<_> = events.iter().map(|(room, _, _)| room.clone()).collect();
    assert!(rooms.contains(&format!("doc:sales_invoice:{invoice_id}")));
    assert!(rooms.contains(&"doctype:sales_invoice".to_owned()));
    for (_, kind, payload) in &events {
        assert_eq!(kind, "doc_created");
        assert_eq!(payload["doctype"], "sales_invoice");
        assert_eq!(payload["version"], 7, "the committed version rides along");
    }
}

#[tokio::test]
async fn payloads_carry_the_contract_vocabulary_only() {
    let (part, p) = stand();
    let invoice_id = stored_id(42).to_string();
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 3);
    part.feed.push_record(
        1,
        "doc_posted",
        "sales_invoice",
        42,
        Some(4),
        &changed_fields_payload(&["total", "number"]),
    );

    let mut w = watcher(&part.bus, &part.site, "w");
    w.join(&Room::document("sales_invoice", &invoice_id).must(), None)
        .await
        .must();
    w.join(&Room::doctype("sales_invoice").must(), None)
        .await
        .must();

    run(&p).await;

    let mut found = Vec::new();
    for _ in 0..4 {
        let event = next_event(&mut w).await;
        found.push((
            event.room.as_str().to_owned(),
            event.kind.to_owned(),
            (*event.payload).clone(),
        ));
    }

    let doc_events: Vec<_> = found
        .iter()
        .filter(|(room, _, _)| room == &format!("doc:sales_invoice:{invoice_id}"))
        .collect();
    assert_eq!(doc_events.len(), 2, "created and posted on the doc room");
    let created = doc_events
        .iter()
        .find(|(_, k, _)| k == "doc_created")
        .unwrap_or_else(|| panic!("doc_created missing"));
    assert_eq!(
        created.2,
        serde_json::json!({
            "event_id": "cf:1:1",
            "doctype": "sales_invoice",
            "id": invoice_id,
            "version": 3,
        }),
        "a change-feed event carries ids and a version, nothing more"
    );
    assert!(
        !created.2.to_string().contains("changed"),
        "a change-feed event carries no `changed` list"
    );
    let posted = doc_events
        .iter()
        .find(|(_, k, _)| k == "doc_posted")
        .unwrap_or_else(|| panic!("doc_posted missing"));
    assert_eq!(
        posted.2,
        serde_json::json!({
            "event_id": support::stored_id(801).to_string(),
            "doctype": "sales_invoice",
            "id": invoice_id,
            "version": 4,
            "changed": ["total", "number"],
        }),
        "an outbox event names the changed fields, never their values"
    );

    let dt_events: Vec<_> = found
        .iter()
        .filter(|(room, _, _)| room == "doctype:sales_invoice")
        .collect();
    assert_eq!(
        dt_events.len(),
        2,
        "both events also reach the doctype room"
    );
    assert_eq!(dt_events[0].1, "doc_created");
    assert_eq!(dt_events[1].1, "doc_posted");
}

#[tokio::test]
async fn no_event_serialises_a_field_value() {
    let (part, p) = stand();
    let invoice_id = stored_id(42).to_string();
    // Every secret-looking value the corpus guards rides in a payload the pump maps; the
    // `changed` list may name fields, never hold their values.
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 3);
    part.feed.push_record(
        1,
        "doc_updated_posted",
        "sales_invoice",
        42,
        Some(4),
        &changed_fields_payload(&["bank_account", "total"]),
    );
    part.feed.push_record(
        2,
        "doc_renamed",
        "sales_invoice",
        42,
        Some(4),
        &rename_payload("ACME Corp Q1 secret pipeline", "New Acme"),
    );
    part.feed.push_record(
        3,
        "identity_sign_in_succeeded",
        "identity_session",
        42,
        None,
        &identity_payload(&user_text(9)),
    );

    let mut w = watcher(&part.bus, &part.site, "w");
    w.join(&Room::document("sales_invoice", &invoice_id).must(), None)
        .await
        .must();
    w.join(&Room::doctype("sales_invoice").must(), None)
        .await
        .must();
    w.join(&Room::user(&user_text(9)).must(), None).await.must();

    run(&p).await;

    const CORPUS: [&str; 8] = [
        "ACME Corp",
        "Q1",
        "pipeline",
        "secret",
        "New Acme",
        "Erin",
        "4242",
        "€",
    ];
    let mut seen = 0;
    while let Some(result) = rx_now(&mut w).await {
        let event = result.must();
        seen += 1;
        let text = event.payload.to_string();
        for secretness in CORPUS {
            assert!(
                !text.contains(secretness),
                "event `{}` in room `{}` leaked `{secretness}`: {text}",
                event.kind,
                event.room.as_str()
            );
        }
        let lower = text.to_ascii_lowercase();
        assert!(!lower.contains("acme"), "a rename leaked a name: {text}");
    }
    assert!(
        seen >= 4,
        "expected the invoice, rename and notification events, saw {seen}"
    );
}

#[tokio::test]
async fn events_arrive_in_feed_order_per_document() {
    let (part, p) = stand();
    let invoice_id = stored_id(42).to_string();
    // Commit order: create (v3), update_posted (v5), rename, cancel — across both logs;
    // the doc room must deliver them in exactly this order.
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 3);
    part.feed.push_record(
        1,
        "doc_updated_posted",
        "sales_invoice",
        42,
        Some(5),
        &changed_fields_payload(&["total"]),
    );
    part.feed.push_record(
        2,
        "doc_renamed",
        "sales_invoice",
        42,
        Some(6),
        &rename_payload("a", "b"),
    );
    part.feed.push_record(
        3,
        "doc_cancelled",
        "sales_invoice",
        42,
        Some(7),
        &empty_data_payload(),
    );

    let mut w = watcher(&part.bus, &part.site, "w");
    w.join(&Room::document("sales_invoice", &invoice_id).must(), None)
        .await
        .must();

    run(&p).await;

    let mut kinds = Vec::new();
    for _ in 0..4 {
        let event = next_event(&mut w).await;
        assert_eq!(
            event.room.as_str(),
            format!("doc:sales_invoice:{invoice_id}"),
            "all four events target the document"
        );
        kinds.push(event.kind);
    }
    assert_eq!(
        kinds,
        ["doc_created", "doc_updated", "doc_renamed", "doc_cancelled"]
    );
}

#[tokio::test]
async fn a_crash_between_publish_and_save_redelivers_the_same_event_ids() {
    let (part, p) = stand();
    let invoice_id = stored_id(42).to_string();
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 3);
    part.feed.push_record(
        1,
        "doc_posted",
        "sales_invoice",
        42,
        Some(4),
        &empty_data_payload(),
    );

    let mut w = watcher(&part.bus, &part.site, "w");
    w.join(&Room::document("sales_invoice", &invoice_id).must(), None)
        .await
        .must();

    // Run 1: publishes everything, then the position save fails — the crash.
    part.store.fail_next_save();
    let error = pump_error(&p).await;
    matches!(
        error,
        BridgeError::PositionStore {
            while_doing: "save",
            ..
        }
    )
    .then_some(())
    .unwrap_or_else(|| panic!("expected a save failure, got {error:?}"));
    let first_pass = captured(&mut [&mut w]).await;

    // Run 2: the stored position is unchanged, so everything redelivers.
    let second = run(&p).await;
    assert_eq!(second.mapping.changes_published, 1);
    assert_eq!(second.mapping.outbox_published, 1);
    let second_pass = captured(&mut [&mut w]).await;

    let ids_of = |events: &[(String, String, serde_json::Value)]| -> Vec<String> {
        events
            .iter()
            .map(|(_, _, payload)| {
                payload["event_id"]
                    .as_str()
                    .unwrap_or_else(|| panic!("event_id missing in {payload}"))
                    .to_owned()
            })
            .collect()
    };
    assert_eq!(
        first_pass.len(),
        second_pass.len(),
        "redelivery republishes every event"
    );
    assert_eq!(
        ids_of(&first_pass),
        ids_of(&second_pass),
        "duplicates carry the same event id"
    );
    assert_eq!(
        first_pass.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(),
        second_pass.iter().map(|e| e.0.as_str()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn identity_topics_become_notifications_in_the_subject_user_room() {
    let (part, p) = stand();
    let greta = user_text(9);
    part.feed.push_record(
        1,
        "identity_user_created",
        "user",
        9,
        None,
        &identity_payload(&greta),
    );
    // A payload without a user: logged and skipped, never half-sent.
    part.feed.push_record(
        2,
        "identity_sign_in_failed",
        "identity_session",
        9,
        None,
        &support::payload_with_data("{}"),
    );
    // A payload that does not parse: logged and skipped.
    part.feed.push_record(
        3,
        "identity_account_locked",
        "identity_session",
        9,
        None,
        "not json",
    );
    // An app topic without a contract room: skipped.
    part.feed.push_record(
        4,
        "sales_order_shipped",
        "sales_order",
        5,
        Some(1),
        &empty_data_payload(),
    );

    let mut w = watcher(&part.bus, &part.site, "w");
    w.join(&Room::user(&greta).must(), None).await.must();

    let outcome = run(&p).await;
    assert_eq!(
        outcome.mapping.outbox_published, 1,
        "only the parseable identity event maps"
    );

    let event = next_event(&mut w).await;
    assert_eq!(event.room.as_str(), format!("user:{greta}"));
    assert_eq!(event.kind, "notification");
    assert_eq!(
        (*event.payload).clone(),
        serde_json::json!({
            "event_id": support::stored_id(801).to_string(),
            "event": "identity_user_created",
        })
    );
    assert!(
        rx_now(&mut w).await.is_none(),
        "nothing else is sent to the user room"
    );
}

#[tokio::test]
async fn a_gap_publishes_resync_and_restart_redelivers_the_retained_range() {
    let (part, p) = stand();
    // Run 1 feeds seqs 1–2 (changes) and the notification (outbox); the positions park
    // there. Seq 3 arrives while the pump is down.
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 1);
    part.feed.push_change(2, "project", 43, ChangeOp::Update, 1);
    part.feed.push_record(
        1,
        "identity_user_created",
        "user",
        9,
        None,
        &identity_payload(&user_text(9)),
    );
    run(&p).await;
    part.feed
        .push_change(3, "item_master", 44, ChangeOp::Insert, 1);

    // The gap: retention sweeps entries 1–3 (all three are gone — including the unread
    // seq 3); the saved changes position (1,2) now sits inside the swept range, so the
    // pump can vouch for nothing it has not read. On the outbox, retention sweeps the
    // record the reader read and the record that arrived after it (1–2); the saved
    // position (1,1) lost the unread seq 2 with it.
    part.feed.evict_through(Log::Changes, 3);
    part.feed.evict_through(Log::Outbox, 2);
    part.feed.push_record(
        3,
        "doc_posted",
        "sales_invoice",
        42,
        Some(2),
        &empty_data_payload(),
    );

    let mut w_invoice = watcher(&part.bus, &part.site, "w-inv");
    let mut w_project = watcher(&part.bus, &part.site, "w-prj");
    let mut w_item = watcher(&part.bus, &part.site, "w-itm");
    let mut w_user = watcher(&part.bus, &part.site, "w-usr");
    w_invoice
        .join(&Room::doctype("sales_invoice").must(), None)
        .await
        .must();
    w_project
        .join(&Room::doctype("project").must(), None)
        .await
        .must();
    w_item
        .join(&Room::doctype("item_master").must(), None)
        .await
        .must();
    w_user
        .join(&Room::user(&user_text(9)).must(), None)
        .await
        .must();

    let outcome = run(&p).await;
    assert!(outcome.mapping.changes_resynced, "the change feed resynced");
    assert!(outcome.mapping.outbox_resynced, "the outbox resynced");
    let resync_records = guard(&part.telemetry.records)
        .iter()
        .filter(|record| matches!(record, TelemetryRecord::Resync(_)))
        .count();
    assert_eq!(
        resync_records, 2,
        "one resync per log, recorded through telemetry"
    );

    // Every room fed since the last resync gets one resync marker each. `item_master`
    // never fed the rooms (seq 3 was swept unread), so it is not among them.
    let events = captured(&mut [&mut w_invoice, &mut w_project, &mut w_user]).await;
    assert_eq!(
        events
            .iter()
            .map(|(room, kind, _)| (room.clone(), kind.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("doctype:sales_invoice".to_owned(), RESYNC_KIND.to_owned()),
            ("doctype:project".to_owned(), RESYNC_KIND.to_owned()),
            (format!("user:{}", user_text(9)), RESYNC_KIND.to_owned()),
        ]
    );

    // The stored positions reset to START; the next run redelivers the retained range
    // with unchanged event ids: seq 3's change maps as itself.
    let second = run(&p).await;
    assert_eq!(
        second.mapping.changes_published, 1,
        "only the retained change (seq 3) redelivers"
    );
    assert_eq!(
        second.mapping.outbox_published, 1,
        "the retained outbox record redelivers (doc_posted)"
    );
    assert!(
        captured(&mut [&mut w_user]).await.is_empty(),
        "a swept notification is not redelivered"
    );
}

#[tokio::test]
async fn a_normal_run_after_a_resync_is_not_a_resync() {
    let (part, p) = stand();
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 1);
    run(&p).await;

    // Retention sweeps through seq 2 (the stored position (1,1) sits inside the swept
    // range — the unread seq 2 is gone with it).
    part.feed.evict_through(Log::Changes, 2);
    let resynced = run(&p).await;
    assert!(resynced.mapping.changes_resynced);

    part.feed
        .push_change(2, "customer", 51, ChangeOp::Insert, 1);
    let outcome = run(&p).await;
    assert_eq!(outcome.mapping.changes_published, 1);
    assert!(
        !outcome.mapping.changes_resynced,
        "a normal run after a resync is not a resync"
    );
    let saved = part
        .store
        .saved()
        .unwrap_or_else(|| panic!("positions saved"));
    assert_eq!(saved.changes, outcome.position.changes);
}

#[tokio::test]
async fn a_read_failure_is_surfaced_and_nothing_is_published_that_run() {
    let (part, p) = stand();
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 1);
    part.feed
        .fail_next_read(Log::Changes, "the connection died");

    let error = pump_error(&p).await;
    matches!(
        error,
        BridgeError::Feed {
            feed: "changes",
            ..
        }
    )
    .then_some(())
    .unwrap_or_else(|| panic!("expected a Feed error for `changes`, got {error:?}"));
    assert!(
        guard(&part.telemetry.records).is_empty(),
        "no run is recorded: the run never happened"
    );
    assert!(
        part.store.saves() == 0,
        "the position never advanced past the failure"
    );
}

#[tokio::test]
async fn a_position_store_load_failure_publishes_nothing() {
    let (part, p) = stand();
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 1);
    part.store.fail_next_load();

    let error = pump_error(&p).await;
    matches!(
        error,
        BridgeError::PositionStore {
            while_doing: "load",
            ..
        }
    )
    .then_some(())
    .unwrap_or_else(|| panic!("expected a load failure, got {error:?}"));
    let mut w = watcher(&part.bus, &part.site, "w");
    w.join(&Room::doctype("sales_invoice").must(), None)
        .await
        .must();
    assert!(
        rx_now(&mut w).await.is_none(),
        "nothing published on a load failure"
    );
    assert!(
        guard(&part.telemetry.records).is_empty(),
        "no run recorded: the run never happened"
    );
    assert!(part.store.saves() == 0);
}

#[tokio::test]
async fn pages_are_read_within_the_page_limit_across_runs() {
    let bus = bus_with(64, Arc::new(AllowAll));
    let site = site1();
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());
    let p = pump(&site, &feed, &bus, &store, &telemetry, 2, false);
    for seq in 1..=5 {
        feed.push_change(seq, "sales_invoice", 42 + seq as u32, ChangeOp::Insert, 1);
    }

    let mut w = watcher(&bus, &site, "w");
    w.join(&Room::doctype("sales_invoice").must(), None)
        .await
        .must();

    let first = run(&p).await;
    assert_eq!(
        first.mapping.changes_published, 2,
        "one page of two per run"
    );
    let second = run(&p).await;
    assert_eq!(second.mapping.changes_published, 2);
    let third = run(&p).await;
    assert_eq!(third.mapping.changes_published, 1);
    let rest = run(&p).await;
    assert_eq!(rest.mapping.changes_published, 0, "drained");

    let mut kinds = Vec::new();
    while let Some(result) = rx_now(&mut w).await {
        kinds.push(result.must().kind);
    }
    assert_eq!(kinds.len(), 5);
}

#[tokio::test]
async fn the_lag_metric_records_how_far_the_pump_trails_the_head() {
    let bus = bus_with(64, Arc::new(AllowAll));
    let site = site1();
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(FakeStore::new());
    let telemetry = Arc::new(FakeTelemetry::new());
    let p = pump(&site, &feed, &bus, &store, &telemetry, 100, true);

    // Nothing in the feed: the head is unknown, no lag recorded (runs are still
    // recorded; the pump never guesses a head).
    run(&p).await;
    assert!(
        lag_records(&telemetry).is_empty(),
        "the pump never guesses a head: {:#?}",
        guard(&telemetry.records)
    );

    feed.push_change(1, "sales_invoice", 42, ChangeOp::Insert, 1);
    feed.push_change(2, "sales_invoice", 43, ChangeOp::Insert, 1);
    feed.push_record(
        1,
        "identity_user_created",
        "user",
        9,
        None,
        &identity_payload(&user_text(9)),
    );
    run(&p).await;
    let lags = lag_records(&telemetry);
    assert!(
        lags.len() == 2 && lags.iter().all(|(_, behind)| *behind == 0),
        "a drained run trails nothing: {lags:?}"
    );

    feed.push_change(3, "sales_invoice", 44, ChangeOp::Insert, 1);
    feed.push_change(4, "sales_invoice", 45, ChangeOp::Insert, 1);
    run(&p).await;
    let lags = lag_records(&telemetry).split_off(lag_records(&telemetry).len().saturating_sub(2));
    assert_eq!(lags, vec![("changes", 0), ("outbox", 0)]);
}

fn lag_records(telemetry: &Arc<FakeTelemetry>) -> Vec<(&'static str, u64)> {
    guard(&telemetry.records)
        .iter()
        .filter_map(|record| match record {
            TelemetryRecord::Lag(log, behind) => Some((*log, *behind)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_outbox_lifecycle_vocabulary_maps_as_documented() {
    let (part, p) = stand();
    // The four mapped lifecycle topics.
    part.feed.push_record(
        1,
        "doc_posted",
        "sales_invoice",
        42,
        Some(2),
        &empty_data_payload(),
    );
    part.feed.push_record(
        2,
        "doc_cancelled",
        "sales_invoice",
        42,
        Some(3),
        &empty_data_payload(),
    );
    part.feed.push_record(
        3,
        "doc_renamed",
        "sales_invoice",
        42,
        Some(4),
        &rename_payload("old", "new"),
    );
    part.feed.push_record(
        4,
        "doc_updated_posted",
        "sales_invoice",
        42,
        Some(5),
        &changed_fields_payload(&["total"]),
    );
    // The two skipped lifecycle topics: the change-feed entry of the same commit already
    // carries those facts.
    part.feed.push_record(
        5,
        "doc_created",
        "sales_invoice",
        42,
        Some(1),
        &empty_data_payload(),
    );
    part.feed.push_record(
        6,
        "doc_updated",
        "sales_invoice",
        42,
        Some(2),
        &changed_fields_payload(&["total"]),
    );

    let mut w = watcher(&part.bus, &part.site, "w");
    // Both rooms the mapped events reach (the skipped records publish nothing).
    let invoice_id = stored_id(42).to_string();
    w.join(&Room::document("sales_invoice", &invoice_id).must(), None)
        .await
        .must();
    w.join(&Room::doctype("sales_invoice").must(), None)
        .await
        .must();

    let outcome = run(&p).await;
    assert_eq!(
        outcome.mapping.outbox_published, 4,
        "doc_created/doc_updated outbox records are skipped"
    );

    let mut kinds = Vec::new();
    for _ in 0..8 {
        kinds.push(next_event(&mut w).await.kind);
    }
    for kind in ["doc_posted", "doc_cancelled", "doc_renamed", "doc_updated"] {
        assert_eq!(
            kinds.iter().filter(|k| **k == kind).count(),
            2,
            "{kind} on both rooms"
        );
    }
}

#[tokio::test]
async fn the_saved_position_carries_the_fed_rooms_for_the_next_resync() {
    let (part, p) = stand();
    let greta = user_text(9);
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 1);
    part.feed.push_record(
        1,
        "identity_user_created",
        "user",
        9,
        None,
        &identity_payload(&greta),
    );
    run(&p).await;

    let saved = part.store.saved().unwrap_or_else(|| panic!("saved"));
    assert_eq!(
        saved.fed_changes.doctypes,
        ["sales_invoice".to_owned()].into_iter().collect()
    );
    assert_eq!(
        saved.fed_outbox.users,
        [greta.clone()].into_iter().collect()
    );

    // A restart (a new pump over the same store) resyncs the same rooms after a gap.
    // The watchers join first: the markers are published during the run.
    let mut w_dt = watcher(&part.bus, &part.site, "w2-dt");
    let mut w_user = watcher(&part.bus, &part.site, "w2-usr");
    w_dt.join(&Room::doctype("sales_invoice").must(), None)
        .await
        .must();
    w_user.join(&Room::user(&greta).must(), None).await.must();

    part.feed.evict_through(Log::Changes, 2);
    part.feed.evict_through(Log::Outbox, 2);
    // Retention leaves one retained entry per log for the restart to redeliver.
    part.feed
        .push_change(2, "sales_invoice", 45, ChangeOp::Insert, 2);
    part.feed.push_record(
        2,
        "identity_user_created",
        "user",
        9,
        None,
        &identity_payload(&greta),
    );
    let pump2 = FeedPump::new(
        part.site.clone(),
        Arc::clone(&part.feed),
        part.bus.clone(),
        Arc::clone(&part.store) as Arc<dyn PositionStore>,
        None,
        Arc::clone(&part.telemetry),
        PumpConfig::default(),
    );
    let outcome = pump2
        .pump_once()
        .await
        .unwrap_or_else(|error| panic!("restart pump: {error:?}"));
    assert!(outcome.mapping.changes_resynced && outcome.mapping.outbox_resynced);
    let rooms: Vec<_> = captured(&mut [&mut w_dt, &mut w_user])
        .await
        .iter()
        .map(|(room, kind, _)| (room.clone(), kind.clone()))
        .collect();
    // The restart marker reaches every room the log has fed.
    assert_eq!(
        rooms,
        vec![
            ("doctype:sales_invoice".to_owned(), RESYNC_KIND.to_owned()),
            (format!("user:{greta}"), RESYNC_KIND.to_owned()),
        ]
    );

    // The run after the restart redelivers the retained range onto the same rooms.
    let outcome = pump2
        .pump_once()
        .await
        .unwrap_or_else(|error| panic!("post-restart pump: {error:?}"));
    assert_eq!(outcome.mapping.changes_published, 1);
    assert_eq!(outcome.mapping.outbox_published, 1);
    let rooms: Vec<_> = captured(&mut [&mut w_dt, &mut w_user])
        .await
        .iter()
        .map(|(room, kind, _)| (room.clone(), kind.clone()))
        .collect();
    assert_eq!(
        rooms,
        vec![
            ("doctype:sales_invoice".to_owned(), "doc_created".to_owned()),
            (format!("user:{greta}"), "notification".to_owned()),
        ]
    );
}

#[tokio::test]
async fn feed_positions_never_regress_across_runs() {
    let (part, p) = stand();
    part.feed
        .push_change(1, "sales_invoice", 42, ChangeOp::Insert, 1);
    let first = run(&p).await;
    let second = run(&p).await; // nothing new: positions hold
    assert_eq!(
        (first.position.changes, first.position.outbox),
        (second.position.changes, second.position.outbox),
        "an empty read never moves the position"
    );
    assert_eq!(first.position.changes, FeedPosition::new(1, 1));
    assert_eq!(second.position.outbox, FeedPosition::START);
}

#[tokio::test]
async fn the_pump_serves_one_site_and_reports_it() {
    let (part, p) = stand();
    assert_eq!(*p.site(), part.site);
    let _ = node("customer", 1); // the fixtures stay exercised (see support)
}
