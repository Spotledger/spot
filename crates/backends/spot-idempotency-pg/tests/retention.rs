//! Retention is configuration, per caller class, recorded per row (the architecture owner, 5 Nov 2026), and
//! `prune`: bounded batches, by `expires_at` only, never a live reservation.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::{clock, expires_at, fp, pool, response, row_count, scope, store_on};
use spot_core::{Clock, ManualClock};
use spot_http::api::{IdempotencyStore, Reservation};
use spot_idempotency_pg::{ConfigError, PgIdempotencyConfig, RetentionPolicy};
use spot_postgres::testing::TestDb;

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

fn classes(entries: &[(&str, Duration)]) -> BTreeMap<String, Duration> {
    entries
        .iter()
        .map(|(name, keep)| ((*name).to_owned(), *keep))
        .collect()
}

fn policy(default: Duration, max: Duration, entries: &[(&str, Duration)]) -> RetentionPolicy {
    RetentionPolicy::new(default, max, classes(entries)).unwrap()
}

fn unix(clock: &Arc<ManualClock>) -> i64 {
    clock.now_utc().unix_timestamp()
}

#[test]
fn the_policy_is_pure_and_clamps_and_falls_back_by_name() {
    let p = policy(
        DAY,
        7 * DAY,
        &[
            ("http_upload", HOUR),
            ("long", 30 * DAY),
            ("exact", 7 * DAY),
        ],
    );
    assert_eq!(
        p.retain_until("http_api", 1000),
        1000 + 86_400,
        "unknown class: default"
    );
    assert_eq!(p.retain_until("http_upload", 1000), 1000 + 3600, "override");
    assert_eq!(
        p.retain_until("long", 1000),
        1000 + 7 * 86_400,
        "clamped to max"
    );
    assert_eq!(p.retain_until("exact", 1000), 1000 + 7 * 86_400);
    assert_eq!(
        p.retain_until("", 1000),
        1000 + 86_400,
        "the empty name is unknown"
    );
    assert_eq!(
        p.retain_until("HTTP_UPLOAD", 1000),
        1000 + 86_400,
        "names are exact"
    );
    assert_eq!(
        p.retain_until("http_api", i64::MAX),
        i64::MAX,
        "no overflow"
    );
    // A default above the maximum cannot exist; a default equal to it is the whole range.
    let tight = policy(DAY, DAY, &[("any", 5 * DAY)]);
    assert_eq!(tight.retention_of("any"), DAY);
}

#[test]
fn an_invalid_policy_is_refused() {
    assert_eq!(
        RetentionPolicy::new(Duration::ZERO, DAY, BTreeMap::new()),
        Err(ConfigError::ZeroDefaultRetention)
    );
    assert_eq!(
        RetentionPolicy::new(Duration::from_millis(900), DAY, BTreeMap::new()),
        Err(ConfigError::ZeroDefaultRetention),
        "less than one whole second keeps nothing"
    );
    assert_eq!(
        RetentionPolicy::new(2 * DAY, DAY, BTreeMap::new()),
        Err(ConfigError::MaxBelowDefault)
    );
    assert_eq!(
        RetentionPolicy::new(DAY, 7 * DAY, classes(&[("zero", Duration::ZERO)])),
        Err(ConfigError::ZeroClassRetention)
    );
    let standard = RetentionPolicy::standard();
    assert_eq!(standard.default_retention(), DAY);
    assert_eq!(standard.max_retention(), 7 * DAY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_class_is_honoured_and_recorded_on_the_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let config = PgIdempotencyConfig::new(
        common::LEASE,
        policy(DAY, 7 * DAY, &[("http_upload", HOUR), ("long", 30 * DAY)]),
    );
    let s = store_on(pool(&db, 4, "idem_classes"), &clock, config).await;
    let now = unix(&clock);
    for (key, class) in [
        ("k-api", "http_api"),
        ("k-upload", "http_upload"),
        ("k-long", "long"),
        ("k-unknown", "no_such_class"),
    ] {
        let sc = scope("cred", key);
        assert_eq!(s.reserve(&sc, &fp(key)).await, Ok(Reservation::Fresh));
        s.complete(&sc, &fp(key), response(200, b"{}"), class)
            .await
            .unwrap();
    }
    assert_eq!(expires_at(&db, "k-api").await, Some(now + 86_400));
    assert_eq!(expires_at(&db, "k-upload").await, Some(now + 3600));
    assert_eq!(
        expires_at(&db, "k-long").await,
        Some(now + 7 * 86_400),
        "clamped"
    );
    assert_eq!(expires_at(&db, "k-unknown").await, Some(now + 86_400));

    // Two hours on: the upload row is free again, the API row still replays.
    clock.advance(2 * HOUR);
    assert!(matches!(
        s.reserve(&scope("cred", "k-api"), &fp("k-api")).await,
        Ok(Reservation::Completed(_))
    ));
    assert_eq!(
        s.reserve(&scope("cred", "k-upload"), &fp("k-upload")).await,
        Ok(Reservation::Fresh),
        "a http_upload row and a http_api row completed together expire at different times"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changing_the_policy_never_alters_rows_already_written() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let first = store_on(pool(&db, 2, "idem_policy_one"), &clock, common::config()).await;
    let sc = scope("cred", "policy-1");
    assert_eq!(first.reserve(&sc, &fp("a")).await, Ok(Reservation::Fresh));
    first
        .complete(&sc, &fp("a"), response(200, b"{}"), "http_api")
        .await
        .unwrap();
    let written = expires_at(&db, "policy-1").await.unwrap();
    drop(first);

    // The site tightens retention to one hour and restarts.
    let tight = PgIdempotencyConfig::new(common::LEASE, policy(HOUR, HOUR, &[]));
    let second = store_on(pool(&db, 2, "idem_policy_two"), &clock, tight).await;
    assert_eq!(
        expires_at(&db, "policy-1").await,
        Some(written),
        "row untouched"
    );
    clock.advance(2 * HOUR);
    assert!(
        matches!(
            second.reserve(&sc, &fp("a")).await,
            Ok(Reservation::Completed(_))
        ),
        "the old row replays until its own expires_at"
    );
    clock.advance(23 * HOUR);
    assert_eq!(second.reserve(&sc, &fp("a")).await, Ok(Reservation::Fresh));
    // New rows follow the new policy.
    second
        .complete(&sc, &fp("a"), response(200, b"{}"), "http_api")
        .await
        .unwrap();
    assert_eq!(expires_at(&db, "policy-1").await, Some(unix(&clock) + 3600));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prune_deletes_by_expires_at_only_in_bounded_batches() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let config =
        PgIdempotencyConfig::new(common::LEASE, policy(HOUR, 7 * DAY, &[("long", 3 * DAY)]));
    let s = store_on(pool(&db, 4, "idem_prune"), &clock, config).await;
    // Five short rows, one long row, one running reservation.
    for n in 0..5 {
        let key = format!("short-{n}");
        let sc = scope("cred", &key);
        s.reserve(&sc, &fp(&key)).await.unwrap();
        s.complete(&sc, &fp(&key), response(200, b"{}"), "http_api")
            .await
            .unwrap();
    }
    let long = scope("cred", "long-1");
    s.reserve(&long, &fp("l")).await.unwrap();
    s.complete(&long, &fp("l"), response(200, b"{}"), "long")
        .await
        .unwrap();
    let running = scope("cred", "running-1");
    assert_eq!(row_count(&db).await, 6);

    assert_eq!(s.prune(100).await, Ok(0), "nothing has expired yet");
    clock.advance(2 * HOUR);
    // A reservation made now holds a live lease while the short rows are long expired.
    assert_eq!(s.reserve(&running, &fp("r")).await, Ok(Reservation::Fresh));
    assert_eq!(row_count(&db).await, 7);
    assert_eq!(
        s.prune(2).await,
        Ok(2),
        "a bounded batch removes at most `limit`"
    );
    assert_eq!(
        s.prune(100).await,
        Ok(3),
        "the rest of the expired short rows"
    );
    assert_eq!(s.prune(100).await, Ok(0));
    assert_eq!(
        row_count(&db).await,
        2,
        "the long row and the reservation remain"
    );
    assert!(matches!(
        s.reserve(&long, &fp("l")).await,
        Ok(Reservation::Completed(_))
    ));
    assert_eq!(
        s.reserve(&running, &fp("r")).await,
        Ok(Reservation::Pending),
        "prune never removes a live reservation"
    );

    // An abandoned reservation (lease ended) stays claimable, and is pruned only once its lease
    // ended more than the policy maximum ago.
    clock.advance(3 * DAY);
    assert_eq!(s.prune(100).await, Ok(1), "the long row expired");
    assert_eq!(row_count(&db).await, 1);
    assert_eq!(
        s.reserve(&scope("cred", "running-1"), &fp("r")).await,
        Ok(Reservation::Fresh),
        "a lapsed lease is free to claim"
    );
    clock.advance(8 * DAY);
    assert_eq!(
        s.prune(100).await,
        Ok(1),
        "an abandoned reservation goes after the maximum"
    );
    assert_eq!(row_count(&db).await, 0);
    assert_eq!(
        s.reserve(&running, &fp("r")).await,
        Ok(Reservation::Fresh),
        "a pruned reservation frees its key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_default_window_is_twenty_four_hours() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let clock = clock();
    let s = store_on(pool(&db, 2, "idem_default"), &clock, common::config()).await;
    let sc = scope("cred", "default-1");
    s.reserve(&sc, &fp("a")).await.unwrap();
    s.complete(&sc, &fp("a"), response(200, b"{}"), "http_api")
        .await
        .unwrap();
    clock.advance(DAY - Duration::from_secs(1));
    assert!(matches!(
        s.reserve(&sc, &fp("a")).await,
        Ok(Reservation::Completed(_))
    ));
    clock.advance(Duration::from_secs(1));
    assert_eq!(s.reserve(&sc, &fp("a")).await, Ok(Reservation::Fresh));
}
