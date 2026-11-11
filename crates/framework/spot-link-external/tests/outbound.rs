//! The outbound carrier: delivery states, rejection, a peer that is down for days, revocation
//! and grant limits, all through the real outbox with a fake peer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use common::*;
use spot_link_exchange::{MessageState, RejectCode};
use spot_link_external::codes;
use spot_link_external::{ChannelFailure, Verdict, classify, reject_code_for};
use spot_tenant_link::Direction;

#[tokio::test]
async fn a_2xx_acknowledges_and_the_body_the_peer_sees_is_the_platform_envelope() {
    let world = World::new();
    let id = world.enqueue("order-1", "hello", T0);
    let peer = FakePeer::answering(202);
    let report = world.pump(&peer, T0 + 10).await;
    assert_eq!((report.attempted, report.acknowledged), (1, 1));
    assert_eq!(world.state_of(&id), MessageState::Acknowledged);
    let body: serde_json::Value = serde_json::from_str(&peer.bodies()[0]).unwrap();
    assert_eq!(body["id"], id.to_string());
    assert_eq!(body["conversation"], "order-1");
    assert_eq!(body["seq"], 1);
    assert_eq!(body["topic"], "bpm.message");
    assert_eq!(body["payload"]["message.body"], "hello");
    // Acknowledged is not sent again, however long we wait.
    let again = world.pump(&peer, T0 + 10 * DAY / 100).await;
    assert_eq!(again.attempted, 0);
    assert_eq!(peer.bodies().len(), 1);
}

#[tokio::test]
async fn a_body_that_says_applied_applies_and_a_business_ack_applies_later() {
    let world = World::new();
    let direct = world.enqueue("order-1", "one", T0);
    let later = world.enqueue("order-2", "two", T0);
    let peer = FakePeer::new(vec![
        Ok(answer(200, "{\"status\":\"applied\"}")),
        Ok(answer(200, "")),
    ]);
    let report = world.pump(&peer, T0 + 10).await;
    assert_eq!((report.applied, report.acknowledged), (1, 1));
    assert_eq!(world.state_of(&direct), MessageState::Applied);
    assert_eq!(world.state_of(&later), MessageState::Acknowledged);

    // The peer later posts a signed business acknowledgement for the second message.
    let body = format!("{{\"ack\":{{\"message\":\"{later}\",\"outcome\":\"applied\"}}}}");
    let response = world
        .receive(&from_peer(&world, &body, T0 + 60_000), T0 + 60_000)
        .await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(world.state_of(&later), MessageState::Applied);

    // An acknowledgement of a message this link never sent is refused.
    let unknown = format!(
        "{{\"ack\":{{\"message\":\"{}\",\"outcome\":\"applied\"}}}}",
        spot_link_exchange::MessageId::new_v7(5, [3; 10])
    );
    let response = world
        .receive(&from_peer(&world, &unknown, T0 + 61_000), T0 + 61_000)
        .await;
    assert_eq!(response.status, 404);
}

#[tokio::test]
async fn a_4xx_rejects_with_a_code_and_the_slot_is_filled_so_the_next_message_flows() {
    let world = World::new();
    let first = world.enqueue("order-1", "bad", T0);
    let second = world.enqueue("order-1", "good", T0);
    let peer = FakePeer::new(vec![
        Ok(answer(422, "{\"why\":\"no\"}")),
        Ok(answer(200, "")),
    ]);
    let report = world.pump(&peer, T0 + 10).await;
    assert_eq!((report.rejected, report.acknowledged), (1, 1));
    assert_eq!(
        world.state_of(&first),
        MessageState::Rejected(RejectCode::PayloadInvalid)
    );
    assert_eq!(world.state_of(&second), MessageState::Acknowledged);
    // The tombstone for the rejected slot is confirmed locally, never posted to the peer.
    let later = world.pump(&peer, T0 + 5_000).await;
    assert_eq!(later.fillers_confirmed, 1);
    assert_eq!(peer.bodies().len(), 2);
    let none = world.pump(&peer, T0 + 10_000).await;
    assert_eq!((none.attempted, none.fillers_confirmed), (0, 0));
}

#[tokio::test]
async fn statuses_map_as_documented() {
    use spot_link_exchange::ReceiptOutcome::{Acknowledged, Applied, Rejected};
    assert_eq!(classify(204, ""), Verdict::Receipt(Acknowledged));
    assert_eq!(classify(200, "not json"), Verdict::Receipt(Acknowledged));
    assert_eq!(
        classify(200, "{\"status\":\"applied\"}"),
        Verdict::Receipt(Applied)
    );
    assert_eq!(
        classify(403, ""),
        Verdict::Receipt(Rejected(RejectCode::NotGranted))
    );
    assert_eq!(reject_code_for(415), RejectCode::VersionUnsupported);
    assert_eq!(reject_code_for(410), RejectCode::OutOfOrderExpired);
    assert_eq!(reject_code_for(400), RejectCode::PayloadInvalid);
    for status in [408, 425, 429, 500, 503] {
        assert_eq!(
            classify(status, ""),
            Verdict::Retry(codes::PEER_UNREACHABLE)
        );
    }
    assert_eq!(classify(302, ""), Verdict::Retry(codes::UNEXPECTED_STATUS));
}

#[tokio::test]
async fn a_peer_down_for_days_keeps_the_message_until_retention_ends() {
    let world = World::new();
    let kept = world.enqueue("order-1", "wait for me", T0);
    let down = FakePeer::new(vec![Err(ChannelFailure::unreachable())]);
    // Two days of hourly attempts: every one fails, nothing is lost, nothing is rejected.
    let mut now = T0;
    let mut retried = 0;
    while now < T0 + 2 * DAY {
        now += 3_600_000;
        retried += world.pump(&down, now).await.retried;
    }
    assert!(retried >= 47, "{retried}");
    assert_eq!(world.state_of(&kept), MessageState::Sent);

    // The peer comes back on day two and takes it.
    let up = FakePeer::answering(200);
    let report = world.pump(&up, T0 + 2 * DAY + 3_600_000).await;
    assert_eq!(report.acknowledged, 1);
    assert_eq!(world.state_of(&kept), MessageState::Acknowledged);

    // A message that waits out the whole retention expires, visibly.
    let lost = world.enqueue("order-2", "too late", T0 + 2 * DAY);
    world.pump(&down, T0 + 3 * DAY).await;
    assert_eq!(world.state_of(&lost), MessageState::Sent);
    world.pump(&down, T0 + 5 * DAY + 1).await;
    assert_eq!(world.state_of(&lost), MessageState::Expired);

    // Every failed attempt is on the link's audit trail, by code, never by content.
    let trail = format!("{:?}", world.audit.entries().unwrap());
    assert!(trail.contains(codes::PEER_UNREACHABLE) || trail.contains("refused"));
    assert!(!trail.contains("wait for me"));
}

#[tokio::test]
async fn a_late_answer_applies_a_message_that_already_expired() {
    let world = World::new();
    let id = world.enqueue("order-1", "slow", T0);
    let down = FakePeer::new(vec![Err(ChannelFailure::timed_out())]);
    world.pump(&down, T0 + 1_000).await;
    world.pump(&down, T0 + 4 * DAY).await;
    assert_eq!(world.state_of(&id), MessageState::Expired);
    let body = format!("{{\"ack\":{{\"message\":\"{id}\",\"outcome\":\"applied\"}}}}");
    let now = T0 + 4 * DAY + 1;
    assert_eq!(
        world
            .receive(&from_peer(&world, &body, now), now)
            .await
            .status,
        200
    );
    assert_eq!(world.state_of(&id), MessageState::Applied);
}

#[tokio::test]
async fn a_revoked_link_sends_nothing_and_a_topic_not_granted_cannot_be_enqueued() {
    let mut world = World::new();
    let id = world.enqueue("order-1", "held", T0);
    let other = spot_tenant_link::Topic::new("bpm.other").unwrap();
    assert!(world.enqueue_on(&other, 1, "order-1", "x", T0).is_err());
    assert!(world.enqueue_on(&topic(), 3, "order-1", "x", T0).is_err());
    world.revoke(T0 + 5);
    let peer = FakePeer::answering(200);
    let report = world.pump(&peer, T0 + 10).await;
    assert_eq!(report.attempted, 0);
    assert!(peer.bodies().is_empty());
    assert_eq!(world.state_of(&id), MessageState::Queued);
}

#[tokio::test]
async fn a_receive_only_grant_cannot_be_sent_on() {
    let world = World::with(webhook_peer(), vec![grant(&topic(), Direction::Receive)]);
    assert!(world.enqueue_on(&topic(), 1, "order-1", "x", T0).is_err());
}
