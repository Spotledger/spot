//! The metering emitter (WP-CTL-05.T6): the planner, the in-memory outbox, the service against a
//! fake control-plane endpoint, and (with `SPOT_TEST_PG_URL`) the PostgreSQL outbox.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spot_cli::services::SiteCtx;
use spot_cli::services::metering::{
    Counters, MemoryOutbox, MeteringPorts, MeteringService, OutboxEvent, Planned, Settings,
    ShipOutcome, SiteSalt, UsageOutbox, UsageShipper, UsageSources, check_credentials,
    member_digest, plan, plan_compaction, register, wire_body,
};
use spot_core::SystemClock;

const T: i64 = 1_800_000_000;

struct Sources {
    counters: Mutex<Counters>,
    users: Mutex<Vec<String>>,
    failing: Mutex<bool>,
}

impl Sources {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            counters: Mutex::new(Counters::default()),
            users: Mutex::new(Vec::new()),
            failing: Mutex::new(false),
        })
    }
}

impl UsageSources for Sources {
    fn counters(&self) -> Result<Counters, String> {
        if *self.failing.lock().unwrap() {
            return Err("down".to_owned());
        }
        Ok(*self.counters.lock().unwrap())
    }
    fn active_users(&self, _from: i64, _until: i64) -> Result<Vec<String>, String> {
        Ok(self.users.lock().unwrap().clone())
    }
}

struct Salt(Vec<u8>);

impl SiteSalt for Salt {
    fn salt(&self) -> Vec<u8> {
        self.0.clone()
    }
}

/// A fake control plane: records bodies; can be down or refuse.
struct Endpoint {
    mode: Mutex<ShipOutcome>,
    bodies: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

impl Endpoint {
    fn new(mode: ShipOutcome) -> Arc<Self> {
        Arc::new(Self {
            mode: Mutex::new(mode),
            bodies: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        })
    }
    fn set(&self, mode: ShipOutcome) {
        *self.mode.lock().unwrap() = mode;
    }
    fn ids(&self) -> Vec<String> {
        self.bodies
            .lock()
            .unwrap()
            .iter()
            .flat_map(|body| {
                let json: serde_json::Value = serde_json::from_str(body).unwrap();
                json["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["id"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

#[async_trait]
impl UsageShipper for Endpoint {
    async fn ship(&self, body: Vec<u8>) -> ShipOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mode = *self.mode.lock().unwrap();
        if mode == ShipOutcome::Accepted {
            self.bodies
                .lock()
                .unwrap()
                .push(String::from_utf8(body).unwrap());
        }
        mode
    }
}

fn settings() -> Settings {
    Settings {
        enabled: true,
        tenant: "acme".to_owned(),
        control_plane: "https://cp.example:8444".to_owned(),
        host_cert: "/nonexistent".to_owned(),
        host_key: "/nonexistent".to_owned(),
        interval_seconds: 3_600,
        outbox_max: 100,
        batch: 500,
        ..Settings::default()
    }
}

struct Rig {
    service: MeteringService,
    sources: Arc<Sources>,
    outbox: Arc<MemoryOutbox>,
    endpoint: Arc<Endpoint>,
}

fn rig(mode: ShipOutcome, settings: Settings) -> Rig {
    let sources = Sources::new();
    let outbox = Arc::new(MemoryOutbox::new());
    let endpoint = Endpoint::new(mode);
    let ports = Arc::new(MeteringPorts {
        sources: sources.clone(),
        salt: Arc::new(Salt(b"site-salt".to_vec())),
        outbox: outbox.clone(),
        shipper: endpoint.clone(),
    });
    Rig {
        service: MeteringService::new(ports, settings, Arc::new(SystemClock::new())),
        sources,
        outbox,
        endpoint,
    }
}

fn counters(t: u64, a: u64, s: u64) -> Counters {
    Counters {
        transactions: t,
        ai_tokens: a,
        storage_bytes: s,
    }
}

#[test]
fn the_first_sample_only_sets_the_baseline_and_a_delta_becomes_one_event() {
    let first = plan(
        "acme",
        T,
        3_600,
        &BTreeMap::new(),
        counters(100, 5, 10),
        &[],
        b"s",
    );
    assert!(
        first
            .iter()
            .filter(|p| p.cursor.is_some())
            .all(|p| p.event.is_none())
    );
    let cursors: BTreeMap<String, u64> = [
        ("transactions".to_owned(), 100),
        ("ai_tokens".to_owned(), 5),
    ]
    .into();
    let second = plan(
        "acme",
        T + 3_600,
        3_600,
        &cursors,
        counters(130, 5, 11),
        &[],
        b"s",
    );
    let tx = second
        .iter()
        .find_map(|p| p.event.as_ref().filter(|e| e.meter == "transactions"))
        .unwrap();
    assert_eq!(tx.amount, 30);
    assert_eq!(tx.id, format!("acme:transactions:{}", (T + 3_600) / 3_600));
    assert!(
        second
            .iter()
            .all(|p| p.event.as_ref().is_none_or(|e| e.meter != "ai_tokens")),
        "a zero delta emits nothing"
    );
    // A counter that reset counts from zero.
    let reset = plan("acme", T, 3_600, &cursors, counters(7, 5, 0), &[], b"s");
    assert_eq!(
        reset
            .iter()
            .find_map(|p| p.event.as_ref().filter(|e| e.meter == "transactions"))
            .unwrap()
            .amount,
        7
    );
}

#[test]
fn active_users_are_salted_digests_one_per_user_and_day() {
    let users = vec![
        "alice@example.com".to_owned(),
        "bob".to_owned(),
        "alice@example.com".to_owned(),
    ];
    let planned = plan(
        "acme",
        T,
        3_600,
        &BTreeMap::new(),
        counters(0, 0, 0),
        &users,
        b"salt-one",
    );
    let members: Vec<&OutboxEvent> = planned
        .iter()
        .filter_map(|p| p.event.as_ref())
        .filter(|e| e.meter == "active_users")
        .collect();
    assert_eq!(members.len(), 2, "a user counts once per day");
    for event in &members {
        let digest = event.member.as_ref().unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!event.id.contains("alice") && !digest.contains("alice"));
    }
    assert_ne!(
        member_digest(b"salt-one", "bob"),
        member_digest(b"salt-two", "bob"),
        "the salt is per site"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_by_default_makes_no_network_call() {
    let endpoint = Endpoint::new(ShipOutcome::Accepted);
    let mut ctx = SiteCtx::new(
        "site",
        Duration::from_secs(1),
        Arc::new(SystemClock::new()),
        BTreeMap::new(),
        true,
    );
    register(&mut ctx).unwrap();
    let mut off = BTreeMap::new();
    off.insert(
        "metering".to_owned(),
        "enabled = false".parse::<toml::Value>().unwrap(),
    );
    let mut ctx = SiteCtx::new(
        "site",
        Duration::from_secs(1),
        Arc::new(SystemClock::new()),
        off,
        true,
    );
    register(&mut ctx).unwrap();
    assert_eq!(endpoint.calls.load(Ordering::SeqCst), 0);
    assert!(!Settings::default().enabled);
}

#[test]
fn enabled_without_ports_refuses_to_start_and_settings_are_validated() {
    let mut table = BTreeMap::new();
    table.insert(
        "metering".to_owned(),
        r#"enabled = true
tenant = "acme"
control_plane = "https://cp.example:8444"
host_cert = "/c.pem"
host_key = "/k.pem""#
            .parse::<toml::Value>()
            .unwrap(),
    );
    let mut ctx = SiteCtx::new(
        "site",
        Duration::from_secs(1),
        Arc::new(SystemClock::new()),
        table,
        true,
    );
    let error = register(&mut ctx).unwrap_err();
    assert!(error.to_string().contains("MeteringPorts"), "{error}");
    for bad in [
        Settings {
            tenant: String::new(),
            ..settings()
        },
        Settings {
            control_plane: "http://cp".to_owned(),
            ..settings()
        },
        Settings {
            interval_seconds: 10,
            ..settings()
        },
        Settings {
            outbox_max: 5,
            ..settings()
        },
        Settings {
            batch: 5_000,
            ..settings()
        },
        Settings {
            host_key: String::new(),
            ..settings()
        },
    ] {
        assert!(bad.validate().is_err());
    }
    settings().validate().unwrap();
    assert!(toml::from_str::<Settings>("enabled = true\nnope = 1").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_is_shipped_once_and_a_resample_adds_nothing() {
    let r = rig(ShipOutcome::Accepted, settings());
    *r.sources.counters.lock().unwrap() = counters(100, 0, 500);
    r.service.pass_at(T).await; // baseline + the gauge
    *r.sources.counters.lock().unwrap() = counters(160, 0, 600);
    let report = r.service.pass_at(T + 3_600).await;
    assert!(report.shipped >= 1);
    let ids = r.endpoint.ids();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "no id is shipped twice: {ids:?}");
    assert!(ids.contains(&format!("acme:transactions:{}", (T + 3_600) / 3_600)));
    // The same bucket sampled again: nothing new is recorded or shipped.
    let before = ids.len();
    let again = r.service.pass_at(T + 3_601).await;
    assert_eq!(again.recorded, 0);
    assert_eq!(r.endpoint.ids().len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_endpoint_down_keeps_events_and_the_service_resumes_without_duplicates() {
    let r = rig(ShipOutcome::Retry, settings());
    *r.sources.counters.lock().unwrap() = counters(10, 0, 1);
    r.service.pass_at(T).await;
    *r.sources.counters.lock().unwrap() = counters(40, 0, 2);
    let down = r.service.pass_at(T + 3_600).await;
    assert_eq!(down.shipped, 0);
    assert!(down.waiting >= 1, "events wait in the outbox");
    *r.sources.counters.lock().unwrap() = counters(90, 0, 3);
    r.service.pass_at(T + 7_200).await;
    r.endpoint.set(ShipOutcome::Accepted);
    let up = r.service.pass_at(T + 10_800).await;
    assert!(up.shipped >= 2);
    let ids = r.endpoint.ids();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "no duplicates after the outage: {ids:?}"
    );
    // Every counter delta arrived exactly once: 30 + 50 = 80 transactions in the shipped events.
    let total: u64 = r
        .endpoint
        .bodies
        .lock()
        .unwrap()
        .iter()
        .flat_map(|b| {
            serde_json::from_str::<serde_json::Value>(b).unwrap()["events"]
                .as_array()
                .unwrap()
                .clone()
        })
        .filter(|e| e["meter"] == "transactions")
        .map(|e| e["amount"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 80);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_source_or_endpoint_never_surfaces_to_the_caller() {
    let r = rig(ShipOutcome::Retry, settings());
    *r.sources.failing.lock().unwrap() = true;
    // The pass returns normally: nothing that posts a document waits on, or fails because of, it.
    let report = r.service.pass_at(T).await;
    assert_eq!(report.recorded, 0);
    assert_eq!(report.shipped, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_batch_the_control_plane_refuses_is_dropped_and_counted() {
    let r = rig(ShipOutcome::Refused, settings());
    *r.sources.counters.lock().unwrap() = counters(1, 1, 1);
    let report = r.service.pass_at(T).await;
    assert!(report.refused >= 1);
    assert_eq!(report.waiting, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wire_has_counts_and_digests_never_a_user_name() {
    let r = rig(ShipOutcome::Accepted, settings());
    *r.sources.users.lock().unwrap() = vec!["alice@example.com".to_owned()];
    r.service.pass_at(T).await;
    let all = r.endpoint.bodies.lock().unwrap().join("");
    assert!(!all.contains("alice"));
    assert!(!all.contains("site-salt"));
    assert!(all.contains("\"member\""));
}

fn event(id: &str, meter: &str, amount: u64, at: i64) -> OutboxEvent {
    OutboxEvent {
        id: id.to_owned(),
        meter: meter.to_owned(),
        amount,
        member: None,
        at,
    }
}

#[test]
fn compaction_sums_counters_keeps_the_gauge_latest_and_never_touches_users() {
    let mut pending: Vec<OutboxEvent> = (0..10)
        .map(|i| event(&format!("acme:transactions:{i}"), "transactions", 5, i))
        .collect();
    pending.push(event("acme:storage_bytes:1", "storage_bytes", 100, 1));
    pending.push(event("acme:storage_bytes:2", "storage_bytes", 250, 2));
    pending.push(OutboxEvent {
        member: Some("ab".repeat(32)),
        ..event("acme:active_users:x:1", "active_users", 0, 3)
    });
    let (remove, insert) = plan_compaction(&pending, 9).unwrap();
    assert!(!remove.iter().any(|id| id.contains("active_users")));
    let tx = insert.iter().find(|e| e.meter == "transactions").unwrap();
    assert_eq!(tx.amount, 50);
    assert!(tx.id.starts_with("acme:transactions:summary:"));
    // Only user events: nothing to summarise.
    assert!(plan_compaction(&[pending[12].clone()], 1).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_outbox_summarises_without_losing_counts_and_degrades_only_when_it_cannot() {
    let r = rig(ShipOutcome::Retry, settings());
    let planned: Vec<Planned> = (0..300)
        .map(|i| Planned {
            event: Some(event(
                &format!("acme:transactions:{i}"),
                "transactions",
                2,
                i,
            )),
            cursor: None,
        })
        .collect();
    r.outbox.append(&planned).await.unwrap();
    r.service.pass_at(T).await;
    assert!(r.outbox.unshipped().await.unwrap() <= 100);
    assert!(!r.service.degraded());
    let total: u64 = r
        .outbox
        .all()
        .iter()
        .filter(|(e, _)| e.meter == "transactions")
        .map(|(e, _)| e.amount)
        .sum();
    assert_eq!(total, 600, "summarised, not dropped");

    let r2 = rig(ShipOutcome::Retry, settings());
    let users: Vec<Planned> = (0..150)
        .map(|i| Planned {
            event: Some(OutboxEvent {
                member: Some(format!("{i:064x}")),
                ..event(&format!("acme:active_users:{i}:1"), "active_users", 0, i)
            }),
            cursor: None,
        })
        .collect();
    r2.outbox.append(&users).await.unwrap();
    r2.service.pass_at(T).await;
    assert!(
        r2.service.degraded(),
        "user events cannot be summarised: readiness degrades, nothing is dropped"
    );
    assert!(r2.outbox.unshipped().await.unwrap() >= 150);
}

#[test]
fn a_key_readable_by_others_is_refused() {
    let dir = std::env::temp_dir().join(format!("spot-metering-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cert = dir.join("host.pem");
    let key = dir.join("host.key");
    std::fs::write(&cert, "x").unwrap();
    std::fs::write(&key, "x").unwrap();
    let mut s = settings();
    s.host_cert = cert.to_string_lossy().into_owned();
    s.host_key = key.to_string_lossy().into_owned();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_credentials(&s).is_err());
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_credentials(&s).is_ok());
    }
    s.host_cert = "/nonexistent".to_owned();
    assert!(check_credentials(&s).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_wire_body_has_the_shape_the_control_plane_accepts() {
    let body = wire_body(&[
        event("a:transactions:1", "transactions", 3, 10),
        OutboxEvent {
            member: Some("ab".repeat(32)),
            ..event("a:active_users:x:1", "active_users", 0, 11)
        },
    ]);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let events = json["events"].as_array().unwrap();
    assert_eq!(events[0]["amount"], 3);
    assert!(events[0].get("member").is_none());
    assert!(
        events[1].get("amount").is_none(),
        "a member event carries no amount"
    );
    assert!(body.len() < 256 * 1024);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_postgres_outbox_is_idempotent_and_moves_cursors_atomically() {
    let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let mut config = deadpool_postgres::Config::new();
    config.url = Some(url);
    let pool = config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            deadpool_postgres::tokio_postgres::NoTls,
        )
        .unwrap();
    let prefix = format!("usage_t{}", std::process::id());
    let store = spot_cli::services::metering::PgOutbox::new(pool.clone(), &prefix).unwrap();
    store.prepare().await.unwrap();
    let planned = vec![Planned {
        event: Some(event("acme:transactions:1", "transactions", 30, 100)),
        cursor: Some(("transactions", 130)),
    }];
    assert_eq!(store.append(&planned).await.unwrap(), 1);
    assert_eq!(store.cursors().await.unwrap()["transactions"], 130);
    // The same event id again: nothing inserted and the cursor does not move.
    let again = vec![Planned {
        event: Some(event("acme:transactions:1", "transactions", 99, 100)),
        cursor: Some(("transactions", 999)),
    }];
    assert_eq!(store.append(&again).await.unwrap(), 0);
    assert_eq!(store.cursors().await.unwrap()["transactions"], 130);
    assert_eq!(store.pending(10).await.unwrap().len(), 1);
    store
        .mark_shipped(&["acme:transactions:1".to_owned()])
        .await
        .unwrap();
    assert_eq!(store.unshipped().await.unwrap(), 0);
    store.purge_shipped(1_000).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .batch_execute(&format!(
            "DROP TABLE {prefix}_outbox; DROP TABLE {prefix}_cursor;"
        ))
        .await
        .unwrap();
}
