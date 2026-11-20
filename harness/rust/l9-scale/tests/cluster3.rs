//! The three-node cluster proofs (WP-TST-15.T6): three `spot serve` nodes of one site on one
//! PostgreSQL, behind a load balancer. Each proof is a test and starts its own cluster; they
//! run one after another (one cluster is a dozen processes).
//!
//! Needs `SPOT_TEST_PG_URL` (a TCP URL); without it each test prints why and returns, never
//! faked. The cluster-redis proofs also need `redis-server` on the path and skip with a printed
//! reason without it. `L9_DURATION_SCALE` (default `0.1`) scales the 5 minute leader sampling.
//!
//! Two sites run on the three nodes; every proof ends with the isolation check (a marker role
//! of each site must not appear in the other's answers).
//!
//! Not proven here, and listed as such in the project process notes: a scheduled job
//! fires once per slot and in-flight jobs are retried exactly once (need a scheduled job and a
//! job that can be observed); partition ownership (`partition.not_owner`, needs a `[partition]`
//! site profile and a partitioned DocType).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../src/cluster.rs"]
mod cluster;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cluster::{Cluster, ClusterOptions, call};
use l9_scale::clock::{Clock, MonotonicClock};
use l9_scale::http::Target;
use l9_scale::launcher;
use l9_scale::profile::{Profile, parse_decimal};
use serde_json::{Value as Json, json};

const SCALE_ENV: &str = "L9_DURATION_SCALE";
const DEFAULT_PERMILLE: u64 = 100;
/// The lease time to live the cluster runs with, milliseconds.
const LEASE_TTL_MS: u64 = 4_000;
/// The leader must change within this many TTLs of its loss.
const FAILOVER_TTLS: u64 = 2;
/// Writes may be attempted this many times before one counts as failed.
const WRITE_ATTEMPTS: u32 = 3;

/// One cluster at a time.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn load_profile() -> Profile {
    Profile::load(&crate_dir().join("profiles/cluster3.toml")).unwrap()
}

fn scale_permille() -> u64 {
    std::env::var(SCALE_ENV)
        .ok()
        .and_then(|v| parse_decimal(&v, 3))
        .map_or(DEFAULT_PERMILLE, |n| n.clamp(1, 1000))
}

async fn start(redis: bool) -> Option<Cluster> {
    let options = ClusterOptions {
        repo: launcher::default_repo(),
        spot_bin: None,
        nodes: 3,
        run_id: launcher::random_hex(12),
        sites: 2,
        redis,
        lease_ttl_ms: LEASE_TTL_MS,
    };
    match Cluster::start(&options).await {
        Ok(Some(cluster)) => Some(cluster),
        Ok(None) => {
            println!(
                "SKIPPED: the cluster proofs need SPOT_TEST_PG_URL (and redis-server for the redis variant)"
            );
            None
        }
        Err(e) => panic!("cannot start the cluster: {e}"),
    }
}

/// How many nodes hold the scheduler lease right now, and which; `None` for a node whose
/// metrics cannot be read (a stopped node).
async fn scheduler_holders(cluster: &Cluster, alive: &[usize]) -> Vec<usize> {
    let mut holders = Vec::new();
    for i in alive {
        if let Some(view) = cluster.leases_of(*i).await
            && view.scheduler_held
        {
            holders.push(*i);
        }
    }
    holders
}

/// Waits (bounded by `limit_ms` on `clock`) for exactly one scheduler holder among `alive`;
/// the holder and the milliseconds it took.
async fn wait_for_one_leader(
    cluster: &Cluster,
    alive: &[usize],
    clock: &MonotonicClock,
    limit_ms: u64,
) -> Result<(usize, u64), String> {
    let start = clock.now_ns();
    loop {
        let holders = scheduler_holders(cluster, alive).await;
        let elapsed_ms = (clock.now_ns() - start) / 1_000_000;
        if let [one] = holders.as_slice() {
            return Ok((*one, elapsed_ms));
        }
        if elapsed_ms > limit_ms {
            return Err(format!(
                "no single leader within {limit_ms} ms (holders: {holders:?})"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn make_role(cluster: &Cluster, site: &str, name: &str) -> (u16, String) {
    let target = cluster.lb_target(site);
    let bearer = cluster.admin_bearer(&target).await.unwrap();
    let (status, body) = call(
        &target,
        &bearer,
        "POST",
        "/api/v1/resource/role",
        Some(&json!({ "role_name": name, "origin": "reference" })),
    )
    .await
    .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn list_roles(cluster: &Cluster, site: &str) -> String {
    let target = cluster.lb_target(site);
    let bearer = cluster.admin_bearer(&target).await.unwrap();
    let (status, body) = call(
        &target,
        &bearer,
        "POST",
        "/api/v1/resource/role/query",
        Some(&json!({ "limit": 1000 })),
    )
    .await
    .unwrap();
    assert_eq!(status, 200, "the role list answered {status}");
    String::from_utf8_lossy(&body).into_owned()
}

/// The isolation check: each site holds its own marker role and never shows the other's.
struct Markers {
    first: String,
    second: String,
}

async fn plant_markers(cluster: &Cluster) -> Markers {
    let first = format!("l9iso{}", launcher::random_hex(12));
    let second = format!("l9iso{}", launcher::random_hex(12));
    let other = cluster.other_sites.first().expect("two sites").clone();
    assert_eq!(make_role(cluster, &cluster.site, &first).await.0, 201);
    assert_eq!(make_role(cluster, &other, &second).await.0, 201);
    Markers { first, second }
}

async fn assert_isolated(cluster: &Cluster, markers: &Markers, when: &str) {
    let other = cluster.other_sites.first().expect("two sites").clone();
    let a = list_roles(cluster, &cluster.site).await;
    let b = list_roles(cluster, &other).await;
    assert!(
        a.contains(&markers.first),
        "{when}: the first site lost its own marker"
    );
    assert!(
        b.contains(&markers.second),
        "{when}: the second site lost its own marker"
    );
    assert!(
        !a.contains(&markers.second),
        "{when}: the first site showed the second site's data"
    );
    assert!(
        !b.contains(&markers.first),
        "{when}: the second site showed the first site's data"
    );
}

// --- (a) metadata propagation, (b) single leader --------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_metadata_change_on_node_a_is_visible_on_b_and_c_and_there_is_one_leader() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(cluster) = start(false).await else {
        return;
    };
    let markers = plant_markers(&cluster).await;
    let clock = MonotonicClock::new();
    let profile = load_profile();

    // (b) exactly one scheduler holder at every sample after the first election.
    let (leader, took_ms) = wait_for_one_leader(&cluster, &[0, 1, 2], &clock, 30_000)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    println!("l9 cluster: first leader is node {leader} after {took_ms} ms");
    let window_s = (profile.steady_seconds * scale_permille() / 1000).max(10);
    for second in 0..window_s {
        let holders = scheduler_holders(&cluster, &[0, 1, 2]).await;
        assert_eq!(
            holders.len(),
            1,
            "second {second}: scheduler lease holders {holders:?}, expected exactly one"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    // (a) a custom field added on node A shows on B and C within the 2 s invalidation SLA.
    let field = format!("l9_note_{}", launcher::random_hex(6));
    let a = cluster.node_target(0, &cluster.site);
    let bearer_a = cluster.admin_bearer(&a).await.unwrap();
    let (status, body) = call(
        &a,
        &bearer_a,
        "POST",
        "/api/v1/method/platform/customise/add_custom_field",
        Some(&json!({ "doctype": "role", "field": { "name": field, "label": "L9 note", "type": "text" } })),
    )
    .await
    .unwrap();
    assert!(
        status == 200 || status == 201,
        "add_custom_field on node A answered {status}: {}",
        String::from_utf8_lossy(&body)
            .chars()
            .take(300)
            .collect::<String>()
    );
    let changed = clock.now_ns();
    for i in [1usize, 2] {
        let target = cluster.node_target(i, &cluster.site);
        let bearer = cluster.admin_bearer(&target).await.unwrap();
        let seen_after_ms = loop {
            let (status, raw) = call(&target, &bearer, "GET", "/api/v1/meta/role", None)
                .await
                .unwrap();
            if status == 200 && String::from_utf8_lossy(&raw).contains(&field) {
                break (clock.now_ns() - changed) / 1_000_000;
            }
            let waited = (clock.now_ns() - changed) / 1_000_000;
            assert!(
                waited < 10_000,
                "node {i} never showed the new field (waited {waited} ms)"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        println!("l9 cluster: node {i} shows the field after {seen_after_ms} ms");
        assert!(
            seen_after_ms <= 2_000 + 500,
            "node {i} took {seen_after_ms} ms, above the 2 s SLA (+500 ms probe)"
        );
        // A document with the field validates on that node.
        let (status, raw) = call(
            &target,
            &bearer,
            "POST",
            "/api/v1/resource/role",
            Some(&json!({ "role_name": format!("l9meta{i}{}", launcher::random_hex(6)), "origin": "reference", field.clone(): "x" })),
        )
        .await
        .unwrap();
        assert_eq!(
            status,
            201,
            "a document with the new field answered {status} on node {i}: {}",
            String::from_utf8_lossy(&raw)
                .chars()
                .take(300)
                .collect::<String>()
        );
    }
    assert_isolated(&cluster, &markers, "after the metadata change").await;
    cluster.shutdown().await;
}

// --- (c) kill the leader, (e) sessions survive node loss ------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn killing_the_leader_moves_the_lease_within_two_ttls_and_a_session_survives_the_loss() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(mut cluster) = start(false).await else {
        return;
    };
    let markers = plant_markers(&cluster).await;
    let clock = MonotonicClock::new();
    let (leader, _) = wait_for_one_leader(&cluster, &[0, 1, 2], &clock, 30_000)
        .await
        .unwrap_or_else(|e| panic!("{e}"));

    // (e) a bearer token minted on the leader, which is about to die.
    let leader_target = cluster.node_target(leader, &cluster.site);
    let token = cluster.admin_bearer(&leader_target).await.unwrap();

    // (c) the lease passes to another node within 2 TTLs (plus one probe interval).
    cluster.kill_node(leader).await;
    let alive: Vec<usize> = (0..3).filter(|i| *i != leader).collect();
    let (next, took_ms) = wait_for_one_leader(
        &cluster,
        &alive,
        &clock,
        FAILOVER_TTLS * LEASE_TTL_MS + 1_000,
    )
    .await
    .unwrap_or_else(|e| panic!("the lease did not pass: {e}"));
    println!("l9 cluster: lease passed from node {leader} to node {next} in {took_ms} ms");
    assert_ne!(next, leader);

    // (e) the token still works on the survivors.
    for i in &alive {
        let target = cluster.node_target(*i, &cluster.site);
        let (status, _) = call(&target, &token, "GET", "/api/v1/auth/session", None)
            .await
            .unwrap();
        assert_eq!(
            status, 200,
            "a token minted on node {leader} answered {status} on node {i} after node {leader} was killed \
             (sessions are not shared across nodes)"
        );
    }
    assert_isolated(&cluster, &markers, "after the leader was killed").await;

    // The killed node comes back and does not take the lease from the new leader.
    cluster.start_node(leader).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2 * LEASE_TTL_MS / 1000)).await;
    let holders = scheduler_holders(&cluster, &[0, 1, 2]).await;
    assert_eq!(
        holders,
        vec![next],
        "the restarted node must not take the lease"
    );
    cluster.shutdown().await;
}

// --- (f) rolling restart under write load -----------------------------------------------------------

struct Writer {
    acked: Vec<String>,
    failed: u64,
}

async fn write_loop(
    cluster_admin: (Target, String),
    stop: Arc<AtomicBool>,
    run: String,
    rate_milli: u64,
) -> Writer {
    let (target, password) = cluster_admin;
    let mut out = Writer {
        acked: Vec::new(),
        failed: 0,
    };
    let mut bearer = login(&target, &password).await.unwrap_or_default();
    let period = Duration::from_millis((1_000_000 / rate_milli.max(1)).max(1));
    let mut n = 0u64;
    while !stop.load(Ordering::SeqCst) {
        n += 1;
        let name = format!("l9seq{run}x{n}");
        let mut acked = false;
        let mut ambiguous = false;
        for _ in 0..WRITE_ATTEMPTS {
            match call(
                &target,
                &bearer,
                "POST",
                "/api/v1/resource/role",
                Some(&json!({ "role_name": name, "origin": "reference" })),
            )
            .await
            {
                Ok((201, _)) => {
                    acked = true;
                    break;
                }
                // A retry after a lost answer finds the row already there.
                Ok((409, _)) if ambiguous => {
                    acked = true;
                    break;
                }
                // A node that restarted does not know the token: sign in again (this proof is
                // about acknowledged writes; the session proof is a separate test).
                Ok((401, _)) => {
                    if let Ok(fresh) = login(&target, &password).await {
                        bearer = fresh;
                    }
                    ambiguous = true;
                }
                Ok(_) | Err(_) => {
                    ambiguous = true;
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
        if acked {
            out.acked.push(name);
        } else {
            out.failed += 1;
        }
        tokio::time::sleep(period).await;
    }
    out
}

async fn login(target: &Target, password: &str) -> Result<String, String> {
    let body = json!({ "user": "administrator", "password": password });
    let mut conn = l9_scale::http::Conn::new();
    let bytes = body.to_string().into_bytes();
    let r = conn
        .request(
            target,
            "POST",
            "/api/v1/auth/login",
            &[("Content-Type", "application/json")],
            Some(&bytes),
            Duration::from_secs(10),
        )
        .await
        .map_err(|e| format!("{e:?}"))?;
    if r.status != 200 {
        return Err(format!("HTTP {}", r.status));
    }
    r.json()
        .pointer("/data/token")
        .and_then(Json::as_str)
        .map(|t| format!("Bearer {t}"))
        .ok_or_else(|| "no token".to_owned())
}

async fn all_role_names(cluster: &Cluster) -> BTreeSet<String> {
    let target = cluster.lb_target(&cluster.site);
    let bearer = cluster.admin_bearer(&target).await.unwrap();
    let mut names = BTreeSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({ "limit": 500 });
        if let Some(c) = &cursor {
            body["cursor"] = Json::String(c.clone());
        }
        let (status, raw) = call(
            &target,
            &bearer,
            "POST",
            "/api/v1/resource/role/query",
            Some(&body),
        )
        .await
        .unwrap();
        assert_eq!(status, 200, "the role list answered {status}");
        let page: Json = serde_json::from_slice(&raw).unwrap();
        for row in page
            .get("data")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(n) = row.get("role_name").and_then(Json::as_str) {
                names.insert(n.to_owned());
            }
        }
        cursor = page
            .get("next_cursor")
            .and_then(Json::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return names;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_rolling_restart_under_5_writes_per_second_loses_no_acknowledged_write() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(mut cluster) = start(false).await else {
        return;
    };
    let markers = plant_markers(&cluster).await;
    let profile = load_profile();
    let rate_milli = profile.target_milli_tps();
    let run = launcher::random_hex(8);
    let stop = Arc::new(AtomicBool::new(false));
    let writer = tokio::spawn(write_loop(
        (
            cluster.lb_target(&cluster.site),
            cluster.admin_password.clone(),
        ),
        stop.clone(),
        run,
        rate_milli,
    ));
    tokio::time::sleep(Duration::from_secs(3)).await;
    // One node after another: stop it as an operator does (SIGTERM), start it, wait until it
    // is ready (start_node does), and let the balancer see it before the next one goes.
    for i in 0..3 {
        cluster.stop_node(i).await;
        cluster.start_node(i).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    stop.store(true, Ordering::SeqCst);
    let written = writer.await.unwrap();
    println!(
        "l9 cluster: {} writes acknowledged, {} failed after {WRITE_ATTEMPTS} attempts",
        written.acked.len(),
        written.failed
    );
    assert!(
        written.acked.len() > 20,
        "the writer acknowledged only {} writes",
        written.acked.len()
    );
    assert_eq!(
        written.failed, 0,
        "writes failed beyond the retry budget during the rolling restart"
    );

    let names = all_role_names(&cluster).await;
    let lost: Vec<&String> = written
        .acked
        .iter()
        .filter(|n| !names.contains(*n))
        .collect();
    assert!(
        lost.is_empty(),
        "acknowledged writes are missing after the restart: {lost:?}"
    );
    assert_isolated(&cluster, &markers, "after the rolling restart").await;
    cluster.shutdown().await;
}

// --- loss of PostgreSQL: refuse writes, do not serve stale -------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_node_cut_off_from_postgres_refuses_writes_and_recovers() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(cluster) = start(false).await else {
        return;
    };
    let target = cluster.node_target(0, &cluster.site);
    let bearer = cluster.admin_bearer(&target).await.unwrap();
    cluster.partition_node(0, true);
    let mut refused = false;
    for attempt in 0..10 {
        let name = format!("l9cut{}x{attempt}", launcher::random_hex(6));
        match call(
            &target,
            &bearer,
            "POST",
            "/api/v1/resource/role",
            Some(&json!({ "role_name": name, "origin": "reference" })),
        )
        .await
        {
            Ok((201, _)) => panic!("a node without PostgreSQL acknowledged a write"),
            // Any other answer (an error status or a dropped connection) is a refusal.
            Ok(_) | Err(_) => refused = true,
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(refused);
    cluster.partition_node(0, false);
    // It recovers without a restart.
    let mut recovered = false;
    for _ in 0..60 {
        let name = format!("l9back{}", launcher::random_hex(8));
        if let Ok((201, _)) = call(
            &target,
            &bearer,
            "POST",
            "/api/v1/resource/role",
            Some(&json!({ "role_name": name, "origin": "reference" })),
        )
        .await
        {
            recovered = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        recovered,
        "the node did not accept writes within 60 s of the partition healing"
    );
    cluster.shutdown().await;
}

// --- the cluster-redis variant -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_redis_cluster_has_one_leader_fails_closed_without_redis_and_resumes() {
    let _one = ONE_AT_A_TIME.lock().await;
    let Some(mut cluster) = start(true).await else {
        return;
    };
    let clock = MonotonicClock::new();
    // (b) one leader.
    let (leader, _) = wait_for_one_leader(&cluster, &[0, 1, 2], &clock, 30_000)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    // (c) kill the leader: the lease passes.
    cluster.kill_node(leader).await;
    let alive: Vec<usize> = (0..3).filter(|i| *i != leader).collect();
    wait_for_one_leader(
        &cluster,
        &alive,
        &clock,
        FAILOVER_TTLS * LEASE_TTL_MS + 1_000,
    )
    .await
    .unwrap_or_else(|e| panic!("the lease did not pass with Redis: {e}"));
    cluster.start_node(leader).await.unwrap();

    // Redis killed: no node keeps a lease (fail closed, jobs stop claiming).
    cluster.redis.as_mut().expect("redis").kill().await;
    let start = clock.now_ns();
    loop {
        let holders = scheduler_holders(&cluster, &[0, 1, 2]).await;
        if holders.is_empty() {
            break;
        }
        assert!(
            (clock.now_ns() - start) / 1_000_000 < 3 * LEASE_TTL_MS + 5_000,
            "nodes {holders:?} still hold the scheduler lease with Redis gone"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // Redis back: one leader returns.
    cluster
        .redis
        .as_mut()
        .expect("redis")
        .restart()
        .await
        .unwrap();
    wait_for_one_leader(&cluster, &[0, 1, 2], &clock, 60_000)
        .await
        .unwrap_or_else(|e| panic!("no leader after Redis returned: {e}"));
    cluster.shutdown().await;
}

// --- no-PostgreSQL tests ----------------------------------------------------------------------------------

#[test]
fn the_cluster3_profile_carries_the_proof_parameters() {
    let profile = load_profile();
    assert_eq!(profile.name, "cluster3");
    assert_eq!(
        profile.steady_seconds, 300,
        "the single-leader proof samples for 5 minutes"
    );
    assert_eq!(profile.target_milli_tps(), 5_000, "5 writes per second");
}

#[test]
fn a_postgres_url_is_split_and_pointed_at_a_proxy() {
    let (host, port, prefix, tail) =
        cluster::split_pg_url("postgres://u:p%40ss@db.example:6543/spot?sslmode=disable").unwrap();
    assert_eq!((host.as_str(), port), ("db.example", 6543));
    assert_eq!(prefix, "postgres://u:p%40ss@");
    assert_eq!(tail, "/spot?sslmode=disable");
    assert_eq!(
        cluster::pg_url_via(&prefix, &tail, 7000),
        "postgres://u:p%40ss@127.0.0.1:7000/spot?sslmode=disable"
    );
    let (host, port, _, _) = cluster::split_pg_url("postgresql://localhost/postgres").unwrap();
    assert_eq!((host.as_str(), port), ("localhost", 5432));
    assert!(cluster::split_pg_url("postgres:///db?host=/var/run/postgresql").is_none());
    assert!(cluster::split_pg_url("not a url").is_none());
}

#[test]
fn lease_metrics_are_read_from_prometheus_text() {
    let text = "spot_lease_held{lease=\"scheduler\"} 1\nspot_lease_held{lease=\"consumer\"} 0\n\
        spot_lease_events_total{lease=\"scheduler\",event=\"lost\"} 2\n\
        spot_lease_events_total{lease=\"consumer\",event=\"lost\"} 1\n\
        spot_lease_events_total{lease=\"scheduler\",event=\"renewed\"} 40\n";
    let view = cluster::parse_leases(text);
    assert!(view.scheduler_held);
    assert!(!view.consumer_held);
    assert_eq!(view.lost, 3);
    assert_eq!(
        cluster::parse_leases("nothing"),
        cluster::LeaseView::default()
    );
}

#[tokio::test]
async fn a_cut_proxy_drops_traffic_and_a_restored_one_relays_again() {
    // An echo server stands in for PostgreSQL.
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = echo.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = echo.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (mut r, mut w) = s.split();
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
        }
    });
    let proxy = cluster::TcpProxy::start(upstream).await.unwrap();
    let ping = |port: u16| async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .ok()?;
        s.write_all(b"ping").await.ok()?;
        let mut buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(2), s.read_exact(&mut buf))
            .await
            .ok()?
            .ok()?;
        Some(buf)
    };
    assert_eq!(ping(proxy.port).await, Some(*b"ping"));
    proxy.set_cut(true);
    assert_eq!(ping(proxy.port).await, None, "a cut proxy must not relay");
    proxy.set_cut(false);
    assert_eq!(ping(proxy.port).await, Some(*b"ping"));
}
