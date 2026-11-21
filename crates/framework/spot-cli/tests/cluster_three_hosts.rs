//! The cluster profile through `spot serve`'s own start-up (WP-PLT-18.T3): three real hosts on one
//! PostgreSQL database, no Redis. One scheduler leader at a time, every schedule firing enqueued
//! exactly once, no job claimed twice, a stopped or lapsed leader replaced without a missed or a
//! duplicate firing, a rate limit that holds across the hosts' coordinators, and the in-process
//! coordinator refusing a second node.
//!
//! Every scenario needs PostgreSQL (`SPOT_TEST_PG_URL`; otherwise it prints `SKIPPED` and
//! returns), takes a database and a site of its own, and stops every host it started, also when an
//! assertion fails (the hosts are held by a guard whose `finish` runs on every path that reaches
//! the end; a panic drops the runtime, which stops the tasks). The scenarios run one after the
//! other (a process-wide lock) because the lease gauge of the metrics recorder is process-global.
//!
//! What the harness cannot do is listed in `docs/running-spot/cluster_profile.md` and in
//! the task report: the hosts' node ids are random and private, so the leader is found by its
//! lease holder and by which host's stop moves the lease; a host cannot be killed without
//! releasing its lease, so a lapsed lease is made by expiring the lease row (a node that stopped
//! renewing); the single-node host builds its presence in memory, so the refusal is asserted on
//! the coordinator over the database presence.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::{Env, Started, fast_options, start_with};
use deadpool_postgres::Pool;
use spot_cli::services::jobs::{FinalAttemptHooks, build_worker, scheduler_lease};
use spot_coordinator::error::SECOND_NODE;
use spot_coordinator::{Admit, Coordinator, InProcessCoordinator, NodeId, RateLimit};
use spot_coordinator_pg::{PgCoordinator, PgCoordinatorConfig, PgPresence};
use spot_core::{ManualClock, SiteId, SystemClock};
use spot_jobs::model::{InsertResult, NewJob};
use spot_jobs::store::JobStore;
use spot_jobs::worker::{HandlerResult, JobHandlerRegistry};
use spot_jobs::{Error as JobError, QueuePolicy};
use spot_jobs_pg::PgJobStore;
use spot_postgres::PgBackend;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::watch;

/// The lease time to live of both leases: short, so a handover is seen within seconds.
const TTL_MS: u64 = 1_500;
const TTL: Duration = Duration::from_millis(TTL_MS);
const TICK_MS: u64 = 200;
/// The app's schedule fires every second.
const JOB_NAME: &str = "sample:tick";

static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn services(profile: &str) -> String {
    format!(
        "[services]\nprofile = \"{profile}\"\n[services.metadata]\nbus = \"polling\"\n\
         [services.jobs]\nstore = \"postgres\"\nlease_ttl_ms = {TTL_MS}\nintake_poll_ms = 100\n\
         [services.scheduler]\ntick_ms = {TICK_MS}\nlease_ttl_ms = {TTL_MS}\n"
    )
}

/// The `common.toml` services text of the single-node profile: the memory-free defaults but the
/// same short leases.
fn single_node_services() -> String {
    format!(
        "[services]\nprofile = \"single-node\"\n[services.jobs]\nstore = \"postgres\"\n\
         lease_ttl_ms = {TTL_MS}\n[services.scheduler]\ntick_ms = {TICK_MS}\nlease_ttl_ms = {TTL_MS}\n"
    )
}

/// An environment whose sample app declares one schedule that fires every second.
async fn env_with_schedule(services: &str) -> Option<Env> {
    let env = Env::new(services).await?;
    let manifest = env.root.0.join("sample/app.toml");
    let text = std::fs::read_to_string(&manifest).unwrap()
        + "\n[[schedules]]\nname = \"tick\"\ncron = \"* * * * * *\"\nstep = \"sample:derive_project_owner\"\nbudget_ms = 1000\n";
    std::fs::write(&manifest, text).unwrap();
    let _first_secret = env.site();
    Some(env)
}

/// The site database's pool.
async fn pool_of(env: &Env) -> Pool {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    let config = loaded.admin.clone().with_dbname(&site.database);
    PgBackend::connect(config).await.unwrap().pool()
}

/// A coordinator handle over the site database: what a fourth observer sees.
async fn observer(pool: &Pool) -> Arc<PgCoordinator> {
    let coordinator = PgCoordinator::new(pool.clone(), PgCoordinatorConfig::default());
    coordinator.prepare().await.unwrap();
    Arc::new(coordinator)
}

/// The hosts of one scenario; `finish_all` stops each.
struct Hosts(Vec<Option<Started>>);

impl Hosts {
    async fn start(env: &Env, count: usize) -> Hosts {
        let mut hosts = Vec::new();
        for _ in 0..count {
            hosts.push(Some(start_with(env, fast_options()).await));
        }
        Hosts(hosts)
    }

    /// Stops host `index` and waits for its services to drain.
    async fn stop(&mut self, index: usize) {
        if let Some(host) = self.0[index].take() {
            host.finish().await;
        }
    }

    async fn finish_all(&mut self) {
        for index in 0..self.0.len() {
            self.stop(index).await;
        }
    }
}

/// Every `sample:tick` job of the site: (idempotency key, state).
async fn firings(pool: &Pool, site: &str) -> Vec<(String, String)> {
    let client = pool.get().await.unwrap();
    client
        .query(
            "SELECT idempotency_key, state FROM spot_job WHERE site = $1 AND name = $2 \
             ORDER BY id",
            &[&site, &JOB_NAME],
        )
        .await
        .unwrap()
        .iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect()
}

/// The scheduled instants in the keys (`<app>:<schedule>:<RFC 3339 instant>`), in seconds since
/// the epoch, with how many jobs carry each.
fn instants(rows: &[(String, String)]) -> BTreeMap<i64, u32> {
    let mut seen = BTreeMap::new();
    for (key, _) in rows {
        let at = key
            .strip_prefix("sample:tick:")
            .unwrap_or_else(|| panic!("an unexpected key `{key}`"));
        let at = OffsetDateTime::parse(at, &Rfc3339)
            .unwrap_or_else(|e| panic!("the key `{key}` carries no RFC 3339 instant: {e}"));
        *seen.entry(at.unix_timestamp()).or_insert(0) += 1;
    }
    seen
}

#[track_caller]
fn assert_each_once_and_none_missed(rows: &[(String, String)]) {
    let seen = instants(rows);
    assert!(!seen.is_empty(), "no schedule ever fired");
    for (instant, count) in &seen {
        assert_eq!(*count, 1, "the firing at {instant} has {count} jobs");
    }
    let first = *seen.keys().next().unwrap();
    let last = *seen.keys().next_back().unwrap();
    let missing: Vec<i64> = (first..=last).filter(|s| !seen.contains_key(s)).collect();
    assert!(
        missing.is_empty(),
        "firings missed between {first} and {last}: {missing:?}"
    );
}

async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// The live scheduler lease, waiting up to `wait` for one to exist.
async fn leader(
    coordinator: &PgCoordinator,
    site: &str,
    wait: Duration,
) -> spot_coordinator::Lease {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(lease) = coordinator.current(&scheduler_lease(site)).await.unwrap() {
            return lease;
        }
        assert!(
            Instant::now() < deadline,
            "no node took the scheduler lease within {wait:?}"
        );
        sleep_ms(100).await;
    }
}

fn mark_counter(site: &str) -> String {
    format!("schedule_mark:{site}:sample:tick")
}

fn held_gauge() -> f64 {
    spot_telemetry::render_prometheus()
        .lines()
        .find(|l| l.starts_with(r#"spot_lease_held{lease="scheduler"}"#))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

// ---- 1. one leader ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_leader_and_every_firing_once() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(env) = env_with_schedule(&services("cluster")).await else {
        return;
    };
    let pool = pool_of(&env).await;
    let watcher = observer(&pool).await;
    let mut hosts = Hosts::start(&env, 3).await;

    let first = leader(&watcher, &env.site, Duration::from_secs(10)).await;
    let name = scheduler_lease(&env.site);
    let mut last_mark = 0;
    let mut holders = BTreeSet::new();
    let mut gauge_samples = Vec::new();
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(20) {
        // One live lease, held by one node; the same node while it keeps renewing.
        let live = watcher.current(&name).await.unwrap().expect("a live lease");
        assert!(live.token >= first.token);
        holders.insert((live.holder.clone(), live.token.0));
        let mark = watcher.peek(&mark_counter(&env.site)).await.unwrap();
        assert!(
            mark >= last_mark,
            "the mark went from {last_mark} to {mark}"
        );
        last_mark = mark;
        // The lease table has one row for the name, never two holders.
        let client = pool.get().await.unwrap();
        let rows = client
            .query(
                "SELECT holder FROM spot_coord_lease WHERE name = $1",
                &[&name],
            )
            .await
            .unwrap();
        assert!(rows.len() <= 1);
        gauge_samples.push(held_gauge());
        sleep_ms(500).await;
    }

    // Steady state: a single holder and a single token over the whole window.
    assert_eq!(
        holders.len(),
        1,
        "the leader changed without a failure: {holders:?}"
    );
    // The gauge is process-wide here (the three hosts share one recorder), so it can only say
    // that no sample shows two holders.
    assert!(
        gauge_samples.iter().all(|g| *g <= 1.0),
        "the lease gauge showed more than one holder: {gauge_samples:?}"
    );

    let rows = firings(&pool, &env.site).await;
    assert!(rows.len() >= 10, "only {} firings in 20 s", rows.len());
    assert_each_once_and_none_missed(&rows);
    let keys: BTreeSet<&String> = rows.iter().map(|(k, _)| k).collect();
    assert_eq!(keys.len(), rows.len(), "an idempotency key twice");
    assert!(last_mark > 0, "the mark never moved");
    hosts.finish_all().await;
}

// ---- 2. no job claimed twice ----

/// Three workers, one per node, over the one PostgreSQL job store: every job runs once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_job_runs_twice_across_three_workers() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(env) = env_with_schedule(&services("cluster")).await else {
        return;
    };
    let pool = pool_of(&env).await;
    let store = PgJobStore::new(pool.clone());
    store.prepare().await.unwrap();
    let site = SiteId::new(&env.site).unwrap();
    for i in 0..200u32 {
        let job = NewJob::new(
            "app:count".to_owned(),
            serde_json::json!({ "i": i }),
            "default".to_owned(),
            site.clone(),
            OffsetDateTime::now_utc(),
        )
        .with_idempotency_key(format!("count:{i}"));
        assert!(matches!(
            store.insert_if_new(job).await.unwrap(),
            InsertResult::Inserted(_)
        ));
    }

    let ran: Arc<std::sync::Mutex<HashMap<u32, Vec<usize>>>> = Arc::default();
    let per_node: Vec<Arc<AtomicU32>> = (0..3).map(|_| Arc::new(AtomicU32::new(0))).collect();
    let mut stops = Vec::new();
    for (node, counter) in per_node.iter().enumerate() {
        let mut registry = JobHandlerRegistry::new();
        let (ran, count) = (Arc::clone(&ran), Arc::clone(counter));
        registry.register("app:count", move |ctx| {
            let (ran, count) = (Arc::clone(&ran), Arc::clone(&count));
            Box::pin(async move {
                let i = ctx.job.args["i"].as_u64().unwrap() as u32;
                ran.lock().unwrap().entry(i).or_default().push(node);
                count.fetch_add(1, Ordering::SeqCst);
                // A little work, so the nodes overlap.
                tokio::time::sleep(Duration::from_millis(15)).await;
                Ok::<HandlerResult, JobError>(HandlerResult::Ok)
            })
        });
        let mut policies = HashMap::new();
        policies.insert("default".to_owned(), QueuePolicy::custom("default").build());
        let worker = build_worker(
            Arc::new(PgJobStore::new(pool.clone())) as Arc<dyn JobStore>,
            Arc::new(registry),
            policies,
            Arc::new(SystemClock::new()),
            FinalAttemptHooks::new(),
        );
        let (tx, rx) = watch::channel(false);
        let worker_id = format!("node-{node}:{}", env.site);
        let handle = tokio::spawn(async move {
            let _ = worker
                .run(
                    &worker_id,
                    vec!["default".to_owned()],
                    rx,
                    Duration::from_secs(1),
                )
                .await;
        });
        stops.push((tx, handle));
    }

    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let total: u32 = per_node.iter().map(|c| c.load(Ordering::SeqCst)).sum();
        if total >= 200 || Instant::now() > deadline {
            break;
        }
        sleep_ms(100).await;
    }
    // Let a duplicate, if any, show itself.
    sleep_ms(1_000).await;
    for (tx, handle) in stops {
        let _ = tx.send(true);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("the worker stops")
            .expect("the worker did not panic");
    }

    let ran = ran.lock().unwrap();
    let total: u32 = per_node.iter().map(|c| c.load(Ordering::SeqCst)).sum();
    assert_eq!(total, 200, "the nodes ran {total} jobs, not 200");
    assert_eq!(ran.len(), 200, "{} distinct jobs ran", ran.len());
    for (i, nodes) in ran.iter() {
        assert_eq!(nodes.len(), 1, "job {i} ran on nodes {nodes:?}");
    }
    let busy = per_node
        .iter()
        .filter(|c| c.load(Ordering::SeqCst) > 0)
        .count();
    assert!(busy > 1, "one node did all the work: {per_node:?}");
}

/// The hosted worker takes the queue only under the consumer lease: of three keepers on the
/// same lease name exactly one holds it, and after it releases another does, with a larger token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_node_holds_the_consumer_lease() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(env) = env_with_schedule(&services("cluster")).await else {
        return;
    };
    let pool = pool_of(&env).await;
    let coordinator = observer(&pool).await;
    let name = spot_cli::services::jobs::consumer_lease(&env.site);
    let keepers: Vec<_> = (0..3)
        .map(|n| {
            spot_cli::services::jobs::Keeper::new(
                coordinator.clone() as Arc<dyn Coordinator>,
                name.clone(),
                format!("node-{n}"),
                TTL,
            )
        })
        .collect();
    let mut won = Vec::new();
    for (n, keeper) in keepers.iter().enumerate() {
        if keeper.acquire().await {
            won.push(n);
        }
    }
    assert_eq!(won.len(), 1, "keepers holding the consumer lease: {won:?}");
    let before = coordinator.current(&name).await.unwrap().unwrap();
    keepers[won[0]].release().await;
    let mut next = None;
    for (n, keeper) in keepers.iter().enumerate() {
        if n != won[0] && keeper.acquire().await {
            next = Some(n);
            break;
        }
    }
    assert!(next.is_some(), "no other node took the released lease");
    let after = coordinator.current(&name).await.unwrap().unwrap();
    assert!(after.token > before.token);
}

// ---- 3. leader loss ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_leader_loses_no_firing_and_repeats_none() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(env) = env_with_schedule(&services("cluster")).await else {
        return;
    };
    let pool = pool_of(&env).await;
    let watcher = observer(&pool).await;
    let name = scheduler_lease(&env.site);
    let mut hosts = Hosts::start(&env, 3).await;
    let before = leader(&watcher, &env.site, Duration::from_secs(10)).await;
    sleep_ms(3_000).await;

    // The leader is the host whose stop moves the lease; a standby's stop leaves it where it
    // was. The stop waits for the drain, which gives the lease up.
    let mut handed_over = None;
    for index in 0..3 {
        hosts.stop(index).await;
        let started = Instant::now();
        let now = loop {
            match watcher.current(&name).await.unwrap() {
                Some(lease) if lease.token == before.token => break None,
                Some(lease) => break Some(lease),
                None if started.elapsed() > TTL + Duration::from_secs(2) => {
                    panic!("no node took over within ttl + 2 s")
                }
                None => sleep_ms(100).await,
            }
        };
        if let Some(lease) = now {
            assert!(lease.token > before.token, "the fencing token did not grow");
            assert_ne!(lease.holder, before.holder);
            assert!(
                started.elapsed() <= TTL + Duration::from_secs(2),
                "the takeover took {:?}",
                started.elapsed()
            );
            handed_over = Some(index);
            break;
        }
    }
    assert!(
        handed_over.is_some(),
        "stopping the hosts never moved the lease"
    );
    // Keep firing for a few more seconds under the new leader.
    sleep_ms(4_000).await;
    hosts.finish_all().await;

    let rows = firings(&pool, &env.site).await;
    assert_each_once_and_none_missed(&rows);
}

// ---- 4. a leader that stops renewing ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_leader_that_stops_renewing_is_fenced_out() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(env) = env_with_schedule(&services("cluster")).await else {
        return;
    };
    let pool = pool_of(&env).await;
    let watcher = observer(&pool).await;
    let name = scheduler_lease(&env.site);
    let mut hosts = Hosts::start(&env, 3).await;
    let old = leader(&watcher, &env.site, Duration::from_secs(10)).await;
    sleep_ms(2_000).await;
    let mark_before = watcher.peek(&mark_counter(&env.site)).await.unwrap();

    // The lease row lapses as it does when its holder dies without releasing: its expiry passes.
    // (The harness has no way to kill a host without its drain releasing the lease.)
    {
        let client = pool.get().await.unwrap();
        client
            .execute(
                "UPDATE spot_coord_lease SET expires_at = now() - interval '1 second', \
                 holder = $2 WHERE name = $1",
                &[&name, &"dead-node"],
            )
            .await
            .unwrap();
    }
    // Some host takes it with a larger token within ttl + 2 s.
    let deadline = Instant::now() + TTL + Duration::from_secs(2);
    let taken = loop {
        match watcher.current(&name).await.unwrap() {
            Some(lease) if lease.holder != "dead-node" && lease.token > old.token => break lease,
            _ => {
                assert!(Instant::now() < deadline, "no host took the lapsed lease");
                sleep_ms(100).await;
            }
        }
    };
    // The old token is stale: a writer holding it is refused and the mark does not go back.
    let stale = watcher.check_fence(&name, old.token).await.unwrap_err();
    assert_eq!(stale.code, spot_coordinator::error::LEASE_LOST);
    watcher.check_fence(&name, taken.token).await.unwrap();
    sleep_ms(3_000).await;
    let mark_after = watcher.peek(&mark_counter(&env.site)).await.unwrap();
    assert!(mark_after >= mark_before, "the mark went back");
    assert_eq!(
        watcher
            .set_at_least(&mark_counter(&env.site), 1)
            .await
            .unwrap(),
        mark_after,
        "set_at_least lowered the mark"
    );
    hosts.finish_all().await;

    let rows = firings(&pool, &env.site).await;
    assert_each_once_and_none_missed(&rows);
}

// ---- 5. rate limit ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_rate_limit_holds_across_three_coordinators() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(env) = env_with_schedule(&services("cluster")).await else {
        return;
    };
    let pool = pool_of(&env).await;
    // Nothing hosted takes from the coordinator under a stable name in this tree, so the limit is
    // asserted on the three hosts' kind of handle: three coordinators over the one database.
    let handles: Vec<Arc<PgCoordinator>> = vec![
        observer(&pool).await,
        observer(&pool).await,
        observer(&pool).await,
    ];
    let limit = RateLimit {
        capacity: 20,
        per: Duration::from_secs(60),
    };
    let key = format!("test:signin:{}", env.site);
    let mut callers = Vec::new();
    for handle in &handles {
        for _ in 0..50 {
            let (handle, key) = (Arc::clone(handle), key.clone());
            callers.push(tokio::spawn(async move {
                matches!(
                    handle.take(&key, limit, 1).await.unwrap(),
                    Admit::Allowed { .. }
                )
            }));
        }
    }
    let mut admitted = 0;
    for caller in callers {
        if caller.await.unwrap() {
            admitted += 1;
        }
    }
    assert!(admitted <= 20, "{admitted} calls admitted of a limit of 20");
    assert_eq!(admitted, 20, "the bucket was not used up: {admitted}");
}

// ---- 6. the single-node profile refuses a second node ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_process_coordinator_refuses_a_second_node() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(env) = env_with_schedule(&single_node_services()).await else {
        return;
    };
    let mut hosts = Hosts::start(&env, 1).await;
    let pool = pool_of(&env).await;
    let admin = observer(&pool).await;
    let presence = Arc::new(PgPresence::new(pool.clone(), Duration::from_secs(30)));
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let first = InProcessCoordinator::start(
        presence.clone(),
        NodeId::new("node-first").unwrap(),
        clock.clone(),
    )
    .await
    .expect("the first node starts");
    let refused = InProcessCoordinator::start(presence, NodeId::new("node-second").unwrap(), clock)
        .await
        .expect_err("a second node on the same database is refused");
    assert_eq!(refused.code, SECOND_NODE);
    // The first node and the first host keep running: the first still grants leases, the host
    // still serves.
    let lease = first
        .acquire("scheduler:alone", "node-first", Duration::from_secs(5))
        .await
        .unwrap();
    first.release(&lease).await.unwrap();
    assert!(
        !hosts.0[0]
            .as_ref()
            .unwrap()
            .running
            .local_addrs()
            .is_empty()
    );
    // The profile's own database coordinator is unaffected by presence rows.
    admin
        .acquire("scheduler:other", "x", Duration::from_secs(5))
        .await
        .unwrap();
    hosts.finish_all().await;
}
