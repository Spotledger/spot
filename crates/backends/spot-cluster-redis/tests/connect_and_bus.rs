//! The connect deadline, the prompt reconnect `Gap` and the wire timestamp (WP-PLT-19.T5).
//!
//! The deadline and wire-format tests need no server; the others skip (printing the reason)
//! without `SPOT_TEST_REDIS_URL`.

#![cfg(feature = "redis")]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spot_cluster_redis::{BusMessage, RedisBus, RedisConfig, RedisConnection, StampedEvent};
use spot_core::{Clock, ManualClock, SiteId};
use spot_meta_live::{BusEvent, Invalidation, InvalidationBus, MetaHash, Version};
use time::OffsetDateTime;

fn site() -> SiteId {
    common::sid("t5.example")
}

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

#[tokio::test]
async fn connect_to_a_closed_port_fails_within_the_deadline() {
    // Port 1 on the loopback interface refuses connections.
    let config =
        RedisConfig::new("redis://127.0.0.1:1").with_connect_timeout(Duration::from_millis(800));
    let started = Instant::now();
    let error = RedisConnection::connect(&config).await.unwrap_err();
    assert_eq!(error.code, "cluster_redis.unavailable");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_deadline_bounds_a_host_supplied_attempt_count() {
    let config = RedisConfig::new("redis://127.0.0.1:1")
        .with_connect_attempts(50, Duration::from_millis(100))
        .with_connect_timeout(Duration::from_millis(500));
    let started = Instant::now();
    assert!(RedisConnection::connect(&config).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn a_message_without_a_timestamp_is_still_accepted() {
    let old = r#"{"kind":"meta","version":3,"hash":"ab"}"#;
    let message: BusMessage = serde_json::from_str(old).unwrap();
    assert_eq!(message.sent_at_ms, None);
    assert!(message.into_invalidation().is_ok());
}

#[test]
fn the_timestamp_round_trips_and_lag_is_never_negative() {
    let invalidation = Invalidation::meta(Version::new(3), MetaHash::from_hex("ab"));
    let message = BusMessage::of(&invalidation).sent_at(at(1_000));
    let back: BusMessage = serde_json::from_str(&serde_json::to_string(&message).unwrap()).unwrap();
    assert_eq!(back.sent_at_ms, Some(1_000_000));
    let stamped = StampedEvent {
        event: BusEvent::Change(invalidation),
        sent_at_ms: back.sent_at_ms,
    };
    assert_eq!(stamped.lag(at(1_002)), Some(Duration::from_secs(2)));
    // A sender whose clock runs ahead reads as no lag, not a negative one.
    assert_eq!(stamped.lag(at(990)), Some(Duration::ZERO));
    let unstamped = StampedEvent {
        event: BusEvent::Gap,
        sent_at_ms: None,
    };
    assert_eq!(unstamped.lag(at(1_002)), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quiet_subscriber_gets_the_gap_promptly_after_a_reconnect() {
    let Some(connection) = common::connect().await else {
        return eprintln!("SKIPPED: SPOT_TEST_REDIS_URL is not set");
    };
    let bus = RedisBus::new(connection.clone());
    let site = site();
    let mut stream = bus.subscribe(&site);
    // Wait until the reader is subscribed: publish until a change arrives.
    let probe = Invalidation::meta(Version::new(1), MetaHash::from_hex("aa"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        bus.publish(&site, probe.clone()).await.unwrap();
        if let Ok(Some(BusEvent::Change(_))) =
            tokio::time::timeout(Duration::from_millis(300), stream.next()).await
        {
            break;
        }
        assert!(Instant::now() < deadline, "the subscriber never connected");
    }
    // Drop every pub/sub connection on the server; the reader reconnects on its own.
    let mut manager = connection.manager();
    redis::cmd("CLIENT")
        .arg("KILL")
        .arg("TYPE")
        .arg("pubsub")
        .query_async::<i64>(&mut manager)
        .await
        .unwrap();
    // Nothing is published now: the Gap must arrive without a further message.
    let event = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match stream.next().await {
                Some(BusEvent::Gap) => return true,
                Some(_) => {}
                None => return false,
            }
        }
    })
    .await;
    assert_eq!(event, Ok(true), "no Gap within 3 s of the reconnect");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_receive_lag_is_small_and_not_negative_for_a_normal_publish() {
    let Some(connection) = common::connect().await else {
        return eprintln!("SKIPPED: SPOT_TEST_REDIS_URL is not set");
    };
    let bus = RedisBus::new(connection);
    let site = site();
    let mut stream = bus.subscribe_stamped(&site);
    let invalidation = Invalidation::meta(Version::new(2), MetaHash::from_hex("bb"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // The test's clock stands in for the host's; both ends read the same one here.
        let clock = ManualClock::new(at(5_000));
        bus.publish_at(&site, invalidation.clone(), clock.now_utc())
            .await
            .unwrap();
        if let Ok(Some(stamped)) =
            tokio::time::timeout(Duration::from_millis(300), stream.next()).await
            && matches!(stamped.event, BusEvent::Change(_))
        {
            assert_eq!(stamped.sent_at_ms, Some(5_000_000));
            let lag = stamped.lag(clock.now_utc()).unwrap();
            assert!(lag < Duration::from_secs(1), "lag {lag:?}");
            return;
        }
        assert!(Instant::now() < deadline, "the subscriber never connected");
    }
}
