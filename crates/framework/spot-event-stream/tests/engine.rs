//! The engine's delivery guarantees: per-partition order, at-least-once, ack-before-advance,
//! resync, one-connection fencing, window flow control and parked-consumer isolation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_event_stream::{
    BatchPolicy, ConsumerSpec, ConsumerStore, DenyStreamRights, ParkPolicy, PayloadMode, Start,
    StreamError, TopicSelector,
};
use spot_events_contract::envelope::Position;
use spot_permissions::Principal;
use spot_storage_contract::RecordId;
use support::{doc_record, fixture, fixture_full, rebuild, rid};

const BIG: BatchPolicy = BatchPolicy {
    max_events: 20_000,
    max_bytes: 64 * 1024 * 1024,
    window: 20_000,
    page_limit: 20_000,
    max_wait: std::time::Duration::from_secs(25),
};

fn spec(name: &str, partitions: u32) -> ConsumerSpec {
    ConsumerSpec {
        name: name.to_owned(),
        owner: "owner".to_owned(),
        topics: vec![TopicSelector::Prefix("doc".to_owned())],
        filter: None,
        partitions,
        payload_mode: PayloadMode::Fields,
        start: Start::FromBeginning,
    }
}

async fn drain(
    engine: &spot_event_stream::StreamEngine<support::FakeFeed>,
    lease: &spot_event_stream::ConsumerLease,
    partition: u32,
) -> Vec<RecordId> {
    let mut out = Vec::new();
    loop {
        let batch = engine.pull(lease, partition, None, false).await.unwrap();
        if batch.events.is_empty() {
            break;
        }
        if let Some(position) = batch.next_position {
            engine.ack(lease, partition, position).await.unwrap();
        }
        out.extend(batch.events.iter().map(|e| e.subject.id));
    }
    out
}

#[tokio::test]
async fn in_order_per_partition_under_ten_thousand_events() {
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        BIG,
        ParkPolicy::default(),
    )
    .await;
    let spec = spec("orders", 4);
    for seq in 1..=10_000u64 {
        f.feed.push(doc_record(0, seq, rid(seq)));
    }
    f.engine
        .register(spec.clone(), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("orders").await.unwrap();

    let mut expected: Vec<Vec<RecordId>> = vec![Vec::new(); 4];
    for seq in 1..=10_000u64 {
        let id = rid(seq);
        expected[spec.partition_for(&id) as usize].push(id);
    }

    let mut total = 0usize;
    for partition in 0..4u32 {
        let delivered = drain(&f.engine, &lease, partition).await;
        assert_eq!(
            delivered, expected[partition as usize],
            "partition {partition} is out of order or missing events"
        );
        total += delivered.len();
    }
    assert_eq!(total, 10_000, "not every event was delivered exactly once");
}

#[tokio::test]
async fn at_least_once_redelivers_unacked_events() {
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        BIG,
        ParkPolicy::default(),
    )
    .await;
    for seq in 1..=5u64 {
        f.feed.push(doc_record(0, seq, rid(seq)));
    }
    f.engine
        .register(spec("resume", 1), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("resume").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    let first: Vec<RecordId> = batch.events.iter().map(|e| e.subject.id).collect();
    assert_eq!(first.len(), 5);
    // Crash between delivery and ack: the lease is given up and a new engine resumes.
    f.engine.release(&lease).await.unwrap();
    let restarted = rebuild(&f, BIG);
    let lease2 = restarted.acquire("resume").await.unwrap();
    let batch2 = restarted.pull(&lease2, 0, None, false).await.unwrap();
    let second: Vec<RecordId> = batch2.events.iter().map(|e| e.subject.id).collect();
    assert_eq!(
        second, first,
        "the unacked events were not redelivered in order"
    );
    // Acknowledging once is enough: a further pull delivers nothing.
    restarted
        .ack(&lease2, 0, batch2.next_position.unwrap())
        .await
        .unwrap();
    let batch3 = restarted.pull(&lease2, 0, None, false).await.unwrap();
    assert!(batch3.events.is_empty(), "acked events were redelivered");
}

#[tokio::test]
async fn ack_beyond_delivered_is_refused() {
    let f = fixture().await;
    for seq in 1..=3u64 {
        f.feed.push(doc_record(0, seq, rid(seq)));
    }
    f.engine
        .register(spec("ackrules", 1), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("ackrules").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    let next = batch.next_position.unwrap();
    let beyond = Position {
        generation: next.generation,
        seq: next.seq + 1,
    };
    let error = f.engine.ack(&lease, 0, beyond).await.unwrap_err();
    assert_eq!(error.code(), spot_event_stream::error::CODE_ACK_REFUSED);
    let behind = Position {
        generation: next.generation,
        seq: next.seq - 1,
    };
    let error = f.engine.ack(&lease, 0, behind).await.unwrap_err();
    assert_eq!(error.code(), spot_event_stream::error::CODE_ACK_REFUSED);
    f.engine.ack(&lease, 0, next).await.unwrap();
}

#[tokio::test]
async fn a_holder_that_lost_its_lease_cannot_ack() {
    let f = fixture().await;
    for seq in 1..=3u64 {
        f.feed.push(doc_record(0, seq, rid(seq)));
    }
    f.engine
        .register(spec("fenced", 1), &Principal::system())
        .await
        .unwrap();
    let lease1 = f.engine.acquire("fenced").await.unwrap();
    let batch = f.engine.pull(&lease1, 0, None, false).await.unwrap();
    let next = batch.next_position.unwrap();
    f.engine.release(&lease1).await.unwrap();
    let lease2 = f.engine.acquire("fenced").await.unwrap();
    let error = f.engine.ack(&lease1, 0, next).await.unwrap_err();
    assert_eq!(error.code(), spot_event_stream::error::CODE_CONSUMER_BUSY);
    f.engine.ack(&lease2, 0, next).await.unwrap();
}

#[tokio::test]
async fn one_connection_per_consumer() {
    let f = fixture().await;
    f.engine
        .register(spec("single", 1), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("single").await.unwrap();
    let error = f.engine.acquire("single").await.unwrap_err();
    assert_eq!(error.code(), spot_event_stream::error::CODE_CONSUMER_BUSY);
    f.engine.release(&lease).await.unwrap();
    f.engine.acquire("single").await.unwrap();
}

#[tokio::test]
async fn resync_after_retention() {
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        BIG,
        ParkPolicy::default(),
    )
    .await;
    f.feed.set_generation(1);
    for seq in 1..=10u64 {
        f.feed.push(doc_record(1, seq, rid(seq)));
    }
    f.engine
        .register(spec("resync", 1), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("resync").await.unwrap();
    let delivered = drain(&f.engine, &lease, 0).await;
    assert_eq!(delivered.len(), 10);
    // Retention drops the early range, then a restart to a lost position must resync.
    f.feed.retain_from(5);
    f.engine
        .restart(
            &lease,
            0,
            Position {
                generation: 1,
                seq: 1,
            },
        )
        .await
        .unwrap();
    let error = f.engine.pull(&lease, 0, None, false).await.unwrap_err();
    assert!(
        matches!(error, StreamError::ResyncRequired { .. }),
        "expected a resync, got {error:?}"
    );
    assert_eq!(error.code(), spot_event_stream::error::CODE_RESYNC_REQUIRED);
    // Restarting to a held position continues normally.
    f.engine
        .restart(
            &lease,
            0,
            Position {
                generation: 1,
                seq: 5,
            },
        )
        .await
        .unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(batch.events.len(), 5);
}

#[tokio::test]
async fn window_full_returns_an_empty_flagged_batch() {
    let policy = BatchPolicy {
        max_events: 100,
        max_bytes: 64 * 1024 * 1024,
        window: 2,
        page_limit: 100,
        max_wait: std::time::Duration::from_secs(25),
    };
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        policy,
        ParkPolicy::default(),
    )
    .await;
    for seq in 1..=5u64 {
        f.feed.push(doc_record(0, seq, rid(seq)));
    }
    f.engine
        .register(spec("windowed", 1), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("windowed").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(batch.events.len(), 2, "the window did not bound the batch");
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert!(batch.events.is_empty());
    assert!(batch.window_full);
    assert_eq!(
        batch.error_code(),
        Some(spot_event_stream::error::CODE_WINDOW_FULL)
    );
    let error = f.engine.pull(&lease, 0, None, true).await.unwrap_err();
    assert_eq!(error.code(), spot_event_stream::error::CODE_WINDOW_FULL);
}

#[tokio::test]
async fn a_parked_consumer_does_not_affect_the_others() {
    let park = ParkPolicy {
        park_after: std::time::Duration::from_secs(10),
        report_after: std::time::Duration::from_secs(5),
    };
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        BIG,
        park,
    )
    .await;
    for seq in 1..=3u64 {
        f.feed.push(doc_record(0, seq, rid(seq)));
    }
    for index in 0..50 {
        f.engine
            .register(spec(&format!("consumer_{index}"), 1), &Principal::system())
            .await
            .unwrap();
    }
    // consumer_0 receives events and never acks.
    let slow = f.engine.acquire("consumer_0").await.unwrap();
    let batch = f.engine.pull(&slow, 0, None, false).await.unwrap();
    assert_eq!(batch.events.len(), 3);
    f.clock.advance(std::time::Duration::from_secs(11));
    let batch = f.engine.pull(&slow, 0, None, false).await.unwrap();
    assert!(batch.parked, "the stalled consumer was not parked");

    // It must be parked for at least the report window before it is named.
    f.clock.advance(std::time::Duration::from_secs(6));
    let report = f.engine.parked_report().await.unwrap();
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].consumer, "consumer_0");

    // Every other consumer still pulls its events.
    for index in 1..50 {
        let lease = f
            .engine
            .acquire(&format!("consumer_{index}"))
            .await
            .unwrap();
        let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
        assert_eq!(
            batch.events.len(),
            3,
            "consumer_{index} was affected by the parked one"
        );
        assert!(!batch.parked);
    }
}

#[tokio::test]
async fn registration_needs_the_owner_or_an_administrator() {
    let f = fixture().await;
    let stranger = Principal::user("stranger", Vec::new());
    let error = f
        .engine
        .register(spec("owned", 1), &stranger)
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        spot_event_stream::error::CODE_CONSUMER_NOT_FOUND
    );
    let owner = Principal::user("owner", Vec::new());
    f.engine.register(spec("owned", 1), &owner).await.unwrap();
    // The stranger may not read it either.
    let error = f.engine.get("owned", &stranger).await.unwrap_err();
    assert_eq!(
        error.code(),
        spot_event_stream::error::CODE_CONSUMER_NOT_FOUND
    );
}

#[tokio::test]
async fn a_reserved_topic_class_needs_the_stream_right() {
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(DenyStreamRights),
        BatchPolicy::default(),
        ParkPolicy::default(),
    )
    .await;
    let error = f
        .engine
        .register(spec("reserved", 1), &Principal::system())
        .await
        .unwrap_err();
    assert_eq!(error.code(), spot_event_stream::error::CODE_SPEC_INVALID);
}

fn with_topic(name: &str, topic: &str) -> ConsumerSpec {
    let mut s = spec(name, 1);
    s.topics = vec![TopicSelector::parse(topic).unwrap()];
    s
}

/// A short prefix selector such as `d.*` matches `doc_invoice`, so an owner without the
/// reserved stream right must be refused for it exactly as for `doc_.*` itself, and the
/// refused registration leaves no consumer behind in the store.
#[tokio::test]
async fn short_prefix_selectors_overlapping_a_reserved_class_are_refused() {
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(DenyStreamRights),
        BatchPolicy::default(),
        ParkPolicy::default(),
    )
    .await;
    for (index, topic) in [
        "d.*",
        "do.*",
        "doc.*",
        "doc_.*",
        "doc_invoice.*",
        "doc_invoice",
        "s.*",
        "sy.*",
        "sys.*",
        "sys_audit",
        "i.*",
        "id.*",
        "identity.*",
        "identity_login.*",
        "doc_x.*",
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("refused_{index}");
        let error = f
            .engine
            .register(with_topic(&name, topic), &Principal::system())
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            spot_event_stream::error::CODE_SPEC_INVALID,
            "{topic} should be refused"
        );
        assert!(
            f.engine.get(&name, &Principal::system()).await.is_err(),
            "a refused registration left a consumer behind for {topic}"
        );
    }
}

/// An owner holding the reserved stream right is accepted for the same short prefix.
#[tokio::test]
async fn an_owner_with_the_right_is_accepted_for_a_short_reserved_prefix() {
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        BatchPolicy::default(),
        ParkPolicy::default(),
    )
    .await;
    f.engine
        .register(with_topic("allowed", "d.*"), &Principal::system())
        .await
        .unwrap();
    f.engine.get("allowed", &Principal::system()).await.unwrap();
}

#[tokio::test]
async fn tail_start_follows_the_head() {
    let f = fixture().await;
    for seq in 1..=3u64 {
        f.feed.push(doc_record(0, seq, rid(seq)));
    }
    let mut tail = spec("tail", 1);
    tail.start = Start::Tail;
    f.engine.register(tail, &Principal::system()).await.unwrap();
    let lease = f.engine.acquire("tail").await.unwrap();
    // Nothing published after registration: a tail consumer gets nothing.
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert!(batch.events.is_empty(), "a tail consumer saw old events");
    f.feed.push(doc_record(0, 4, rid(4)));
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(batch.events.len(), 1);
}

/// Recorded security finding: two nodes starting the same new consumer at once, and a node that registers
/// again after another advanced, never replay what was acknowledged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_registration_never_resets_acknowledged_progress() {
    for iteration in 0..100 {
        let f = fixture_full(
            Arc::new(spot_event_stream::AllVisible),
            Arc::new(spot_event_stream::AllowAllStreamRights),
            BIG,
            ParkPolicy::default(),
        )
        .await;
        for seq in 1..=3u64 {
            f.feed.push(doc_record(0, seq, rid(seq)));
        }
        let node_a = Arc::new(rebuild(&f, BIG));
        let node_b = Arc::new(rebuild(&f, BIG));
        let a = {
            let node = node_a.clone();
            tokio::spawn(async move {
                node.register(spec("shared", 1), &Principal::system())
                    .await
                    .unwrap();
                let lease = node.acquire("shared").await.unwrap();
                let batch = node.pull(&lease, 0, None, false).await.unwrap();
                node.ack(&lease, 0, batch.next_position.unwrap())
                    .await
                    .unwrap();
                node.release(&lease).await.unwrap();
                batch.events.len()
            })
        };
        let b = {
            let node = node_b.clone();
            tokio::spawn(async move {
                for _ in 0..3 {
                    node.register(spec("shared", 1), &Principal::system())
                        .await
                        .unwrap();
                    tokio::task::yield_now().await;
                }
            })
        };
        let delivered = a.await.unwrap();
        b.await.unwrap();
        assert_eq!(delivered, 3, "iteration {iteration}: node A delivery");
        // The acknowledged events are never delivered again, by either node.
        let lease = node_b.acquire("shared").await.unwrap();
        let again = node_b.pull(&lease, 0, None, false).await.unwrap();
        assert!(
            again.events.is_empty(),
            "iteration {iteration}: acknowledged events were redelivered"
        );
        assert_eq!(
            f.store.checkpoint("shared", 0).await.unwrap(),
            Position {
                generation: 0,
                seq: 3
            },
            "iteration {iteration}: the checkpoint moved backwards"
        );
    }
}
