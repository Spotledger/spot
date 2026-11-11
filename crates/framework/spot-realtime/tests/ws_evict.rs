//! Eviction over the wire (WP-PLT-08.T5): silent, and the connection's room count stays true.

#[allow(dead_code)]
mod support;

use spot_realtime::ws::ClientMessage;
use spot_realtime::{Principal, Room};
use support::{TEST_TOKEN, TEST_USER, connect_and_auth, doc_room_name, expect_kind, harness};

fn join(client_ref: &str, room: &str) -> ClientMessage {
    ClientMessage::Join {
        client_ref: client_ref.to_owned(),
        room: room.to_owned(),
        resume_after: None,
    }
}

#[tokio::test]
async fn an_evicted_connection_stays_open_gets_no_message_and_can_rejoin_with_a_true_count() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    let principal = Principal::user(TEST_USER);
    // The auto-joined user room is one; 199 more fill the 200-room cap.
    for i in 1..=199 {
        client
            .send_message(&join(&format!("j{i}"), &doc_room_name(i)))
            .await;
        expect_kind(&client, "joined").await;
    }
    let victim = Room::parse(&doc_room_name(1)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        harness.bus.evict_room(&harness.site, &principal, &victim),
        1
    );

    // Silent: no event of the room, no new message; the connection answers a ping, and its
    // other rooms still deliver.
    harness.bus.publish(
        &harness.site,
        &victim,
        "doc_updated",
        serde_json::json!({ "doctype": "items", "id": "x", "version": 0 }),
    );
    let kept = Room::parse(&doc_room_name(2)).unwrap_or_else(|e| panic!("{e}"));
    harness.bus.publish(
        &harness.site,
        &kept,
        "doc_updated",
        serde_json::json!({ "doctype": "items", "id": "x", "version": 1 }),
    );
    let event = expect_kind(&client, "event").await;
    assert_eq!(event["room"], doc_room_name(2));

    // The cap counts the true set: the evicted room freed a slot, so a new room joins; the
    // evicted room is not "already joined" (it is authorised again and joined afresh).
    client.send_message(&join("new", "doctype:items")).await;
    let joined = expect_kind(&client, "joined").await;
    assert_eq!(joined["ref"], "new");
    client.send_message(&join("back", &doc_room_name(1))).await;
    let error = expect_kind(&client, "join_error").await;
    assert_eq!(
        error["error"]["code"], "realtime.too_many_rooms",
        "back at the cap: the set was reconciled, not stale"
    );
    client
        .send_message(&ClientMessage::Leave {
            room: "doctype:items".to_owned(),
        })
        .await;
    client.send_message(&join("back2", &doc_room_name(1))).await;
    let joined = expect_kind(&client, "joined").await;
    assert_eq!(joined["ref"], "back2");
    harness.bus.publish(
        &harness.site,
        &victim,
        "doc_updated",
        serde_json::json!({ "doctype": "items", "id": "x", "version": 2 }),
    );
    let event = expect_kind(&client, "event").await;
    assert_eq!(event["kind"], "doc_updated");
}
