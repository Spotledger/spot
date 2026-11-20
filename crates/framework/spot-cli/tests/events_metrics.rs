//! Watching the event stream (WP-INT-02.T7): `/metrics` shows consumers, open streams, lag,
//! parked partitions and the counters; readiness stays 200 and says `degraded` with the reason
//! when a partition is parked past the report time or a consumer is further behind than
//! `lag_warn_events`.
//!
//! Real `spot serve` on real PostgreSQL (skipped, with the printed reason, when
//! `SPOT_TEST_PG_URL` is unset), reusing the fixture of `events.rs` (`events/fixture.rs`, read
//! only). The metrics recorder is process-global, so every scenario that reads a gauge runs in
//! the one test below, in order; the settings test never starts a sampler. The sampler is driven
//! with `sample_now` so nothing waits for `metrics_interval_seconds`, and the site's clock is a
//! manual clock so a partition can stay parked "for a while" without sleeping.
#![allow(missing_docs, dead_code, clippy::unwrap_used, clippy::expect_used)]
mod support;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_cli::{Options, Running};
use spot_core::{Clock, ManualClock, SiteId};
use spot_event_stream::ConsumerStore;
use spot_event_stream_pg::PgConsumerStore;
use spot_events_contract::envelope::Position;
use spot_login::LoginConfig;
use support::Env;
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

include!("events/fixture.rs");

/// The site's own options with a clock the test moves.
fn clocked(clock: &Arc<ManualClock>) -> Options {
    let shared: Arc<dyn Clock> = clock.clone();
    Options {
        clock: Some(shared),
        login: LoginConfig::for_tests(),
    }
}

async fn serve_clocked(
    env: &Env,
    clock: &Arc<ManualClock>,
) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = spot_cli::start_with(&env.root, false, clocked(clock), async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

/// A bare site with `[services.events]` written from `table`, users with their roles, and the
/// host serving it on `clock`.
async fn start(users: &[(&str, &str)], table: &str, clock: &Arc<ManualClock>) -> Option<Stack> {
    let env = Env::new().await?;
    let secret = env
        .ok(&["new-site", &env.site])
        .trim()
        .rsplit(' ')
        .next()
        .unwrap()
        .to_owned();
    let toml = env.root.join("common.toml");
    let mut text = std::fs::read_to_string(&toml).unwrap();
    text.push_str("\n[services.events]\nenabled = true\n");
    text.push_str(table);
    std::fs::write(&toml, text).unwrap();
    for (email, _) in users {
        spot_cli::users::add_user(&env.root, &env.site, email, None, &[])
            .await
            .unwrap();
        spot_cli::users::set_password(
            &env.root,
            &env.site,
            email,
            &spot_auth::Password::new(PASSWORD),
        )
        .await
        .unwrap();
    }
    let (server, addr, stop) = serve_clocked(&env, clock).await;
    let host = env.site.clone();
    let admin = administrator(addr, &host, &secret).await;
    let stack = Stack {
        env,
        addr,
        host,
        admin,
        server: Some(server),
        stop: Some(stop),
    };
    for (email, role) in users {
        if !role.is_empty() {
            stack.assign(email, role).await;
        }
    }
    Some(stack)
}

/// The scraped text of this process's recorder.
fn scrape() -> String {
    spot_telemetry::render_prometheus()
}

/// The value of the sample named `key` (with its labels, as scraped), `0` when absent.
fn value(key: &str) -> f64 {
    scrape()
        .lines()
        .find(|l| l.split(' ').next() == Some(key))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// `/api/health/ready`: the status and the whole body as text.
async fn ready(s: &Stack) -> (u16, String) {
    let reply = http(s.addr, &s.host, None, "GET", "/api/health/ready", &[], None).await;
    (reply.status, reply.body.to_string())
}

/// Takes one sample now.
async fn sample(s: &Stack) {
    assert!(spot_cli::services::events::sample_now(&s.env.site).await);
}

fn consumer_path(name: &str) -> String {
    format!("/api/v1/events/consumers/{name}")
}

async fn register(s: &Stack, name: &str) {
    let made = s
        .admin(
            "PUT",
            &consumer_path(name),
            Some(spec(ROLES_ONLY, json!({"kind": "tail"}))),
        )
        .await;
    assert!(made.status == 201 || made.status == 200, "{:?}", made.body);
}

/// Pulls (optionally with the lease of an earlier pull) and returns the reply.
async fn pull(s: &Stack, name: &str, lease: Option<&str>) -> Reply {
    let extra: Vec<(&str, &str)> = lease.map(|l| ("x-consumer-lease", l)).into_iter().collect();
    s.call(
        &s.admin,
        "POST",
        &format!("{}/pull", consumer_path(name)),
        &extra,
        Some(pull_body()),
    )
    .await
}

/// Acknowledges what `reply` (a pull) returned.
async fn ack(s: &Stack, name: &str, lease: &str, reply: &Reply) {
    let done = s
        .call(
            &s.admin,
            "POST",
            &format!("{}/ack", consumer_path(name)),
            &[("x-consumer-lease", lease)],
            Some(json!({"partition": 0, "position": reply.body["data"]["next_position"]})),
        )
        .await;
    assert_eq!(done.status, 200, "{:?}", done.body);
}

/// Pulls and acknowledges until nothing is left; returns how many events were consumed.
async fn drain(s: &Stack, name: &str) -> usize {
    drain_with(s, name, None).await.0
}

/// [`drain`] holding the lease `lease` (a consumer's earlier lease lasts until it expires: a
/// second drain without it is refused as busy); returns the count and the lease at the end.
async fn drain_with(s: &Stack, name: &str, mut lease: Option<String>) -> (usize, Option<String>) {
    let mut total = 0;
    loop {
        let reply = pull(s, name, lease.as_deref()).await;
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        let token = reply.header("x-consumer-lease").expect("a lease");
        let count = reply.body["data"]["events"].as_array().unwrap().len();
        if count == 0 {
            lease = Some(token);
            break;
        }
        ack(s, name, &token, &reply).await;
        total += count;
        lease = Some(token);
    }
    (total, lease)
}

async fn site_store(env: &Env) -> PgConsumerStore {
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
    PgConsumerStore::new(backend.pool(), SiteId::new(&env.site).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_event_stream_can_be_watched() {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
    let table = "poll_ms = 50\nheartbeat_seconds = 1\nmax_idle_seconds = 30\nlease_seconds = 40\n\
        max_streams = 2\nmax_streams_per_user = 2\nconsumer_roles = [\"Event Reader\"]\n\
        stream_roles = [\"Event Reader\"]\n\
        lag_warn_events = 100\nmetrics_interval_seconds = 5\n";
    let Some(mut s) = start(&[("reader@example.com", "Event Reader")], table, &clock).await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };

    // ---- lag: 250 events behind, then caught up ---------------------------------------------
    register(&s, "ledger").await;
    for i in 0..250 {
        s.make_role(&format!("Lag {i}")).await;
    }
    sample(&s).await;
    assert_eq!(value("spot_events_consumers"), 1.0);
    let behind = value("spot_events_max_lag_events");
    assert!(behind >= 250.0, "250 events behind, saw {behind}");
    let (consumed, ledger_lease) = drain_with(&s, "ledger", None).await;
    assert_eq!(consumed, 250);
    sample(&s).await;
    let caught_up = value("spot_events_max_lag_events");
    assert!(
        behind - caught_up >= 250.0,
        "the lag fell by what was consumed: {behind} to {caught_up}"
    );

    // ---- readiness follows lag_warn_events = 100 --------------------------------------------
    register(&s, "slow").await;
    for i in 0..150 {
        s.make_role(&format!("Slow {i}")).await;
    }
    sample(&s).await;
    let (status, body) = ready(&s).await;
    assert_eq!(status, 200, "degraded readiness is still 200: {body}");
    assert!(body.contains("degraded"), "{body}");
    assert!(body.contains("events ("), "{body}");
    assert!(
        body.contains("slow"),
        "the lagging consumer is named: {body}"
    );
    assert!(body.contains("limit 100"), "{body}");
    // Every consumer catches up (`ledger` fell behind again by the 150 roles above), or the
    // lag the readiness reports is its.
    drain_with(&s, "ledger", ledger_lease).await;
    drain(&s, "slow").await;
    sample(&s).await;
    let (status, body) = ready(&s).await;
    assert_eq!(status, 200);
    assert!(
        !body.contains("consumer lag"),
        "readiness recovers by itself: {body}"
    );

    // ---- a partition parked past the report time -------------------------------------------
    register(&s, "parker").await;
    let store = site_store(&s.env).await;
    store.park("parker", 0, clock.now_utc()).await.unwrap();
    clock.advance(Duration::from_secs(10));
    sample(&s).await;
    assert_eq!(value("spot_events_parked_partitions"), 1.0);
    assert!(value("spot_events_oldest_parked_seconds") >= 10.0);
    let (_, body) = ready(&s).await;
    assert!(
        !body.contains("partition(s) parked"),
        "not yet past the report time: {body}"
    );
    // The report time is 900 s by default.
    clock.advance(Duration::from_secs(900));
    sample(&s).await;
    assert!(value("spot_events_oldest_parked_seconds") >= 910.0);
    let (status, body) = ready(&s).await;
    assert_eq!(status, 200);
    assert!(body.contains("1 partition(s) parked"), "{body}");
    assert!(
        !body.contains("parker"),
        "the parked reason names a count, never a consumer: {body}"
    );
    // Sessions may have lapsed on the moved clock: sign in again, then restart the partition.
    s.admin = s.session("administrator").await;
    let first = pull(&s, "parker", None).await;
    let lease = first.header("x-consumer-lease").expect("a lease");
    let restarted = s
        .call(
            &s.admin,
            "POST",
            &format!("{}/restart", consumer_path("parker")),
            &[("x-consumer-lease", &lease)],
            Some(json!({"partition": 0, "from": {"kind": "tail"}})),
        )
        .await;
    assert_eq!(restarted.status, 200, "{:?}", restarted.body);
    sample(&s).await;
    assert_eq!(value("spot_events_parked_partitions"), 0.0);
    assert_eq!(value("spot_events_oldest_parked_seconds"), 0.0);
    let (_, body) = ready(&s).await;
    assert!(!body.contains("partition(s) parked"), "{body}");

    // ---- a checkpoint in an older generation -----------------------------------------------
    register(&s, "restored").await;
    let latest = s.make_role("After restore").await;
    let _ = latest;
    let seen = pull(&s, "restored", None).await;
    let lease = seen.header("x-consumer-lease").expect("a lease");
    let generation = seen.body["data"]["events"][0]["position"]["generation"]
        .as_u64()
        .unwrap();
    if generation >= 1 {
        let before = value("spot_events_resync_required");
        store
            .restart(
                "restored",
                0,
                Position {
                    generation: generation - 1,
                    seq: 7,
                },
            )
            .await
            .unwrap();
        sample(&s).await;
        assert_eq!(value("spot_events_resync_required"), before + 1.0);
        let lag = value("spot_events_max_lag_events");
        assert!(
            lag < 1_000_000.0,
            "an older generation is not a huge lag: {lag}"
        );
    } else {
        eprintln!("generation 0: the older-generation case is skipped");
    }
    let _ = lease;

    // ---- counters: pulled, redelivered, acked, skipped -------------------------------------
    register(&s, "counted").await;
    let pulled = value("spot_events_pulled_total");
    let redelivered = value("spot_events_redelivered_total");
    let acked = value("spot_events_acked_total");
    for i in 0..5 {
        s.make_role(&format!("Counted {i}")).await;
    }
    let first = pull(&s, "counted", None).await;
    let lease = first.header("x-consumer-lease").expect("a lease");
    assert_eq!(first.body["data"]["events"].as_array().unwrap().len(), 5);
    assert_eq!(value("spot_events_pulled_total") - pulled, 5.0);
    // The same pull again before the ack delivers the same five once more.
    let again = pull(&s, "counted", Some(&lease)).await;
    assert_eq!(again.body["data"]["events"].as_array().unwrap().len(), 5);
    assert_eq!(value("spot_events_redelivered_total") - redelivered, 5.0);
    ack(&s, "counted", &lease, &again).await;
    assert_eq!(value("spot_events_acked_total") - acked, 1.0);

    // A reader without the right to see roles has the events skipped, never named.
    let reader = s.session("reader@example.com").await;
    let skipped = value("spot_events_skipped_total");
    let made = s
        .call(
            &reader,
            "PUT",
            &consumer_path("blind"),
            &[],
            Some(spec(ROLES_ONLY, json!({"kind": "tail"}))),
        )
        .await;
    assert!(made.status == 201 || made.status == 200, "{:?}", made.body);
    s.make_role("Hidden").await;
    let blind = s
        .call(
            &reader,
            "POST",
            &format!("{}/pull", consumer_path("blind")),
            &[],
            Some(pull_body()),
        )
        .await;
    assert_eq!(blind.status, 200, "{:?}", blind.body);
    assert!(value("spot_events_skipped_total") - skipped >= 1.0);

    // ---- open streams and the site cap (2) -------------------------------------------------
    for name in ["s1", "s2", "s3"] {
        register(&s, name).await;
    }
    let rejected = r#"spot_events_stream_rejections_total{reason="site_cap"}"#;
    let before = value(rejected);
    let mut one = Sse::open(s.addr, &s.host, &s.admin, "consumer=s1&partition=0", &[]).await;
    let mut two = Sse::open(s.addr, &s.host, &s.admin, "consumer=s2&partition=0", &[]).await;
    assert!(one.until("200", Duration::from_secs(10)).await);
    assert!(two.until("200", Duration::from_secs(10)).await);
    sample(&s).await;
    assert_eq!(value("spot_events_open_streams"), 2.0);
    let mut three = Sse::open(s.addr, &s.host, &s.admin, "consumer=s3&partition=0", &[]).await;
    assert!(three.until("429", Duration::from_secs(10)).await);
    assert_eq!(value(rejected) - before, 1.0);

    // ---- no label and no readiness detail carries a position, subject, user or consumer ----
    let text = scrape();
    for line in text.lines().filter(|l| l.starts_with("spot_events_")) {
        for forbidden in [
            "consumer=",
            "user=",
            "topic=",
            "position",
            "generation",
            "subject",
        ] {
            assert!(!line.contains(forbidden), "{forbidden} in a label: {line}");
        }
        for name in ["ledger", "slow", "parker", "restored", "counted", "blind"] {
            assert!(!line.contains(name), "a consumer name in a metric: {line}");
        }
    }
    let (_, body) = ready(&s).await;
    assert!(
        !body.contains("v1."),
        "a position in the readiness detail: {body}"
    );

    drop((one, two, three));
    s.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sampler_settings_are_checked_at_start() {
    // A sample interval under 5 s is a start-up error that names the key; an unknown key is
    // refused too. Neither site gets as far as sampling.
    for (table, needle) in [
        (
            "metrics_interval_seconds = 1\n",
            Some("metrics_interval_seconds"),
        ),
        ("lag_warn = 5\n", None),
    ] {
        let Some(env) = Env::new().await else {
            eprintln!("skipped: SPOT_TEST_PG_URL is not set");
            return;
        };
        env.ok(&["new-site", &env.site]);
        let toml = env.root.join("common.toml");
        let mut text = std::fs::read_to_string(&toml).unwrap();
        text.push_str(&format!("\n[services.events]\nenabled = true\n{table}"));
        std::fs::write(&toml, text).unwrap();
        let clock = Arc::new(ManualClock::new(OffsetDateTime::now_utc()));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let outcome = spot_cli::start_with(&env.root, false, clocked(&clock), async {
            let _ = stopped.await;
        })
        .await;
        match outcome {
            Ok(running) => {
                // A site whose service refuses its settings does not stop the server (the other
                // sites keep serving): it is unavailable, and says so to every request.
                let addr = running.local_addrs()[0];
                let reply = http(
                    addr,
                    &env.site,
                    None,
                    "GET",
                    "/api/v1/resource/user",
                    &[],
                    None,
                )
                .await;
                let _ = stop.send(());
                running.join().await.unwrap();
                assert_eq!(
                    reply.status, 503,
                    "{table:?} must leave the site unavailable: {:?}",
                    reply.body
                );
                assert!(
                    !reply.body.to_string().contains("= 1"),
                    "a value is never echoed: {:?}",
                    reply.body
                );
            }
            Err(error) => {
                if let Some(needle) = needle {
                    assert!(error.to_string().contains(needle), "{error}");
                }
                assert!(
                    !error.to_string().contains("= 1"),
                    "a value is never echoed: {error}"
                );
            }
        }
    }
}
