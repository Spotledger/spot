//! Rooms over the bus (suite brief WP-PLT-08.T2): the happy path with the auto-joined user
//! room, joins with and without resume, the 200-room cap, permission denials that stay
//! connected, `leave` silence and repeat-join no-ops. Every server message read is validated
//! against the contract's `openapi.yaml`.

#[allow(dead_code)]
mod support;

use spot_realtime::ws::ClientMessage;
use support::{TEST_TOKEN, TEST_USER, connect_and_auth, doc_room_name, expect_kind, harness};

/// Parses a wire room name into the bus room.
fn room_for(name: &str) -> spot_realtime::Room {
    spot_realtime::Room::parse(name).unwrap_or_else(|e| panic!("test room should parse: {}", e))
}

/// The document payload shape the contract pins for document events.
fn doc_payload(doctype: &str, id: &str, version: u64) -> serde_json::Value {
    serde_json::json!({ "doctype": doctype, "id": id, "version": version })
}

#[tokio::test]
async fn the_happy_path_auths_readies_joins_and_delivers_with_seq() {
    let harness = harness().await;
    let (client, ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    assert_eq!(ready["user"], TEST_USER);
    assert_eq!(ready["heartbeat_seconds"], 25);

    // Join the doc room, no resume: `joined` with no replay field.
    let room = doc_room_name(1);
    client
        .send_message(&ClientMessage::Join {
            client_ref: "j1".to_owned(),
            room: room.clone(),
            resume_after: None,
        })
        .await;
    let joined = expect_kind(&client, "joined").await;
    assert_eq!(joined["ref"], "j1");
    assert_eq!(joined["room"], room);
    assert_eq!(joined["outcome"], "joined");
    assert!(joined.get("replayed").is_none());

    // An event published after commit reaches the connection with the room's seq.
    harness.bus.publish(
        &harness.site,
        &room_for(&room),
        "doc_updated",
        doc_payload("items", "x0", 3),
    );
    let event = expect_kind(&client, "event").await;
    assert_eq!(event["room"], room);
    assert_eq!(event["seq"], 1);
    assert_eq!(event["kind"], "doc_updated");
    assert_eq!(event["payload"], doc_payload("items", "x0", 3));
    assert!(
        event["published_at"]
            .as_str()
            .is_some_and(|stamp| stamp.ends_with("Z"))
    );
}

#[tokio::test]
async fn the_user_room_is_joined_automatically_and_delivers_notifications() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    let user_room = format!("user:{TEST_USER}");
    harness.bus.publish(
        &harness.site,
        &room_for(&user_room),
        "notification",
        serde_json::json!({ "note": "you have mail" }),
    );
    let event = expect_kind(&client, "event").await;
    assert_eq!(event["room"], user_room);
    assert_eq!(event["seq"], 1);
    assert_eq!(event["kind"], "notification");
}

#[tokio::test]
async fn app_ping_gets_pong_and_leave_is_silent() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    client.send_message(&ClientMessage::Ping).await;
    let pong = expect_kind(&client, "pong").await;
    assert_eq!(pong["type"], "pong");

    // Leaving a room the client is not in is a no-op without a reply; the connection lives.
    client
        .send_message(&ClientMessage::Leave {
            room: "doctype:items".to_owned(),
        })
        .await;
    client.send_message(&ClientMessage::Ping).await;
    let pong = expect_kind(&client, "pong").await;
    assert_eq!(pong["type"], "pong");
}

#[tokio::test]
async fn joining_a_room_twice_replies_joined_with_no_side_effects() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    let room = doc_room_name(1);
    for count in 1..=2 {
        client
            .send_message(&ClientMessage::Join {
                client_ref: format!("j{count}"),
                room: room.clone(),
                resume_after: None,
            })
            .await;
        let joined = expect_kind(&client, "joined").await;
        assert_eq!(joined["outcome"], "joined");
    }
    // Exactly one delivery per published event (a double enrolment would deliver the event
    // twice — as two messages with the same `seq`). Publish two events; the deliveries must
    // be `seq` 1 then 2, once each.
    harness.bus.publish(
        &harness.site,
        &room_for(&room),
        "doc_created",
        doc_payload("items", "x0", 1),
    );
    let first = expect_kind(&client, "event").await;
    assert_eq!(first["seq"], 1);
    harness.bus.publish(
        &harness.site,
        &room_for(&room),
        "doc_updated",
        doc_payload("items", "x0", 2),
    );
    let second = expect_kind(&client, "event").await;
    assert_eq!(second["seq"], 2);
}

#[tokio::test]
async fn resume_replays_the_missed_events_in_order() {
    let harness = harness().await;
    let room = doc_room_name(1);
    let parsed_room = room_for(&room);

    // First connection: join (fresh), see two events, then drop the connection without
    // closing the room's history.
    {
        let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
        client
            .send_message(&ClientMessage::Join {
                client_ref: "j1".to_owned(),
                room: room.clone(),
                resume_after: None,
            })
            .await;
        expect_kind(&client, "joined").await;
        for version in 1..=2u64 {
            harness.bus.publish(
                &harness.site,
                &parsed_room,
                "doc_updated",
                doc_payload("items", "x1", version),
            );
        }
        // Read both so the client really saw them; drop without a graceful close.
        let event = expect_kind(&client, "event").await;
        assert_eq!(event["seq"], 1);
        let event = expect_kind(&client, "event").await;
        assert_eq!(event["seq"], 2);
        drop(client);
    }
    // Three more events while nobody is connected (seqs 3..5).
    for version in 3..=5u64 {
        harness.bus.publish(
            &harness.site,
            &parsed_room,
            "doc_updated",
            doc_payload("items", "x1", version),
        );
    }

    // Reconnect and resume after the last seq seen: `replayed` with the three missed events,
    // in `seq` order, immediately after the `joined` reply.
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    client
        .send_message(&ClientMessage::Join {
            client_ref: "resume".to_owned(),
            room: room.clone(),
            resume_after: Some(2),
        })
        .await;
    let joined = expect_kind(&client, "joined").await;
    assert_eq!(joined["outcome"], "replayed");
    assert_eq!(joined["replayed"], 3);
    for expected_seq in 3..=5u64 {
        let event = expect_kind(&client, "event").await;
        assert_eq!(event["seq"], expected_seq, "replays run in seq order");
    }

    // And the connection continues to deliver live events after the replay.
    harness.bus.publish(
        &harness.site,
        &parsed_room,
        "doc_updated",
        doc_payload("items", "x1", 6),
    );
    let event = expect_kind(&client, "event").await;
    assert_eq!(event["seq"], 6);
}

#[tokio::test]
async fn a_resume_point_past_the_buffer_resyncs_instead_of_replaying() {
    let harness = harness().await;
    // Reconnect with a resume point the bus never handed out (past the room's head) and one
    // below the buffer's floor: both are the contract's resync case.
    harness.bus.publish(
        &harness.site,
        &room_for(&doc_room_name(9)),
        "doc_created",
        doc_payload("items", "x9", 1),
    );

    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    client
        .send_message(&ClientMessage::Join {
            client_ref: "future".to_owned(),
            room: doc_room_name(9),
            resume_after: Some(10),
        })
        .await;
    let joined = expect_kind(&client, "joined").await;
    assert_eq!(joined["outcome"], "resync_required");
    assert!(joined.get("replayed").is_none());

    // The client IS in the room: the next event arrives live.
    harness.bus.publish(
        &harness.site,
        &room_for(&doc_room_name(9)),
        "doc_updated",
        doc_payload("items", "x9", 2),
    );
    let event = expect_kind(&client, "event").await;
    assert_eq!(event["seq"], 2);
}

#[tokio::test]
async fn a_permission_denial_joins_nothing_and_the_connection_stays_open() {
    // An authorizer that refuses the salary-slip feed, admits everything else.
    struct DenySalary;
    impl spot_realtime::RoomAuthorizer for DenySalary {
        fn authorize(
            &self,
            _site: &spot_core::SiteId,
            _principal: &spot_realtime::Principal,
            room: &spot_realtime::Room,
        ) -> spot_realtime::AuthFuture<'_> {
            let denied = room.as_str().starts_with("doctype:salary_slip");
            Box::pin(std::future::ready(if denied {
                Err(spot_realtime::Denied::new("You cannot see salary slips."))
            } else {
                Ok(())
            }))
        }
    }
    let authorizer: std::sync::Arc<dyn spot_realtime::RoomAuthorizer> =
        std::sync::Arc::new(DenySalary);
    let mut harness = support::start(25, authorizer, 256).await;
    harness.admit(TEST_TOKEN, TEST_USER);

    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    client
        .send_message(&ClientMessage::Join {
            client_ref: "slip".to_owned(),
            room: "doctype:salary_slip".to_owned(),
            resume_after: None,
        })
        .await;
    let error = expect_kind(&client, "join_error").await;
    assert_eq!(error["ref"], "slip");
    assert_eq!(error["room"], "doctype:salary_slip");
    assert_eq!(error["error"]["code"], "permission.denied");

    // The connection stays open (pings still answer) and can still join elsewhere.
    client.send_message(&ClientMessage::Ping).await;
    expect_kind(&client, "pong").await;
    client
        .send_message(&ClientMessage::Join {
            client_ref: "ok".to_owned(),
            room: "doctype:items".to_owned(),
            resume_after: None,
        })
        .await;
    expect_kind(&client, "joined").await;
}

#[tokio::test]
async fn the_201st_room_fails_too_many_rooms() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    // The auto-joined user room is one of the connection's rooms; 199 more reach the cap.
    for i in 1..=199 {
        let room = doc_room_name(i);
        client
            .send_message(&ClientMessage::Join {
                client_ref: format!("j{i}"),
                room: room.clone(),
                resume_after: None,
            })
            .await;
        let joined = expect_kind(&client, "joined").await;
        assert_eq!(joined["outcome"], "joined");
    }
    // The 201st distinct room fails; the connection stays alive.
    client
        .send_message(&ClientMessage::Join {
            client_ref: "over".to_owned(),
            room: "doctype:items".to_owned(),
            resume_after: None,
        })
        .await;
    let error = expect_kind(&client, "join_error").await;
    assert_eq!(error["ref"], "over");
    assert_eq!(error["error"]["code"], "realtime.too_many_rooms");

    // Rejoining an existing room is fine (no new room), and leave frees the count.
    client
        .send_message(&ClientMessage::Join {
            client_ref: "again".to_owned(),
            room: doc_room_name(1),
            resume_after: None,
        })
        .await;
    expect_kind(&client, "joined").await;
    client
        .send_message(&ClientMessage::Leave {
            room: doc_room_name(1),
        })
        .await;
    client
        .send_message(&ClientMessage::Join {
            client_ref: "over-2".to_owned(),
            room: "doctype:items".to_owned(),
            resume_after: None,
        })
        .await;
    let joined = expect_kind(&client, "joined").await;
    assert_eq!(joined["ref"], "over-2");
}

#[tokio::test]
async fn a_room_name_outside_the_contract_closes_nothing_and_reports_unknown_room() {
    let harness = harness().await;
    let (client, _ready) = connect_and_auth(&harness, TEST_TOKEN).await;
    // `user:alice` is a room the bus grammar admits, but not a wire room: the transport
    // answers join_error instead of joining, and the connection stays open.
    client
        .send_message(&ClientMessage::Join {
            client_ref: "bad".to_owned(),
            room: "user:alice".to_owned(),
            resume_after: None,
        })
        .await;
    let error = expect_kind(&client, "join_error").await;
    assert_eq!(error["ref"], "bad");
    assert_eq!(error["room"], "user:alice");
    assert_eq!(error["error"]["code"], "realtime.unknown_room");
    client.send_message(&ClientMessage::Ping).await;
    expect_kind(&client, "pong").await;
}
