//! The invalidation bus, node presence, the version store and the health probe, on a real server.
//!
//! `BusMessage` round-trips without a server (a pure unit check); the rest skip without
//! `SPOT_TEST_REDIS_URL`.

#![cfg(feature = "redis")]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use spot_cluster_redis::{
    BusMessage, HealthReport, RedisBus, RedisConfig, RedisConnection, RedisPresence,
    RedisVersionStore, probe,
};
use spot_coordinator::{NodeId, NodePresence};
use spot_core::SiteId;
use spot_meta_live::{
    BusEvent, Invalidation, InvalidationBus, MetaHash, Version, VersionStore, next_event,
};

fn site() -> SiteId {
    common::sid("acme.example")
}

#[test]
fn bus_message_round_trips() {
    let meta = Invalidation::meta(Version::new(7), MetaHash::from_hex("ab"));
    assert_eq!(BusMessage::of(&meta).into_invalidation(), Ok(meta));
    let cache = Invalidation::cache(
        Version::new(7),
        MetaHash::from_hex("ab"),
        vec!["meta:x".into()],
    );
    assert_eq!(BusMessage::of(&cache).into_invalidation(), Ok(cache));
    let unknown = BusMessage {
        kind: "future".into(),
        version: 1,
        hash: String::new(),
        keys: Vec::new(),
    };
    assert!(unknown.into_invalidation().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_reaches_a_subscriber() {
    let Some(connection) = common::connect().await else {
        return common::skip();
    };
    let bus = RedisBus::new(connection);
    let site = site();
    let mut stream = bus.subscribe(&site);
    // The reader connects and subscribes asynchronously; give it a moment, then retry the
    // publish until the event arrives (pub/sub does not replay).
    let expected = Invalidation::meta(Version::new(4), MetaHash::from_hex("cd"));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        bus.publish(&site, expected.clone()).await.expect("publish");
        match tokio::time::timeout(Duration::from_millis(300), next_event(&mut stream)).await {
            Ok(Some(BusEvent::Change(invalidation))) => {
                assert_eq!(invalidation, expected);
                return;
            }
            Ok(Some(BusEvent::Gap)) => continue,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("the stream ended"),
            Err(_) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "no invalidation arrived"
                );
            }
        }
    }
}

#[tokio::test]
async fn presence_lists_other_nodes() {
    let Some(connection) = common::connect().await else {
        return common::skip();
    };
    let presence = RedisPresence::new(connection, Duration::from_secs(5));
    let first = NodeId::new("node-a").expect("node id");
    let second = NodeId::new("node-b").expect("node id");
    let registration = presence.register(&first).await.expect("register");
    assert!(presence.others(&first).await.expect("others").is_empty());
    let other = presence.register(&second).await.expect("register");
    assert_eq!(
        presence.others(&first).await.expect("others"),
        vec![second.clone()]
    );
    drop(other);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        presence.others(&first).await.expect("others").is_empty(),
        "a dropped registration withdraws its node"
    );
    drop(registration);
}

#[tokio::test]
async fn version_store_reads_back_what_it_wrote() {
    let Some(connection) = common::connect().await else {
        return common::skip();
    };
    let store = RedisVersionStore::new(connection);
    assert_eq!(store.read().await.expect("read"), None);
    let version = Version::new(3);
    let hash = MetaHash::from_hex("deadbeef");
    store.write(version, hash.clone()).await.expect("write");
    assert_eq!(store.read().await.expect("read"), Some((version, hash)));
}

#[tokio::test]
async fn health_probe_reports_a_reachable_server() {
    let Some(connection) = common::connect().await else {
        return common::skip();
    };
    let report = probe(&connection).await;
    assert_eq!(
        report,
        HealthReport {
            reachable: true,
            keyspace: connection.keyspace().prefix().to_owned(),
            detail: String::new(),
        }
    );
    assert!(report.is_healthy());
}

#[tokio::test]
async fn an_unreachable_server_fails_to_connect() {
    let config =
        RedisConfig::new("redis://127.0.0.1:1").with_connect_attempts(1, Duration::from_millis(10));
    assert!(RedisConnection::connect(&config).await.is_err());
}
