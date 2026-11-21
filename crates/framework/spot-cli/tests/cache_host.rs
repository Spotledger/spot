//! The hosted step cache (WP-PLT-09.T3): `StepCache` over `spot-cache`, bounded and per site.
//!
//! Everything but the two-host Redis case runs everywhere; that one reads `SPOT_TEST_REDIS_URL`
//! and prints `SKIPPED` when it is unset, as `tests/cluster_redis.rs` does.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::context::AppCache;
use spot_cache::{CacheStore, InProcessCache, InProcessConfig};
use spot_cli::services::cache::{
    Built, MAX_KEY_BYTES, STEP_PREFIX, Settings, StepCache, build, reason,
};
use spot_core::{Clock, ManualClock, SiteId};
use time::macros::datetime;

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-13 00:00:00 UTC)))
}

fn small() -> Settings {
    Settings {
        max_bytes: 8 * 1024 * 1024,
        site_max_bytes: 4 * 1024 * 1024,
        max_value_bytes: 1024 * 1024,
        default_ttl_seconds: 3600,
    }
}

fn built(
    settings: &Settings,
    site: &SiteId,
    clock: &Arc<ManualClock>,
    existing: Option<Arc<dyn CacheStore>>,
) -> (Built, Arc<StepCache>) {
    let slot = StepCache::new(site.clone());
    let built = build(
        settings,
        site,
        Arc::clone(clock) as Arc<dyn Clock>,
        existing,
        &slot,
    )
    .unwrap();
    (built, slot)
}

#[tokio::test]
async fn an_unfilled_slot_answers_nothing_and_breaks_nothing() {
    let slot = StepCache::new(site("acme"));
    assert_eq!(slot.get("k").await, None);
    slot.set("k", b"v".to_vec(), None).await;
    slot.delete("k").await;
    assert_eq!(slot.dropped(reason::UNFILLED), 3);
}

#[tokio::test]
async fn a_filled_slot_round_trips_deletes_and_expires_on_the_clock() {
    let clock = clock();
    let (_, slot) = built(&small(), &site("acme"), &clock, None);
    slot.set("k", b"v".to_vec(), Some(Duration::from_secs(10)))
        .await;
    assert_eq!(slot.get("k").await, Some(b"v".to_vec()));
    clock.advance(Duration::from_secs(11));
    assert_eq!(
        slot.get("k").await,
        None,
        "the ttl ran on the injected clock"
    );

    slot.set("d", b"x".to_vec(), Some(Duration::from_secs(60)))
        .await;
    slot.delete("d").await;
    assert_eq!(slot.get("d").await, None);

    // No ttl: the default (3600 s) applies, so nothing lives forever by accident.
    slot.set("forever", b"v".to_vec(), None).await;
    clock.advance(Duration::from_secs(3599));
    assert_eq!(slot.get("forever").await, Some(b"v".to_vec()));
    clock.advance(Duration::from_secs(2));
    assert_eq!(slot.get("forever").await, None);

    // A zero ttl stores nothing.
    slot.set("zero", b"v".to_vec(), Some(Duration::ZERO)).await;
    assert_eq!(slot.get("zero").await, None);
    assert_eq!(slot.dropped(reason::ERROR), 0);
}

#[tokio::test]
async fn a_step_that_floods_the_cache_evicts_its_own_oldest_keys_within_the_site_quota() {
    let clock = clock();
    let settings = small();
    let (built_a, flood) = built(&settings, &site("acme"), &clock, None);
    // A second site over the same store.
    let (_, other) = built(
        &settings,
        &site("globex"),
        &clock,
        Some(Arc::clone(&built_a.store)),
    );
    for i in 0..10 {
        other.set(&format!("keep{i}"), vec![7; 1024], None).await;
    }
    let value = vec![1_u8; 10 * 1024];
    for i in 0..100_000_u32 {
        flood.set(&format!("flood{i}"), value.clone(), None).await;
    }
    let stats = built_a.store.stats(&site("acme")).await;
    assert!(
        stats.bytes <= settings.site_max_bytes,
        "{} bytes held, quota {}",
        stats.bytes,
        settings.site_max_bytes
    );
    assert!(stats.evictions > 0, "the flood evicted its own entries");
    assert_eq!(
        flood.get("flood99999").await,
        Some(value),
        "the newest key is readable"
    );
    assert_eq!(flood.get("flood0").await, None, "the oldest was evicted");
    for i in 0..10 {
        assert_eq!(
            other.get(&format!("keep{i}")).await,
            Some(vec![7; 1024]),
            "the other site's entries are untouched by the flood"
        );
    }
}

#[tokio::test]
async fn a_long_key_and_a_big_value_are_dropped_and_counted_never_stored_never_an_error() {
    let clock = clock();
    let settings = Settings {
        max_value_bytes: 1024,
        ..small()
    };
    let (store, slot) = built(&settings, &site("acme"), &clock, None);
    let long = "k".repeat(600);
    assert!(long.len() > MAX_KEY_BYTES);
    slot.set(&long, b"v".to_vec(), None).await;
    assert_eq!(slot.get(&long).await, None);
    slot.set("big", vec![0; 1025], None).await;
    assert_eq!(slot.get("big").await, None);
    slot.set("edge", vec![0; 1024], None).await;
    assert!(
        slot.get("edge").await.is_some(),
        "exactly the limit is allowed"
    );
    assert_eq!(slot.dropped(reason::KEY_SIZE), 2, "the set and the get");
    assert_eq!(slot.dropped(reason::VALUE_SIZE), 1);
    assert!(
        !store
            .store
            .exists(&site("acme"), &format!("{STEP_PREFIX}big"))
            .await
    );
}

#[tokio::test]
async fn a_value_over_the_site_quota_is_a_counted_quota_drop() {
    let clock = clock();
    let settings = Settings {
        max_bytes: 4096,
        site_max_bytes: 2048,
        max_value_bytes: 1024 * 1024,
        default_ttl_seconds: 60,
    };
    let (_, slot) = built(&settings, &site("acme"), &clock, None);
    slot.set("too-big", vec![0; 4000], None).await;
    assert_eq!(slot.get("too-big").await, None);
    assert_eq!(slot.dropped(reason::QUOTA), 1);
}

#[test]
fn settings_have_defaults_and_refuse_what_cannot_work() {
    let d: Settings = toml::from_str("").unwrap();
    assert_eq!(d, Settings::default());
    assert_eq!(d.max_bytes, 64 * 1024 * 1024);
    assert_eq!(d.site_max_bytes, 32 * 1024 * 1024);
    assert_eq!(d.max_value_bytes, 1024 * 1024);
    assert_eq!(d.default_ttl_seconds, 3600);
    d.validate().unwrap();

    assert!(
        toml::from_str::<Settings>("nope = 1").is_err(),
        "unknown key"
    );
    let above: Settings = toml::from_str("max_bytes = 10\nsite_max_bytes = 11").unwrap();
    let e = above.validate().unwrap_err().to_string();
    assert!(e.contains("services.cache.site_max_bytes"), "{e}");
    for text in [
        "max_bytes = 0",
        "site_max_bytes = 0",
        "max_value_bytes = 0",
        "default_ttl_seconds = 0",
    ] {
        let s: Settings = toml::from_str(text).unwrap();
        let e = s.validate().unwrap_err().to_string();
        let key = text.split(' ').next().unwrap();
        assert!(e.contains(&format!("services.cache.{key}")), "{e}");
    }
}

#[tokio::test]
async fn a_store_a_plugin_provided_first_is_used_and_no_second_port_is_provided() {
    let clock = clock();
    let plugin: Arc<dyn CacheStore> = Arc::new(InProcessCache::with_clock(
        InProcessConfig::new(1024 * 1024, 1024 * 1024),
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));
    let (built, slot) = built(&small(), &site("acme"), &clock, Some(Arc::clone(&plugin)));
    assert!(!built.provide, "the plugin's port stays the only one");
    assert_eq!(built.backend, "redis");
    slot.set("k", b"v".to_vec(), None).await;
    let direct = plugin
        .get(&site("acme"), &format!("{STEP_PREFIX}k"))
        .await
        .unwrap();
    assert_eq!(
        direct.as_deref(),
        Some(&b"v"[..]),
        "the step wrote into the plugin's store"
    );

    let (own, _) = self::built(&small(), &site("acme"), &clock, None);
    assert!(own.provide);
    assert_eq!(own.backend, "in-process");
}

#[tokio::test]
async fn the_metrics_move_after_a_hit_a_miss_and_an_eviction() {
    spot_telemetry::metrics::install().unwrap();
    let clock = clock();
    let settings = Settings {
        max_bytes: 64 * 1024,
        site_max_bytes: 32 * 1024,
        max_value_bytes: 1024 * 1024,
        default_ttl_seconds: 3600,
    };
    let (built, slot) = built(&settings, &site("metrics"), &clock, None);
    slot.set("a", vec![0; 1024], None).await;
    assert!(slot.get("a").await.is_some());
    assert!(slot.get("missing").await.is_none());
    for i in 0..100 {
        slot.set(&format!("e{i}"), vec![0; 1024], None).await;
    }
    let stats = built.service.poll_once().await;
    assert!(
        stats.hits >= 1 && stats.misses >= 1 && stats.evictions >= 1,
        "{stats:?}"
    );
    let text = spot_telemetry::metrics::render_prometheus();
    for name in [
        "spot_cache_hits_total",
        "spot_cache_misses_total",
        "spot_cache_evictions_total",
        "spot_cache_entries",
        "spot_cache_bytes",
    ] {
        assert!(text.contains(name), "{name} missing from\n{text}");
    }
}

#[tokio::test]
async fn a_value_set_on_one_host_is_read_on_another_over_the_same_redis() {
    let Ok(url) = std::env::var("SPOT_TEST_REDIS_URL") else {
        eprintln!("SKIPPED: SPOT_TEST_REDIS_URL is not set (needs a Redis or Valkey server)");
        return;
    };
    use spot_cli::services::cluster_redis::{Settings as RedisSettings, build as redis_build};
    let prefix = format!("t9:{}", std::process::id());
    let redis = RedisSettings {
        enabled: true,
        url: Some(url),
        allow_plaintext: true,
        prefix,
        ..RedisSettings::default()
    };
    let clock = clock();
    let site = site("acme");
    let one = redis_build(&redis, &site, Arc::clone(&clock) as Arc<dyn Clock>)
        .await
        .unwrap();
    let two = redis_build(&redis, &site, Arc::clone(&clock) as Arc<dyn Clock>)
        .await
        .unwrap();
    let (_, host_a) = built(&small(), &site, &clock, Some(one.cache));
    let (_, host_b) = built(&small(), &site, &clock, Some(two.cache));
    host_a.set("shared", b"from-a".to_vec(), None).await;
    assert_eq!(host_b.get("shared").await, Some(b"from-a".to_vec()));
}
