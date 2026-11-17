//! The platform app schedules the idle rate-limit bucket eviction (WP-PLT-17.T6): the manifest
//! carries the `[[schedules]]` entry for `platform:function_evict_idle`, the host builds the
//! schedule, and a running site (manual clock) enqueues the job when the time comes and the
//! function service's handler runs it to success.
//!
//! The handler's own result is `HandlerResult::Ok` (it logs the number of buckets dropped at
//! debug); the host exposes no accessor for the service's bucket count to a test, so the
//! "idle buckets dropped, fresh ones kept" rule stays covered by `spot-functions` and the unit
//! tests of `FunctionService::evict_idle` (see the report's open questions).
//!
//! The end-to-end case needs real PostgreSQL (`SPOT_TEST_PG_URL`), and skips without it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use spot_cli::Options;
use spot_cli::services::function::EVICT_JOB;
use spot_core::ManualClock;
use spot_login::LoginConfig;
use support::Env;
use time::macros::datetime;

#[test]
fn the_platform_manifest_schedules_the_eviction_job() {
    let dir = support::repo().join("apps/platform");
    let manifest = spot_cli::appdir::manifest(&dir).unwrap();
    let schedules: Vec<_> = manifest
        .schedules
        .iter()
        .filter(|sub| sub.step.to_string() == EVICT_JOB)
        .collect();
    assert_eq!(schedules.len(), 1, "exactly one schedule runs {EVICT_JOB}");
    let sub = schedules[0];
    // Every few minutes: six fields, seconds first.
    assert_eq!(sub.cron.as_str().split_whitespace().count(), 6);
    assert!(sub.cron.as_str().contains("*/"), "{}", sub.cron.as_str());
    // The schedule builds the way the host builds every manifest schedule.
    let zone = spot_scheduler::TimeZone::parse("UTC").unwrap();
    spot_scheduler::Schedule::new(sub, zone).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_scheduled_job_runs_on_a_site_when_the_clock_reaches_it() {
    let Some(env) = Env::new().await else {
        assert!(
            std::env::var_os("SPOT_REQUIRE_PG").is_none(),
            "SPOT_REQUIRE_PG is set but no PostgreSQL is configured"
        );
        eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    env.ok(&["new-site", &env.site]);
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-06 10:04:58 UTC)));
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(
        &env.root,
        false,
        Options {
            clock: Some(clock.clone() as Arc<dyn spot_core::Clock>),
            login: LoginConfig::for_tests(),
        },
        async {
            let _ = stopped.await;
        },
    )
    .await
    .unwrap();
    // Past the next five-minute boundary: the scheduler's leader enqueues the job on its next
    // tick and the worker runs the function service's handler. The jump stays inside the
    // scheduler's lease time to live: a longer one lets the lease lapse, the next leader starts
    // with a fresh high-water mark and nothing is due.
    // The scheduler's first tick sets its high-water mark to the clock then; let it run before
    // the clock moves, or the mark is taken after the slot and nothing is due.
    tokio::time::sleep(Duration::from_secs(5)).await;
    clock.advance(Duration::from_secs(4));
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|site| site.site == env.site)
        .unwrap()
        .clone();
    let backend = spot_cli::schema::backend(&loaded, &site).await.unwrap();
    let started = Instant::now();
    loop {
        let client = backend.pool().get().await.unwrap();
        let head = "SELECT count(*)::bigint ";
        let tail = "FROM spot_job WHERE name = $1 AND state = 'succeeded'";
        let sql = format!("{head}{tail}");
        let done: i64 = client.query_one(&sql, &[&EVICT_JOB]).await.unwrap().get(0);
        if done >= 1 {
            break;
        }
        if started.elapsed() >= Duration::from_secs(60) {
            let rows = client
                .query(
                    &format!("{}{}", "SELECT name, state ", "FROM spot_job"),
                    &[],
                )
                .await
                .unwrap();
            let seen: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
            panic!("the eviction job never ran; the jobs are {seen:?}");
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = stop.send(());
    server.join().await.unwrap();
}
