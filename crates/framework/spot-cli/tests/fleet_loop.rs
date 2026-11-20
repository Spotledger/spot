//! The fleet loops end to end (WP-CTL-01.T10): the real control-plane modules and the real
//! tenant-host emitters, proving WP-CTL-03 ("seeded failures appear within one minute"), WP-CTL-04
//! ("a desktop activates and renews offline within the grace period") and WP-CTL-05 ("quotas react
//! to usage") agree on paths, JSON, certificate identity, limits and error codes.
//!
//! Time is injected on both sides (one `ManualClock`); no test sleeps. The usage case needs
//! PostgreSQL (`SPOT_TEST_PG_URL`; it prints why it skips, never fakes). See `fleet_loop/rig.rs`
//! for what the link carries, and the report for what this rig does not cover (the TLS handshake,
//! a real `spot serve` site).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "fleet_loop/rig.rs"]
mod rig;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use axum::http::{Method, StatusCode};
use rig::{
    INTERVAL, Link, LinkLicence, LinkUsage, NOW, Rig, TempDir, fresh_tenant, healthy_knobs,
    heartbeat, licence_host,
};
use serde_json::Value;
use spot_cli::services::licence::renew::renew_once;
use spot_cli::services::licence::{LicenceGate, Standing, code};
use spot_cli::services::metering::{
    Counters, MemoryOutbox, MeteringPorts, MeteringService, Settings as MeteringSettings, SiteSalt,
    UsageSources,
};
use spot_license::{Action, Status};

const DAY: i64 = 86_400;

fn colour(node: &Value) -> &str {
    node["health"].as_str().unwrap_or("none")
}

// ---- a. heartbeat to colour ------------------------------------------------------------------

#[tokio::test]
async fn a_seeded_failure_shows_within_a_minute_a_silent_host_goes_stale_and_a_restart_is_green() {
    let rig = Rig::new(None).await;
    let tenant = fresh_tenant();
    let dir = TempDir::new("a");
    let link = Link::new(&rig, &tenant, "h1");
    let (probe, errors, lag) = healthy_knobs();
    let knobs = [probe.clone(), errors.clone(), lag.clone()];
    let site = heartbeat(&rig, &link, dir.path(), &knobs, 64);

    // Healthy: green within two intervals.
    for _ in 0..2 {
        let report = site.pass().await;
        assert!(
            report.reached && report.shipped == 1,
            "heartbeat loop: {report:?}"
        );
        rig.advance(INTERVAL);
    }
    let node = rig.node(&tenant, "h1").await;
    assert_eq!(colour(&node), "green", "heartbeat loop: {node}");

    // Seed a failure: the probe fails and the queue backs up. It must show within 60 seconds.
    probe.set(0);
    lag.set(100_000);
    errors.set(2_500);
    let seeded_at = rig.now();
    let mut shown = None;
    while rig.now() - seeded_at <= 60 {
        site.pass().await;
        let node = rig.node(&tenant, "h1").await;
        if colour(&node) != "green" {
            shown = Some((rig.now() - seeded_at, colour(&node).to_owned()));
            break;
        }
        rig.advance(INTERVAL);
    }
    let (after, seen) = shown.expect("heartbeat loop: a seeded failure never showed within 60 s");
    assert!(
        after <= 60 && (seen == "red" || seen == "amber"),
        "{after}s {seen}"
    );

    // Recovery turns it green again.
    probe.set(1);
    lag.set(0);
    errors.set(0);
    rig.advance(INTERVAL);
    site.pass().await;
    assert_eq!(colour(&rig.node(&tenant, "h1").await), "green");

    // The host stops reporting: after the freshness window it is no longer green.
    rig.advance(46);
    let node = rig.node(&tenant, "h1").await;
    assert_eq!(colour(&node), "amber", "heartbeat loop: {node}");
    assert_eq!(node["stale"], true);

    // It restarts (a new emitter over the same data directory): green again.
    let restarted = heartbeat(&rig, &link, dir.path(), &knobs, 64);
    restarted.pass().await;
    let node = rig.node(&tenant, "h1").await;
    assert_eq!(colour(&node), "green", "heartbeat loop: {node}");
    assert_ne!(node["stale"], true);
}

// ---- b. outage and recovery ------------------------------------------------------------------

#[tokio::test]
async fn an_outage_of_ten_intervals_keeps_the_newest_observations_and_never_repeats_a_batch() {
    let rig = Rig::new(None).await;
    let tenant = fresh_tenant();
    let dir = TempDir::new("b");
    let link = Link::new(&rig, &tenant, "h1");
    let (probe, errors, lag) = healthy_knobs();
    let site = heartbeat(&rig, &link, dir.path(), &[probe, errors, lag], 4);
    site.pass().await;
    assert_eq!(link.accepted.lock().unwrap().len(), 1);

    link.down.store(true, Ordering::SeqCst);
    for _ in 0..10 {
        rig.advance(INTERVAL);
        let report = site.pass().await;
        assert!(!report.reached && report.shipped == 0, "{report:?}");
    }
    assert_eq!(
        site.outbox().len().unwrap(),
        4,
        "bounded: the newest four are kept"
    );
    assert!(
        site.outbox().gap().is_some(),
        "the oldest were summarised as a gap"
    );

    link.down.store(false, Ordering::SeqCst);
    rig.advance(INTERVAL);
    let report = site.pass().await;
    assert!(
        report.reached && report.shipped == 4,
        "heartbeat loop: {report:?}"
    );

    // Every accepted batch is distinct and in time order: no duplicate, nothing out of order.
    let times: Vec<i64> = link
        .accepted
        .lock()
        .unwrap()
        .iter()
        .map(|body| {
            let value: Value = serde_json::from_slice(body).unwrap();
            value["observations"][0]["observed_at"].as_i64().unwrap()
        })
        .collect();
    let mut sorted = times.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        times, sorted,
        "heartbeat loop: batches repeated or reordered"
    );
    assert_eq!(times.len(), 5);
    assert!(
        site.outbox().gap().is_none(),
        "the gap closed once shipped past"
    );
    let node = rig.node(&tenant, "h1").await;
    assert_eq!(colour(&node), "green", "heartbeat loop: {node}");
}

// ---- c. licence offline and renew ------------------------------------------------------------

#[tokio::test]
async fn a_desktop_activates_works_offline_through_grace_and_renews_without_a_restart() {
    let rig = Rig::new(None).await;
    let tenant = fresh_tenant();
    let dir = TempDir::new("c");
    let link = Link::new(&rig, &tenant, "h1");
    let fetcher = LinkLicence(Arc::clone(&link));
    assert_eq!(
        rig.issue_licence(&tenant, NOW + 10 * DAY).await,
        StatusCode::CREATED
    );
    let host = licence_host(&rig, dir.path(), &tenant, true);

    // Activation: required, no file yet, only renewal is allowed; the first fetch installs it.
    assert!(!host.decide(Action::CreateDocument).is_allow());
    let first = renew_once(&host, &fetcher, true).await;
    assert_eq!(first.code, "ok", "licence loop: {first:?}");
    assert_eq!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Active)
    );

    // The control plane goes away. The site works through Active ...
    link.down.store(true, Ordering::SeqCst);
    rig.advance(5 * DAY as u64);
    assert_eq!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Active)
    );
    assert!(host.decide(Action::CreateDocument).is_allow());
    // ... and Grace, with full function.
    rig.advance(6 * DAY as u64);
    assert!(matches!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Grace { .. })
    ));
    assert!(host.decide(Action::CreateDocument).is_allow());
    let attempt = renew_once(&host, &fetcher, false).await;
    assert_eq!(
        attempt.code,
        code::FETCH_UNREACHABLE,
        "offline in grace is silent and harmless"
    );
    assert!(host.decide(Action::CreateDocument).is_allow());

    // After the grace it is Restricted: no new documents, export still allowed.
    rig.advance(8 * DAY as u64);
    assert_eq!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Restricted)
    );
    assert_eq!(
        host.decide(Action::CreateDocument).reason(),
        Some("licence.restricted")
    );
    assert!(host.decide(Action::ExportData).is_allow());

    // The control plane returns and the operator renews: the next check applies it, no restart.
    link.down.store(false, Ordering::SeqCst);
    assert_eq!(
        rig.renew_licence(&tenant, rig.now() + 30 * DAY).await,
        StatusCode::OK
    );
    let renewed = renew_once(&host, &fetcher, false).await;
    assert_eq!(renewed.code, "ok", "licence loop: {renewed:?}");
    assert_eq!(renewed.installed.unwrap().serial, 2);
    assert_eq!(
        LicenceGate::standing(&host),
        Standing::Licensed(Status::Active)
    );
    assert!(host.decide(Action::CreateDocument).is_allow());

    // A revoked licence is no longer served: the site learns there is none and keeps what it
    // holds unchanged (it cannot install a licence it was not given).
    assert_eq!(rig.revoke_licence(&tenant).await, StatusCode::OK);
    let after = renew_once(&host, &fetcher, true).await;
    assert_eq!(after.code, code::FETCH_NONE, "licence loop: {after:?}");
    assert_eq!(host.licence().unwrap().serial, 2);
}

// ---- d. usage and quota (PostgreSQL) ----------------------------------------------------------

struct Site {
    counters: Mutex<Counters>,
    users: Mutex<Vec<String>>,
}

impl UsageSources for Site {
    fn counters(&self) -> Result<Counters, String> {
        Ok(*self.counters.lock().unwrap())
    }
    fn active_users(&self, _from: i64, _until: i64) -> Result<Vec<String>, String> {
        Ok(self.users.lock().unwrap().clone())
    }
}

struct Salt;
impl SiteSalt for Salt {
    fn salt(&self) -> Vec<u8> {
        b"fleet-loop-salt".to_vec()
    }
}

async fn pg_pool() -> Option<deadpool_postgres::Pool> {
    let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set (the usage module needs PostgreSQL)");
        return None;
    };
    let mut config = deadpool_postgres::Config::new();
    config.url = Some(url);
    Some(
        config
            .create_pool(
                Some(deadpool_postgres::Runtime::Tokio1),
                tokio_postgres::NoTls,
            )
            .unwrap(),
    )
}

async fn set_quota(pool: &deadpool_postgres::Pool, tenant: &str, limit: i64, mode: &str) {
    let client = pool.get().await.unwrap();
    client
        .execute(
            "INSERT INTO cp_usage_quota (tenant, meter, limit_value, mode) VALUES ($1, 'transactions', $2, $3) \
             ON CONFLICT (tenant, meter) DO UPDATE SET limit_value = EXCLUDED.limit_value, mode = EXCLUDED.mode",
            &[&tenant, &limit, &mode],
        )
        .await
        .unwrap();
}

fn metering_settings(tenant: &str) -> MeteringSettings {
    MeteringSettings {
        enabled: true,
        tenant: tenant.to_owned(),
        control_plane: "https://cp.example.net:8443".to_owned(),
        ca_cert: String::new(),
        host_cert: "unused".to_owned(),
        host_key: "unused".to_owned(),
        interval_seconds: 3_600,
        outbox_max: 1_000,
        batch: 500,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_reaches_the_control_plane_once_and_the_quota_answer_flips() {
    let Some(pool) = pg_pool().await else { return };
    let rig = Rig::new(Some(pool.clone())).await;
    let tenant = fresh_tenant();
    let link = Link::new(&rig, &tenant, "h1");
    set_quota(&pool, &tenant, 10, "hard").await;
    let site = Arc::new(Site {
        counters: Mutex::new(Counters::default()),
        users: Mutex::new(vec!["alice".to_owned(), "bob".to_owned()]),
    });
    let ports = Arc::new(MeteringPorts {
        sources: Arc::clone(&site) as Arc<dyn UsageSources>,
        salt: Arc::new(Salt),
        outbox: Arc::new(MemoryOutbox::new()),
        shipper: Arc::new(LinkUsage(Arc::clone(&link))),
    });
    let clock: Arc<dyn spot_core::Clock> = rig.clock.clone();
    let meter = MeteringService::new(ports, metering_settings(&tenant), clock);

    // The first sample is the baseline; history before the service existed is not billed.
    meter.pass().await;
    *site.counters.lock().unwrap() = Counters {
        transactions: 12,
        ai_tokens: 0,
        storage_bytes: 4_096,
    };
    rig.advance(3_600);
    let report = meter.pass().await;
    assert!(
        report.shipped >= 1 && report.waiting == 0,
        "usage loop: {report:?}"
    );

    let (status, usage) = rig
        .operator("GET", &format!("/cp/v1/usage?tenant={tenant}"), "")
        .await;
    assert_eq!(status, StatusCode::OK, "usage loop: {usage}");
    let meters = usage["data"]["meters"].as_array().unwrap();
    let transactions = meters
        .iter()
        .find(|m| m.to_string().contains("transactions"))
        .unwrap_or_else(|| panic!("usage loop: no transactions meter in {usage}"));
    assert_eq!(transactions["total"], 12, "usage loop: {usage}");

    // The quota answer for this host flips to a hard exceed (12 > 10).
    let (status, quota) = link
        .call(Method::GET, "/cp/v1/host/quota", Vec::new())
        .await
        .unwrap();
    assert_eq!(status, 200);
    assert!(
        String::from_utf8_lossy(&quota).contains("hard_exceeded"),
        "usage loop: {quota:?}"
    );

    // Idempotent: the same batch sent again (a restart that lost its cursor) counts nothing.
    // The last accepted POST body (the quota GET above is recorded too, with an empty body).
    let body = link
        .accepted
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|b| !b.is_empty())
        .cloned()
        .unwrap();
    let (status, again) = link
        .call(Method::POST, "/cp/v1/host/usage", body)
        .await
        .unwrap();
    assert_eq!(
        status,
        200,
        "usage loop: {}",
        String::from_utf8_lossy(&again)
    );
    let again: Value = serde_json::from_slice(&again).unwrap();
    assert_eq!(again["data"]["accepted"], 0, "usage loop: {again}");
    let (_, usage) = rig
        .operator("GET", &format!("/cp/v1/usage?tenant={tenant}"), "")
        .await;
    let transactions = usage["data"]["meters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m.to_string().contains("transactions"))
        .unwrap();
    assert_eq!(transactions["total"], 12);
}

// ---- e. isolation ------------------------------------------------------------------------------

#[tokio::test]
async fn one_tenants_certificate_cannot_speak_for_or_read_another_tenant() {
    let rig = Rig::new(None).await;
    let (a, b, nobody) = (fresh_tenant(), fresh_tenant(), fresh_tenant());
    let dir = TempDir::new("e");
    let link_a = Link::new(&rig, &a, "h1");
    let link_b = Link::new(&rig, &b, "h1");
    let (probe, errors, lag) = healthy_knobs();
    let site_a = heartbeat(&rig, &link_a, dir.path(), &[probe.clone(), errors, lag], 8);
    site_a.pass().await;
    assert_eq!(colour(&rig.node(&a, "h1").await), "green");

    // B's host cannot post an observation about A: a body that names another tenant is refused
    // (the tenant comes from the certificate only) and A's view does not move.
    rig.advance(46);
    let named = serde_json::json!({
        "observations": [{ "scope": "tenant", "signal": "health_probe", "value": 1, "tenant": a }]
    })
    .to_string();
    let (status, _) = link_b
        .call(Method::POST, "/cp/v1/host/heartbeat", named.into_bytes())
        .await
        .unwrap();
    assert!(status == 400 || status == 422, "isolation: {status}");
    assert_eq!(
        rig.node(&a, "h1").await["stale"],
        true,
        "isolation: B's call refreshed A's node"
    );

    // Licences: A has one, B and a tenant that never existed get the same answer.
    assert_eq!(
        rig.issue_licence(&a, NOW + 10 * DAY).await,
        StatusCode::CREATED
    );
    let (own, _) = link_a
        .call(Method::GET, "/cp/v1/host/licence", Vec::new())
        .await
        .unwrap();
    assert_eq!(own, 200);
    let ghost = Link::new(&rig, &nobody, "h1");
    let (for_b, body_b) = link_b
        .call(Method::GET, "/cp/v1/host/licence", Vec::new())
        .await
        .unwrap();
    let (for_ghost, body_ghost) = ghost
        .call(Method::GET, "/cp/v1/host/licence", Vec::new())
        .await
        .unwrap();
    assert_eq!((for_b, for_ghost), (404, 404));
    let strip = |body: &[u8]| {
        let mut value: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
        value["error"]
            .as_object_mut()
            .map(|e| e.remove("request_id"));
        value
    };
    assert_eq!(
        strip(&body_b),
        strip(&body_ghost),
        "isolation: a missing tenant answers differently"
    );

    // A host whose certificate was revoked (a deprovisioned tenant) is refused everywhere.
    let fingerprint = format!("fp-{a}-h1");
    rig.plane
        .host_ca
        .revoke(&fingerprint, "root")
        .await
        .unwrap();
    rig.advance(INTERVAL);
    probe.set(1);
    let report = site_a.pass().await;
    assert!(
        !report.reached,
        "isolation: a revoked certificate was still accepted"
    );
    let (status, _) = link_a
        .call(Method::GET, "/cp/v1/host/licence", Vec::new())
        .await
        .unwrap();
    assert!(status == 401 || status == 403, "isolation: {status}");
}

// ---- the drift guard ----------------------------------------------------------------------------

/// The host endpoints the emitters call. Renaming one side without the other fails here first.
#[tokio::test]
async fn every_endpoint_an_emitter_calls_is_served_by_the_control_plane_with_the_same_method() {
    let pool = pg_pool().await;
    let have_usage = pool.is_some();
    let rig = Rig::new(pool).await;
    let tenant = fresh_tenant();
    let link = Link::new(&rig, &tenant, "h1");
    let mut endpoints: Vec<(Method, Method, &str)> = vec![
        (
            Method::POST,
            Method::GET,
            spot_cli::services::heartbeat::ENDPOINT,
        ),
        (
            Method::GET,
            Method::POST,
            spot_cli::services::licence::renew::ENDPOINT,
        ),
    ];
    if have_usage {
        endpoints.push((Method::POST, Method::GET, "/cp/v1/host/usage"));
        endpoints.push((Method::GET, Method::POST, "/cp/v1/host/quota"));
    }
    for (method, wrong, path) in endpoints {
        let (status, body) = link
            .call(method.clone(), path, b"{}".to_vec())
            .await
            .unwrap();
        assert!(
            !(status == 404 && body.is_empty()) && status != 405,
            "drift: the control plane does not serve {method} {path} ({status})"
        );
        let (status, _) = link
            .call(wrong.clone(), path, b"{}".to_vec())
            .await
            .unwrap();
        assert_eq!(status, 405, "drift: {path} also answers {wrong}");
    }
}
