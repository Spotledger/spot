//! Both carriers through the real handlers, the real client and the T1 exchange, with no socket
//! (the loopback calls the router in process and presents the certificate TLS would).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(missing_docs)]

mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use common::net::{Flaky, client, code_of, config};
use common::*;
use spot_link_exchange::{EntryStatus, InboxStore, MessageState, NoticeFeed};
use spot_link_transport::{CarrierTable, LinkCarriers, ServerSettings, codes};
use spot_sync_http::{BreakerConfig, HttpRequest, LoopbackTransport, Transport};
use spot_tenant_link::Carrier;

fn direct() -> LinkCarriers {
    LinkCarriers::both(Carrier::Direct)
}

fn nat_side_a() -> LinkCarriers {
    // The server side: serves the mailbox for its outgoing messages, accepts pushes.
    LinkCarriers {
        send: Carrier::Pull,
        receive: Carrier::Direct,
    }
}

fn nat_side_b() -> LinkCarriers {
    // Behind NAT: dials out to push, dials out to pull.
    LinkCarriers {
        send: Carrier::Direct,
        receive: Carrier::Pull,
    }
}

fn table(carriers: LinkCarriers, world: &World) -> Arc<CarrierTable> {
    let table = Arc::new(CarrierTable::new());
    table.set(world.a.link, carriers);
    table
}

fn applied_count(node: &Node, ids: &[spot_link_exchange::MessageId]) -> usize {
    ids.iter()
        .filter(|id| {
            node.inbox
                .entry(&node.link, id)
                .unwrap()
                .is_some_and(|entry| entry.status == EntryStatus::Applied)
        })
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_direct_carrier_delivers_and_the_receipt_closes_the_loop() {
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let transport = Arc::new(LoopbackTransport::new(server_b.router(), cert_a));
    let client_a = client(&w.a, &w.b, transport, direct(), config());

    let id = w.a.enqueue("orders", "hello");
    let report = client_a.push_step().await.unwrap();
    assert_eq!((report.moved, report.receipts), (1, 1));

    assert_eq!(applied_count(&w.b, &[id]), 1);
    assert_eq!(w.a.states(), vec![MessageState::Applied]);
    let notices = w.b.inbox.read_from(0, 10).unwrap();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].message_id, id);
    // Nothing is left to send.
    assert_eq!(client_a.push_step().await.unwrap().moved, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_reply_redelivers_and_idempotency_absorbs_it() {
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let flaky = Flaky::new(Arc::new(LoopbackTransport::new(server_b.router(), cert_a)));
    // The server applies the message, then the reply is lost twice: the client retries the same
    // request and the receiver answers the repeat with the earlier outcome.
    flaky.swallow.store(2, Ordering::SeqCst);
    let client_a = client(&w.a, &w.b, flaky.clone(), direct(), config());

    let id = w.a.enqueue("orders", "once");
    let report = client_a.push_step().await.unwrap();
    assert_eq!(report.moved, 1);
    assert_eq!(flaky.forwarded.load(Ordering::SeqCst), 3);
    assert_eq!(w.a.states(), vec![MessageState::Applied]);
    assert_eq!(applied_count(&w.b, &[id]), 1);
    assert_eq!(
        w.b.inbox.read_from(0, 10).unwrap().len(),
        1,
        "one notice, however often it arrived"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_dies_after_the_server_applied_is_repaired_by_the_next_step() {
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let flaky = Flaky::new(Arc::new(LoopbackTransport::new(server_b.router(), cert_a)));
    // Every reply of the first step is lost, beyond the retries: the step fails.
    flaky.swallow.store(3, Ordering::SeqCst);
    let client_a = client(&w.a, &w.b, flaky.clone(), direct(), config());
    let id = w.a.enqueue("orders", "crash");
    assert!(client_a.push_step().await.is_err());
    assert_eq!(applied_count(&w.b, &[id]), 1, "the receiver applied it");
    assert_eq!(
        w.a.states(),
        vec![MessageState::Sent],
        "the sender never heard"
    );

    // The next step hands it over again; the receiver repeats the Applied receipt.
    let report = client_a.push_step().await.unwrap();
    assert_eq!(report.receipts, 1);
    assert_eq!(w.a.states(), vec![MessageState::Applied]);
    assert_eq!(w.b.inbox.read_from(0, 10).unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pull_carrier_fetches_and_acknowledges() {
    let w = world();
    let cert_b = certificate(2);
    let carriers_a = LinkCarriers {
        send: Carrier::Pull,
        receive: Carrier::Pull,
    };
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, table(carriers_a, &w));
    let transport = Arc::new(LoopbackTransport::new(server_a.router(), cert_b));
    let pulling = LinkCarriers {
        send: Carrier::Pull,
        receive: Carrier::Pull,
    };
    let client_b = client(&w.b, &w.a, transport, pulling, config());

    let id = w.a.enqueue("orders", "fetch me");
    let report = client_b.pull_step().await.unwrap();
    assert_eq!((report.moved, report.receipts), (1, 1));
    assert_eq!(applied_count(&w.b, &[id]), 1);
    assert_eq!(
        w.a.states(),
        vec![MessageState::Applied],
        "the ack reached the mailbox side"
    );
    assert_eq!(w.b.inbox.read_from(0, 10).unwrap().len(), 1);
    // An empty mailbox ends the long-poll with nothing.
    let started = Instant::now();
    let idle = client_b.pull_step().await.unwrap();
    assert_eq!(idle.moved, 0);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_ack_is_repaired_by_the_next_pull() {
    let w = world();
    let cert_b = certificate(2);
    let carriers = LinkCarriers {
        send: Carrier::Pull,
        receive: Carrier::Pull,
    };
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, table(carriers, &w));
    let flaky = Flaky::new(Arc::new(LoopbackTransport::new(server_a.router(), cert_b)));
    let client_b = client(&w.b, &w.a, flaky.clone(), carriers, config());
    let id = w.a.enqueue("orders", "ack me");

    // The mailbox answers and B stores the message, but the ack never reaches A.
    flaky.fail_acks.store(true, Ordering::SeqCst);
    assert!(client_b.pull_step().await.is_err());
    assert_eq!(applied_count(&w.b, &[id]), 1);
    assert_eq!(w.a.states(), vec![MessageState::Sent], "A never heard");

    // The next pull re-offers it; B answers the repeat with the earlier outcome and acks it.
    flaky.fail_acks.store(false, Ordering::SeqCst);
    let report = client_b.pull_step().await.unwrap();
    assert_eq!(report.receipts, 1);
    assert_eq!(w.a.states(), vec![MessageState::Applied]);
    assert_eq!(
        w.b.inbox.read_from(0, 10).unwrap().len(),
        1,
        "applied once, one notice"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pull_only_side_behind_nat_exchanges_both_ways_with_a_direct_peer() {
    let w = world();
    let cert_b = certificate(2);
    // A serves; B never listens. Only B's client exists.
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, table(nat_side_a(), &w));
    let transport = Arc::new(LoopbackTransport::new(server_a.router(), cert_b));
    let client_b = client(&w.b, &w.a, transport, nat_side_b(), config());

    let to_b = w.a.enqueue("orders", "a to b");
    let to_a = w.b.enqueue("orders", "b to a");

    let pushed = client_b.push_step().await.unwrap();
    let pulled = client_b.pull_step().await.unwrap();
    assert_eq!((pushed.moved, pulled.moved), (1, 1));

    assert_eq!(applied_count(&w.a, &[to_a]), 1);
    assert_eq!(applied_count(&w.b, &[to_b]), 1);
    assert_eq!(w.a.states(), vec![MessageState::Applied]);
    assert_eq!(w.b.states(), vec![MessageState::Applied]);
}

#[tokio::test(flavor = "multi_thread")]
async fn each_direction_has_its_own_carrier() {
    let w = world();
    let cert_a = certificate(1);
    // B accepts pushes but does not serve a mailbox; a pull from A is refused as carrier_disabled.
    let carriers_b = LinkCarriers {
        send: Carrier::Direct,
        receive: Carrier::Direct,
    };
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(carriers_b, &w));
    let transport = Arc::new(LoopbackTransport::new(server_b.router(), cert_a));

    let pull_everything = LinkCarriers {
        send: Carrier::Pull,
        receive: Carrier::Pull,
    };
    let client_a = client(&w.a, &w.b, transport.clone(), pull_everything, config());
    let error = client_a.pull_step().await.unwrap_err();
    assert_eq!(error.code, codes::CARRIER_DISABLED);
    // A client that is not configured to push or pull refuses locally.
    let error = client_a.push_step().await.unwrap_err();
    assert_eq!(error.code, codes::CARRIER_DISABLED);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revocation_during_a_long_poll_ends_it_and_releases_nothing() {
    let w = Arc::new(world());
    let cert_b = certificate(2);
    let carriers_a = LinkCarriers {
        send: Carrier::Pull,
        receive: Carrier::Pull,
    };
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, table(carriers_a, &w));
    let server_a = server_a.with_settings(ServerSettings {
        poll_interval: Duration::from_millis(30),
        long_poll: Duration::from_secs(20),
        ..ServerSettings::default()
    });
    let transport = Arc::new(LoopbackTransport::new(server_a.router(), cert_b));
    let mut slow = config();
    slow.pull_wait = Duration::from_secs(15);
    let client_b = Arc::new(client(&w.b, &w.a, transport, carriers_a, slow));

    let started = Instant::now();
    let polling = {
        let client_b = Arc::clone(&client_b);
        tokio::spawn(async move { client_b.pull_step().await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    w.a.revoke();
    let error = polling.await.unwrap().unwrap_err();
    assert_eq!(error.code, codes::REVOKED);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait ended at the revocation, not at its deadline"
    );
    assert!(w.b.inbox.read_from(0, 10).unwrap().is_empty());
    // The refusal is on the audit trail of the side that revoked.
    let refused =
        w.a.audit
            .events()
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == spot_tenant_link::LinkEventKind::Refused)
            .count();
    assert!(refused >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hosts_inbox_re_reads_the_link_before_recording_an_apply() {
    // Recorded security finding: the host wires its link store into the receiver, so a link revoked after a message
    // was taken in (the caller still holds the active snapshot) is refused at the apply step.
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let transport = Arc::new(LoopbackTransport::new(server_b.router(), cert_a));
    let client_a = client(&w.a, &w.b, transport, direct(), config());
    let id = w.a.enqueue("orders", "in flight");
    client_a.push_step().await.unwrap();
    let snapshot = w.b.current_link();
    w.b.revoke();

    let now = w.b.host.now_ms().unwrap();
    let error =
        w.b.host
            .inbox()
            .record_applied(&snapshot, &id, now)
            .unwrap_err();
    assert_eq!(error.code(), "tenant_link.not_active");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_link_accepts_nothing_and_says_only_that() {
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let transport = Arc::new(LoopbackTransport::new(server_b.router(), cert_a));
    let client_a = client(&w.a, &w.b, transport, direct(), config());
    let id = w.a.enqueue("orders", "late");

    w.b.revoke();
    let error = client_a.push_step().await.unwrap_err();
    assert_eq!(error.code, codes::REVOKED);
    assert_eq!(applied_count(&w.b, &[id]), 0);
    assert_eq!(w.a.states(), vec![MessageState::Sent]);
    assert!(w.b.inbox.read_from(0, 10).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_certificate_that_is_not_the_links_peer_is_refused_like_an_unknown_link() {
    let w = world();
    let cert_a = certificate(1);
    let cert_c = certificate(3);
    let (server_b, directory) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    // C is an enrolled party of B's directory, but not this link's peer.
    directory.pin(cert_c.fingerprint(), tenant(3));
    let router = server_b.router();
    let link = w.b.link;

    let as_c = LoopbackTransport::new(router.clone(), cert_c);
    let post = |link: String| {
        HttpRequest::post(format!("/link/v1/{link}/messages"), b"{}".to_vec())
            .with_content_type("application/json")
    };
    let someone_elses = as_c.request(post(link.to_string())).await.unwrap();
    let unknown = as_c
        .request(post("0192f1c0-0000-7000-8000-000000000000".to_owned()))
        .await
        .unwrap();
    assert_eq!(someone_elses.status, 403);
    assert_eq!(unknown.status, 403);
    assert_eq!(code_of(&someone_elses), codes::FORBIDDEN);
    assert_eq!(
        someone_elses.body, unknown.body,
        "the answer must not reveal whether the link exists"
    );
    // Garbage in the path is the same answer too.
    let garbage = as_c.request(post("not-a-link".to_owned())).await.unwrap();
    assert_eq!(garbage.body, unknown.body);
    // The probe left an audit record on the link it touched.
    assert!(
        w.b.audit
            .events()
            .unwrap()
            .iter()
            .any(|event| event.kind == spot_tenant_link::LinkEventKind::Refused)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_or_missing_certificate_never_reaches_the_link() {
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let router = server_b.router();
    let target = format!("/link/v1/{}/messages", w.b.link);

    let stranger = LoopbackTransport::new(router.clone(), certificate(9));
    let refused = stranger
        .request(HttpRequest::post(target.clone(), b"{}".to_vec()))
        .await
        .unwrap();
    assert_eq!(
        (refused.status, code_of(&refused).as_str()),
        (403, codes::FORBIDDEN)
    );

    // No certificate at all: call the router directly.
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let response = router
        .oneshot(Request::post(&target).body(Body::from("{}")).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    // Neither left an audit record: an outsider cannot write to the trail.
    assert!(w.b.audit.events().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_not_from_the_authenticated_peer_is_refused_and_not_stored() {
    use spot_link_exchange::{Conversation, Message, MessageParts};
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let transport = LoopbackTransport::new(server_b.router(), cert_a);
    let now = w.a.host.now_ms().unwrap();
    let id = w.a.host.ids.next_id(now);
    // B's own key signs a message that claims B as the sender, delivered by A's connection.
    let parts = MessageParts {
        id,
        link: w.b.link,
        conversation: Conversation::new("x").unwrap(),
        seq: 1,
        topic: message_topic(),
        schema_version: 1,
        sent_at: now,
        expires_at: now + 3_600_000,
        tombstone: false,
        replaces: None,
        payload: r#"{"message.body":{"text":"forged"}}"#.to_owned(),
    };
    let message = Message::sign(parts, &*w.b.signer).unwrap();
    let response = transport
        .request(
            HttpRequest::post(
                format!("/link/v1/{}/messages", w.b.link),
                serde_json::to_vec(&message).unwrap(),
            )
            .with_content_type("application/json"),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 403);
    assert_eq!(code_of(&response), codes::FORBIDDEN);
    assert!(w.b.inbox.entry(&w.b.link, &id).unwrap().is_none());
    assert!(w.b.inbox.read_from(0, 10).unwrap().is_empty());
    assert!(
        w.b.audit
            .events()
            .unwrap()
            .iter()
            .any(|event| event.kind == spot_tenant_link::LinkEventKind::Refused),
        "the refusal is audited"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn signatures_tolerate_small_clock_skew_and_refuse_large() {
    use spot_link_exchange::{Conversation, Message, MessageParts};
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let transport = LoopbackTransport::new(server_b.router(), cert_a);
    let now = w.a.host.now_ms().unwrap();

    let send = |seq: u64, skew: u64| {
        let id = w.a.host.ids.next_id(now);
        let parts = MessageParts {
            id,
            link: w.b.link,
            conversation: Conversation::new("skew").unwrap(),
            seq,
            topic: message_topic(),
            schema_version: 1,
            sent_at: now + skew,
            expires_at: now + skew + 3_600_000,
            tombstone: false,
            replaces: None,
            payload: r#"{"message.body":{"text":"tick"}}"#.to_owned(),
        };
        let message = Message::sign(parts, &*w.a.signer).unwrap();
        let request = HttpRequest::post(
            format!("/link/v1/{}/messages", w.b.link),
            serde_json::to_vec(&message).unwrap(),
        )
        .with_content_type("application/json");
        let transport = &transport;
        async move {
            let response = transport.request(request).await.unwrap();
            assert_eq!(response.status, 200);
            let reply: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            (id, reply["status"].as_str().unwrap().to_owned())
        }
    };
    // Two minutes ahead is inside the receiver's tolerance.
    let (ahead_id, status) = send(1, 120_000).await;
    assert_eq!(status, "applied");
    assert!(w.b.inbox.entry(&w.b.link, &ahead_id).unwrap().is_some());
    // An hour ahead is refused with a signed rejection, never applied.
    let (far_id, status) = send(2, 3_600_000).await;
    assert_eq!(status, "rejected");
    assert!(w.b.inbox.entry(&w.b.link, &far_id).unwrap().is_none());
    assert_eq!(w.b.quarantine.held().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn bodies_above_the_limit_are_refused_before_they_are_parsed() {
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let server_b = server_b.with_settings(ServerSettings {
        max_message_bytes: 1_024,
        ..ServerSettings::default()
    });
    let transport = LoopbackTransport::new(server_b.router(), cert_a);
    let response = transport
        .request(
            HttpRequest::post(format!("/link/v1/{}/messages", w.b.link), vec![b' '; 4_096])
                .with_content_type("application/json"),
        )
        .await
        .unwrap();
    assert_eq!(
        (response.status, code_of(&response).as_str()),
        (413, codes::TOO_LARGE)
    );
    assert!(w.b.inbox.read_from(0, 10).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_link_has_a_rate_limit_that_other_links_peers_cannot_spend() {
    let w = world();
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let server_b = server_b.with_settings(ServerSettings {
        link_rate: spot_link_transport::RateConfig {
            burst: 2,
            per_second: 1,
            max_keys: 10,
        },
        ..ServerSettings::default()
    });
    let router = server_b.router();
    let transport = LoopbackTransport::new(router.clone(), cert_a);
    let target = format!("/link/v1/{}/messages", w.b.link);
    let post = || HttpRequest::post(target.clone(), b"{}".to_vec());
    // `{}` is not a message: 400, but each request spends a token on the link.
    assert_eq!(transport.request(post()).await.unwrap().status, 400);
    assert_eq!(transport.request(post()).await.unwrap().status, 400);
    let limited = transport.request(post()).await.unwrap();
    assert_eq!(
        (limited.status, code_of(&limited).as_str()),
        (429, codes::RATE_LIMITED)
    );

    // An outsider with no pinned certificate spends nothing on the link's budget.
    let stranger = LoopbackTransport::new(router, certificate(9));
    for _ in 0..5 {
        assert_eq!(stranger.request(post()).await.unwrap().status, 403);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrency_on_a_link_is_bounded() {
    let w = world();
    let cert_b = certificate(2);
    let carriers_a = LinkCarriers {
        send: Carrier::Pull,
        receive: Carrier::Pull,
    };
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, table(carriers_a, &w));
    let server_a = server_a.with_settings(ServerSettings {
        max_concurrent_per_link: 1,
        poll_interval: Duration::from_millis(20),
        ..ServerSettings::default()
    });
    let transport = Arc::new(LoopbackTransport::new(server_a.router(), cert_b));
    let target = format!("/link/v1/{}/mailbox", w.a.link);

    let waiting = {
        let transport = Arc::clone(&transport);
        let request = HttpRequest::get(target.clone()).with_query("wait_ms=1500");
        tokio::spawn(async move { transport.request(request).await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    let second = transport
        .request(HttpRequest::get(target).with_query("wait_ms=0"))
        .await
        .unwrap();
    assert_eq!(
        (second.status, code_of(&second).as_str()),
        (503, codes::BUSY)
    );
    assert_eq!(waiting.await.unwrap().status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_peer_opens_the_breaker_then_is_probed_and_recovers() {
    use spot_core::ManualClock;
    use time::OffsetDateTime;
    let start =
        OffsetDateTime::from_unix_timestamp(i64::try_from(common_now_secs()).unwrap()).unwrap();
    let clock_a = Arc::new(ManualClock::new(start));
    let clock_b = Arc::new(ManualClock::new(start));
    let w = World::with_clocks(clock_a.clone(), clock_b.clone());
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let flaky = Flaky::new(Arc::new(LoopbackTransport::new(server_b.router(), cert_a)));
    flaky.down.store(true, Ordering::SeqCst);
    let mut fragile = config();
    fragile.breaker = BreakerConfig::new(3, Duration::from_secs(60), 1).unwrap();
    let client_a = client(&w.a, &w.b, flaky.clone(), direct(), fragile);
    let id = w.a.enqueue("orders", "waiting");

    // The peer is down: the step fails, the breaker opens at three failures.
    let first = client_a.push_step().await.unwrap_err();
    assert!(
        [codes::PEER_UNAVAILABLE, codes::CIRCUIT_OPEN].contains(&first.code),
        "{first}"
    );
    let attempts_after_first = w.a.outbox.all().unwrap()[0].attempts;
    // The next step does not touch the outbox or the network: it is told to wait.
    let second = client_a.push_step().await.unwrap_err();
    assert_eq!(second.code, codes::CIRCUIT_OPEN);
    assert_eq!(w.a.outbox.all().unwrap()[0].attempts, attempts_after_first);
    assert_eq!(flaky.forwarded.load(Ordering::SeqCst), 0);

    // The stuck peer is reported next to the stuck-message conditions.
    clock_a.advance(Duration::from_secs(120));
    let now = w.a.host.now_ms().unwrap();
    let conditions = client_a
        .conditions(now, &spot_link_exchange::Thresholds::default())
        .unwrap();
    assert!(
        conditions.iter().any(|c| c.code == "LINK_PEER_UNREACHABLE"),
        "{conditions:?}"
    );
    assert!(client_a.health().consecutive_failures >= 1);

    // After the breaker period one probe goes through; the peer is back and the queue drains.
    flaky.down.store(false, Ordering::SeqCst);
    clock_a.advance(Duration::from_secs(61));
    clock_b.advance(Duration::from_secs(181));
    // The resend delay is zero in these tests, so the message goes out at once.
    let report = client_a.push_step().await.unwrap();
    assert_eq!(report.moved, 1);
    assert_eq!(w.a.states(), vec![MessageState::Applied]);
    assert!(w.b.inbox.entry(&w.b.link, &id).unwrap().is_some());
    assert_eq!(client_a.health().consecutive_failures, 0);
    assert!(
        !client_a
            .conditions(
                w.a.host.now_ms().unwrap(),
                &spot_link_exchange::Thresholds::default()
            )
            .unwrap()
            .iter()
            .any(|c| c.code == "LINK_PEER_UNREACHABLE")
    );
}

fn common_now_secs() -> u64 {
    // A fixed instant after the handshake and before every grant expiry; both clocks start here.
    1_790_000_000
}

#[tokio::test(flavor = "multi_thread")]
async fn the_loops_move_traffic_both_ways_and_stop_on_shutdown() {
    let w = world();
    let cert_b = certificate(2);
    let (server_a, _) = server_for(&w.a, &w.b.party, &cert_b, table(nat_side_a(), &w));
    let transport = Arc::new(LoopbackTransport::new(server_a.router(), cert_b));
    let mut quick = config();
    quick.pull_wait = Duration::from_millis(50);
    let push_client = Arc::new(client(&w.b, &w.a, transport.clone(), nat_side_b(), quick));
    let pull_client = Arc::new(client(&w.b, &w.a, transport, nat_side_b(), quick));
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let pushing = {
        let (c, s) = (Arc::clone(&push_client), shutdown.clone());
        tokio::spawn(async move { c.push_loop(s).await })
    };
    let pulling = {
        let (c, s) = (Arc::clone(&pull_client), shutdown.clone());
        tokio::spawn(async move { c.pull_loop(s).await })
    };

    let mut sent = Vec::new();
    for n in 0..5 {
        sent.push((w.a.enqueue("orders", &format!("a{n}")), true));
        sent.push((w.b.enqueue("orders", &format!("b{n}")), false));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let done =
            w.a.states()
                .iter()
                .chain(w.b.states().iter())
                .all(|s| *s == MessageState::Applied);
        if done && w.a.states().len() == 5 && w.b.states().len() == 5 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the loops did not drain the queues"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let to_b: Vec<_> = sent.iter().filter(|(_, a)| *a).map(|(id, _)| *id).collect();
    let to_a: Vec<_> = sent
        .iter()
        .filter(|(_, a)| !*a)
        .map(|(id, _)| *id)
        .collect();
    assert_eq!(applied_count(&w.b, &to_b), 5);
    assert_eq!(applied_count(&w.a, &to_a), 5);
    stop.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        pushing.await.unwrap();
        pulling.await.unwrap();
    })
    .await
    .expect("both loops stop on shutdown");
}

// ADR-058 rule 6 through the host: a grant that requires the peer in a region.

async fn push_once_with(
    oracle: Option<spot_tenant_link::MemoryRegionOracle>,
    peer_region: Option<&str>,
) -> (World, spot_link_exchange::MessageId) {
    let mut w = world();
    if let (Some(oracle), Some(name)) = (&oracle, peer_region) {
        oracle
            .insert(
                w.a.party.clone(),
                spot_data_policy::Region::new(name).unwrap(),
            )
            .unwrap();
    }
    w.b.require_peer_region("eu", oracle);
    let cert_a = certificate(1);
    let (server_b, _) = server_for(&w.b, &w.a.party, &cert_a, table(direct(), &w));
    let transport = Arc::new(LoopbackTransport::new(server_b.router(), cert_a));
    let client_a = client(&w.a, &w.b, transport, direct(), config());
    let id = w.a.enqueue("orders", "hello");
    client_a.push_step().await.unwrap();
    (w, id)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_oracle_that_places_the_peer_in_the_required_region_lets_a_message_in() {
    let (w, id) = push_once_with(
        Some(spot_tenant_link::MemoryRegionOracle::new()),
        Some("eu"),
    )
    .await;
    assert_eq!(applied_count(&w.b, &[id]), 1);
    assert_eq!(w.a.states(), vec![MessageState::Applied]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_oracle_that_places_the_peer_elsewhere_refuses_and_stores_nothing() {
    let (w, id) = push_once_with(
        Some(spot_tenant_link::MemoryRegionOracle::new()),
        Some("us"),
    )
    .await;
    assert_eq!(applied_count(&w.b, &[id]), 0);
    assert!(w.b.inbox.entry(&w.b.link, &id).unwrap().is_none());
    assert!(matches!(w.a.states()[0], MessageState::Rejected(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_oracle_that_does_not_know_the_peer_refuses() {
    let (w, id) = push_once_with(Some(spot_tenant_link::MemoryRegionOracle::new()), None).await;
    assert_eq!(applied_count(&w.b, &[id]), 0);
    assert!(matches!(w.a.states()[0], MessageState::Rejected(_)));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_with_no_oracle_refuses_a_region_restricted_grant() {
    let (w, id) = push_once_with(None, None).await;
    assert_eq!(applied_count(&w.b, &[id]), 0);
    assert!(matches!(w.a.states()[0], MessageState::Rejected(_)));
}
