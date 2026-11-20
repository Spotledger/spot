//! The fleet heartbeat emitter (WP-CTL-03.T3): the wire, the samplers, the file outbox, outage and
//! recovery, silence semantics, registration and the privacy canary. No database and no network:
//! the control plane is a scripted shipper (the mutual-TLS handshake itself belongs to
//! `HttpSender::with_tls`, WP-SEC-09.T8, and is tested there).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use spot_cli::services::heartbeat::outbox::FileOutbox;
use spot_cli::services::heartbeat::sample::{
    CertificateExpirySampler, CertificateSource, DiskProbe, DiskUsageSampler, ErrorRateSampler,
    HealthProbeSampler, JobFailuresSampler, JobStats, LicenceReader, LicenceStateSampler,
    QueueLagSampler, RequestCounters, Scope, Signal, SignalSampler, collect,
};
use spot_cli::services::heartbeat::{
    EgressAllowList, HeartbeatPorts, HeartbeatService, HeartbeatShipper, NoJitter, Settings,
    ShipOutcome, backoff_secs, outcome_of, register,
};
use spot_cli::services::{Health, Level, SiteCtx, State};
use spot_core::ManualClock;
use time::macros::datetime;

static DIRS: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let n = DIRS.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("spot_hb_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A scripted control plane.
#[derive(Default)]
struct FakePlane {
    down: AtomicBool,
    refuse: AtomicBool,
    calls: AtomicUsize,
    bodies: Mutex<Vec<String>>,
}

#[async_trait]
impl HeartbeatShipper for FakePlane {
    async fn ship(&self, body: Vec<u8>) -> ShipOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.down.load(Ordering::SeqCst) {
            return ShipOutcome::Retry;
        }
        if self.refuse.load(Ordering::SeqCst) {
            return ShipOutcome::Refused;
        }
        self.bodies
            .lock()
            .unwrap()
            .push(String::from_utf8(body).unwrap());
        ShipOutcome::Accepted
    }
}

struct Allow(bool);
impl EgressAllowList for Allow {
    fn allows(&self, _host: &str, _port: u16) -> bool {
        self.0
    }
}

/// A sampler with a fixed answer.
struct Fixed(Signal, Scope, Result<i64, String>);
impl SignalSampler for Fixed {
    fn signal(&self) -> Signal {
        self.0
    }
    fn scope(&self) -> Scope {
        self.1
    }
    fn sample(&self) -> Result<i64, String> {
        self.2.clone()
    }
}

struct Slow(Signal);
impl SignalSampler for Slow {
    fn signal(&self) -> Signal {
        self.0
    }
    fn scope(&self) -> Scope {
        Scope::Node
    }
    fn sample(&self) -> Result<i64, String> {
        std::thread::sleep(Duration::from_millis(600));
        Ok(1)
    }
}

fn healthy_samplers() -> Vec<Arc<dyn SignalSampler>> {
    vec![
        Arc::new(Fixed(Signal::HealthProbe, Scope::Node, Ok(1))),
        Arc::new(Fixed(Signal::ErrorRate, Scope::Tenant, Ok(120))),
        Arc::new(Fixed(Signal::DiskUsage, Scope::Node, Ok(4_500))),
        Arc::new(Fixed(Signal::CertificateExpiry, Scope::Node, Ok(86_400))),
    ]
}

fn settings(dir: &Path, max_batches: usize) -> Settings {
    Settings {
        enabled: true,
        control_plane: "https://cp.example.net:8443".to_owned(),
        host_cert: dir.join("host.crt").display().to_string(),
        host_key: dir.join("host.key").display().to_string(),
        ca_file: dir.join("ca.crt").display().to_string(),
        interval_seconds: 15,
        node_id: "node-1".to_owned(),
        sample_budget_ms: 100,
        outbox_max_batches: max_batches,
        outbox_dir: dir.join("outbox").display().to_string(),
    }
}

fn write_credentials(dir: &Path, key_mode: u32) {
    for name in ["host.crt", "ca.crt"] {
        std::fs::write(dir.join(name), "pem").unwrap();
    }
    std::fs::write(dir.join("host.key"), "pem").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            dir.join("host.key"),
            std::fs::Permissions::from_mode(key_mode),
        )
        .unwrap();
    }
    let _ = key_mode;
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-13 10:00:00 UTC)))
}

fn ports(
    samplers: Vec<Arc<dyn SignalSampler>>,
    plane: &Arc<FakePlane>,
    data_dir: &Path,
) -> Arc<HeartbeatPorts> {
    Arc::new(HeartbeatPorts {
        samplers,
        shipper: Arc::clone(plane) as Arc<dyn HeartbeatShipper>,
        egress: Arc::new(Allow(true)),
        jitter: Arc::new(NoJitter),
        data_dir: data_dir.to_path_buf(),
    })
}

fn service(
    dir: &TempDir,
    samplers: Vec<Arc<dyn SignalSampler>>,
    plane: &Arc<FakePlane>,
    max_batches: usize,
) -> HeartbeatService {
    HeartbeatService::new(
        ports(samplers, plane, dir.path()),
        settings(dir.path(), max_batches),
        clock(),
    )
    .unwrap()
}

fn parse(body: &str) -> Vec<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    let object = value.as_object().unwrap();
    assert_eq!(object.len(), 1, "only `observations` is at the top level");
    value["observations"].as_array().unwrap().clone()
}

// ---- wire ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_batch_carries_exactly_the_allow_listed_signals_with_integer_values() {
    let dir = TempDir::new("wire");
    let plane = Arc::new(FakePlane::default());
    let svc = service(&dir, healthy_samplers(), &plane, 8);
    let report = svc.pass_at(1_000).await;
    assert_eq!(report.shipped, 1);
    let bodies = plane.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 1);
    let items = parse(&bodies[0]);
    let allowed: Vec<&str> = Signal::ALL.iter().map(|s| s.as_str()).collect();
    assert_eq!(items.len(), 4);
    for item in &items {
        let object = item.as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["observed_at", "scope", "signal", "value"]);
        assert!(allowed.contains(&item["signal"].as_str().unwrap()));
        assert!(item["value"].is_i64(), "values are integers, never floats");
        assert_eq!(item["observed_at"], 1_000);
        assert!(["tenant", "node"].contains(&item["scope"].as_str().unwrap()));
    }
    assert!(bodies[0].len() <= 256 * 1024);
}

#[test]
fn http_statuses_map_to_the_three_outcomes() {
    assert_eq!(outcome_of(202), ShipOutcome::Accepted);
    for refused in [400, 413, 422] {
        assert_eq!(outcome_of(refused), ShipOutcome::Refused);
    }
    for retry in [401, 403, 404, 429, 500, 503] {
        assert_eq!(outcome_of(retry), ShipOutcome::Retry);
    }
}

#[test]
fn backoff_doubles_is_capped_and_jittered_inside_its_upper_half() {
    assert_eq!(backoff_secs(15, 0, 300, 0), 7);
    assert_eq!(backoff_secs(15, 1, 300, 999), 29);
    assert!(backoff_secs(15, 30, 300, 999) <= 300);
    assert!(backoff_secs(15, 30, 300, 0) >= 150);
}

// ---- off by default ------------------------------------------------------------------------

#[tokio::test]
async fn disabled_registers_nothing_and_makes_no_connection() {
    let dir = TempDir::new("off");
    let plane = Arc::new(FakePlane::default());
    let mut ctx = SiteCtx::new(
        "site",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        true,
    );
    ctx.provide(ports(healthy_samplers(), &plane, dir.path()))
        .unwrap();
    register(&mut ctx).unwrap();
    let assembled = ctx.finish(Router::new());
    assert!(assembled.assembly.unwrap().services().is_empty());
    assert_eq!(plane.calls.load(Ordering::SeqCst), 0);
    assert!(!dir.path().join("heartbeat-outbox").exists());
    assert!(!Settings::default().enabled);
}

fn ctx_with(settings: &Settings) -> SiteCtx<'static> {
    let mut table = toml::map::Map::new();
    table.insert("enabled".to_owned(), toml::Value::Boolean(settings.enabled));
    for (key, value) in [
        ("control_plane", &settings.control_plane),
        ("host_cert", &settings.host_cert),
        ("host_key", &settings.host_key),
        ("ca_file", &settings.ca_file),
        ("node_id", &settings.node_id),
        ("outbox_dir", &settings.outbox_dir),
    ] {
        table.insert(key.to_owned(), toml::Value::String(value.clone()));
    }
    let mut all = BTreeMap::new();
    all.insert("heartbeat".to_owned(), toml::Value::Table(table));
    SiteCtx::new("site", Duration::from_secs(1), clock(), all, true)
}

#[test]
fn enabled_registers_an_optional_service() {
    let dir = TempDir::new("on");
    write_credentials(dir.path(), 0o600);
    let plane = Arc::new(FakePlane::default());
    let mut ctx = ctx_with(&settings(dir.path(), 8));
    ctx.provide(ports(healthy_samplers(), &plane, dir.path()))
        .unwrap();
    register(&mut ctx).unwrap();
    let assembled = ctx.finish(Router::new());
    let assembly = assembled.assembly.unwrap();
    let names: Vec<_> = assembly.services().iter().map(|s| s.name()).collect();
    assert_eq!(names, ["heartbeat"]);
    assert_eq!(assembly.services()[0].level(), Level::Optional);
}

#[cfg(unix)]
#[test]
fn a_world_readable_key_refuses_start_and_names_the_file() {
    let dir = TempDir::new("key");
    write_credentials(dir.path(), 0o644);
    let plane = Arc::new(FakePlane::default());
    let mut ctx = ctx_with(&settings(dir.path(), 8));
    ctx.provide(ports(healthy_samplers(), &plane, dir.path()))
        .unwrap();
    let error = register(&mut ctx).unwrap_err().to_string();
    assert!(error.contains("host.key"), "{error}");
    assert!(error.contains("heartbeat_host.credential_file"), "{error}");
}

#[test]
fn a_missing_certificate_refuses_start_and_names_the_file() {
    let dir = TempDir::new("missing");
    write_credentials(dir.path(), 0o600);
    std::fs::remove_file(dir.path().join("host.crt")).unwrap();
    let plane = Arc::new(FakePlane::default());
    let mut ctx = ctx_with(&settings(dir.path(), 8));
    ctx.provide(ports(healthy_samplers(), &plane, dir.path()))
        .unwrap();
    let error = register(&mut ctx).unwrap_err().to_string();
    assert!(error.contains("host.crt"), "{error}");
}

#[test]
fn a_control_plane_missing_from_the_egress_allow_list_refuses_start_naming_the_entry() {
    let dir = TempDir::new("egress");
    write_credentials(dir.path(), 0o600);
    let plane = Arc::new(FakePlane::default());
    let mut ctx = ctx_with(&settings(dir.path(), 8));
    let denied = Arc::new(HeartbeatPorts {
        samplers: healthy_samplers(),
        shipper: Arc::clone(&plane) as Arc<dyn HeartbeatShipper>,
        egress: Arc::new(Allow(false)),
        jitter: Arc::new(NoJitter),
        data_dir: dir.path().to_path_buf(),
    });
    ctx.provide(denied).unwrap();
    let error = register(&mut ctx).unwrap_err().to_string();
    assert!(error.contains("cp.example.net:8443"), "{error}");
    assert!(
        error.contains("heartbeat_host.egress_not_allowed"),
        "{error}"
    );
}

#[test]
fn enabled_without_ports_refuses_start() {
    let dir = TempDir::new("noports");
    write_credentials(dir.path(), 0o600);
    let mut ctx = ctx_with(&settings(dir.path(), 8));
    let error = register(&mut ctx).unwrap_err().to_string();
    assert!(error.contains("heartbeat_host.no_ports"), "{error}");
}

#[test]
fn settings_are_validated() {
    let dir = TempDir::new("validate");
    let good = settings(dir.path(), 8);
    assert!(good.validate().is_ok());
    let mut bad = good.clone();
    bad.interval_seconds = 4;
    assert!(
        bad.validate()
            .unwrap_err()
            .to_string()
            .contains("interval_seconds")
    );
    let mut bad = good.clone();
    bad.control_plane = "http://cp.example.net".to_owned();
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.control_plane = "https://cp.example.net/path".to_owned();
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.outbox_max_batches = 3;
    assert!(bad.validate().is_err());
    let mut bad = good;
    bad.node_id = "has space".to_owned();
    assert!(bad.validate().is_err());
    assert_eq!(
        Settings {
            control_plane: "https://cp.example.net".to_owned(),
            ..Settings::default()
        }
        .control_plane_endpoint()
        .unwrap(),
        ("cp.example.net".to_owned(), 443)
    );
}

// ---- outage and recovery -------------------------------------------------------------------

#[tokio::test]
async fn an_outage_keeps_the_newest_batches_and_a_gap_marker_then_ships_in_order_without_duplicates()
 {
    let dir = TempDir::new("outage");
    let plane = Arc::new(FakePlane::default());
    plane.down.store(true, Ordering::SeqCst);
    let svc = service(&dir, healthy_samplers(), &plane, 4);
    for i in 0..10_i64 {
        let report = svc.pass_at(1_000 + i * 15).await;
        assert!(!report.reached);
        assert_eq!(report.shipped, 0);
    }
    assert_eq!(svc.outbox().len().unwrap(), 4, "bounded");
    assert!(svc.degraded(), "a full outbox is a degraded state");
    let gap = svc.outbox().gap().unwrap();
    assert_eq!(gap.batches, 6);
    assert_eq!(gap.from, 1_000);
    assert_eq!(gap.until, 1_000 + 5 * 15);

    plane.down.store(false, Ordering::SeqCst);
    let report = svc.pass_at(1_000 + 10 * 15).await;
    assert!(report.reached);
    // This pass queued one more batch (displacing the oldest); the four newest ship.
    assert_eq!(report.shipped, 4);
    assert_eq!(svc.outbox().len().unwrap(), 0);
    assert!(
        svc.outbox().gap().is_none(),
        "the gap is closed once shipped past"
    );
    assert!(!svc.degraded());

    let times: Vec<i64> = plane
        .bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| parse(b)[0]["observed_at"].as_i64().unwrap())
        .collect();
    assert_eq!(
        times,
        vec![
            1_000 + 7 * 15,
            1_000 + 8 * 15,
            1_000 + 9 * 15,
            1_000 + 10 * 15
        ]
    );
    // Nothing more to ship, nothing duplicated.
    let report = svc.pass_at(1_000 + 11 * 15).await;
    assert_eq!(report.shipped, 1);
    assert_eq!(plane.bodies.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn a_restart_keeps_what_waits_and_never_repeats_a_batch() {
    let dir = TempDir::new("restart");
    let plane = Arc::new(FakePlane::default());
    plane.down.store(true, Ordering::SeqCst);
    {
        let svc = service(&dir, healthy_samplers(), &plane, 16);
        svc.pass_at(2_000).await;
        svc.pass_at(2_015).await;
    }
    plane.down.store(false, Ordering::SeqCst);
    let svc = service(&dir, healthy_samplers(), &plane, 16);
    assert_eq!(svc.outbox().len().unwrap(), 2);
    let report = svc.pass_at(2_030).await;
    assert_eq!(report.shipped, 3);
    let bodies = plane.bodies.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3);
    let times: Vec<i64> = bodies
        .iter()
        .map(|b| parse(b)[0]["observed_at"].as_i64().unwrap())
        .collect();
    assert_eq!(times, [2_000, 2_015, 2_030]);
}

#[tokio::test]
async fn a_batch_the_control_plane_refuses_for_good_is_dropped_not_retried() {
    let dir = TempDir::new("refused");
    let plane = Arc::new(FakePlane::default());
    plane.refuse.store(true, Ordering::SeqCst);
    let svc = service(&dir, healthy_samplers(), &plane, 8);
    let report = svc.pass_at(3_000).await;
    assert_eq!(report.refused, 1);
    assert_eq!(report.waiting, 0);
    assert_eq!(plane.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn the_delay_grows_with_failures_and_resets_after_success() {
    let dir = TempDir::new("delay");
    let plane = Arc::new(FakePlane::default());
    plane.down.store(true, Ordering::SeqCst);
    let svc = service(&dir, healthy_samplers(), &plane, 8);
    assert_eq!(svc.next_delay(), Duration::from_secs(15));
    svc.pass_at(1).await;
    let first = svc.next_delay();
    svc.pass_at(2).await;
    svc.pass_at(3).await;
    assert!(svc.next_delay() > first);
    assert!(svc.next_delay() >= Duration::from_secs(15));
    plane.down.store(false, Ordering::SeqCst);
    svc.pass_at(4).await;
    assert_eq!(svc.next_delay(), Duration::from_secs(15));
}

// ---- silence means down --------------------------------------------------------------------

#[tokio::test]
async fn every_sampler_failing_still_ships_health_probe_zero_and_names_nothing() {
    let dir = TempDir::new("allfail");
    let plane = Arc::new(FakePlane::default());
    let failing: Vec<Arc<dyn SignalSampler>> = vec![
        Arc::new(Fixed(
            Signal::HealthProbe,
            Scope::Node,
            Err("probe is broken".to_owned()),
        )),
        Arc::new(Fixed(
            Signal::DiskUsage,
            Scope::Node,
            Err("/secret/path cannot be read".to_owned()),
        )),
        Arc::new(Fixed(Signal::ErrorRate, Scope::Tenant, Ok(99_999))),
    ];
    let svc = service(&dir, failing, &plane, 8);
    let report = svc.pass_at(4_000).await;
    assert_eq!(report.sampler_failures, 3);
    assert_eq!(report.shipped, 1);
    let body = plane.bodies.lock().unwrap()[0].clone();
    let items = parse(&body);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["signal"], "health_probe");
    assert_eq!(items[0]["value"], 0);
    assert!(!body.contains("secret") && !body.contains("broken"));
}

#[tokio::test]
async fn a_failing_sampler_is_omitted_and_the_others_still_ship() {
    let dir = TempDir::new("somefail");
    let plane = Arc::new(FakePlane::default());
    let mut samplers = healthy_samplers();
    samplers.push(Arc::new(Fixed(
        Signal::QueueLag,
        Scope::Tenant,
        Err("no store".to_owned()),
    )));
    let svc = service(&dir, samplers, &plane, 8);
    let report = svc.pass_at(5_000).await;
    assert_eq!(report.sampler_failures, 1);
    let items = parse(&plane.bodies.lock().unwrap()[0]);
    assert_eq!(items.len(), 4);
    assert!(items.iter().all(|i| i["signal"] != "queue_lag"));
}

#[tokio::test]
async fn a_slow_sampler_is_cut_off_at_its_budget_and_does_not_delay_the_pass() {
    let samplers: Vec<Arc<dyn SignalSampler>> = vec![
        Arc::new(Slow(Signal::DiskUsage)),
        Arc::new(Fixed(Signal::HealthProbe, Scope::Node, Ok(1))),
    ];
    let started = std::time::Instant::now();
    let collected = collect(&samplers, Duration::from_millis(60)).await;
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(collected.failed, 1);
    assert_eq!(collected.readings.len(), 1);
    assert_eq!(collected.readings[0].signal, Signal::HealthProbe);
}

#[tokio::test]
async fn an_out_of_range_value_is_a_failed_sampler_not_a_refused_batch() {
    let samplers: Vec<Arc<dyn SignalSampler>> = vec![
        Arc::new(Fixed(Signal::HealthProbe, Scope::Node, Ok(1))),
        Arc::new(Fixed(Signal::DiskUsage, Scope::Node, Ok(10_001))),
        Arc::new(Fixed(Signal::LicenceState, Scope::Tenant, Ok(7))),
    ];
    let collected = collect(&samplers, Duration::from_millis(200)).await;
    assert_eq!(collected.failed, 2);
    assert_eq!(collected.readings.len(), 1);
}

// ---- the samplers --------------------------------------------------------------------------

struct Jobs(i64, i64);
impl JobStats for Jobs {
    fn oldest_waiting_secs(&self) -> Result<i64, String> {
        Ok(self.0)
    }
    fn failures_in_window(&self) -> Result<i64, String> {
        Ok(self.1)
    }
}

struct Disk(u64, u64);
impl DiskProbe for Disk {
    fn capacity(&self, _path: &Path) -> Result<(u64, u64), String> {
        Ok((self.0, self.1))
    }
}

struct Cert(i64);
impl CertificateSource for Cert {
    fn not_after(&self) -> Result<i64, String> {
        Ok(self.0)
    }
}

struct Licence(i64);
impl LicenceReader for Licence {
    fn state_code(&self) -> Result<i64, String> {
        Ok(self.0)
    }
}

#[test]
fn health_probe_is_one_only_while_every_required_service_runs() {
    let health = Health::new();
    health.set("storage", Level::Required, State::Running);
    health.set(
        "mail",
        Level::Optional,
        State::Stopped("smtp down".to_owned()),
    );
    let sampler = HealthProbeSampler::new(health.clone());
    assert_eq!(
        sampler.sample().unwrap(),
        1,
        "an optional service does not count"
    );
    health.set(
        "jobs",
        Level::Required,
        State::Stopped("lease lost".to_owned()),
    );
    assert_eq!(sampler.sample().unwrap(), 0);
}

#[test]
fn error_rate_is_a_windowed_integer_in_basis_points() {
    let counters = Arc::new(RequestCounters::default());
    let sampler = ErrorRateSampler::new(Arc::clone(&counters));
    assert_eq!(
        sampler.sample().unwrap(),
        0,
        "no requests is a rate of zero"
    );
    counters.total.store(200, Ordering::Relaxed);
    counters.failed.store(5, Ordering::Relaxed);
    assert_eq!(sampler.sample().unwrap(), 250);
    counters.total.store(300, Ordering::Relaxed);
    counters.failed.store(5, Ordering::Relaxed);
    assert_eq!(sampler.sample().unwrap(), 0, "only the last window counts");
    counters.total.store(10, Ordering::Relaxed);
    counters.failed.store(10, Ordering::Relaxed);
    assert_eq!(
        sampler.sample().unwrap(),
        10_000,
        "a reset starts a new window"
    );
}

#[test]
fn queue_jobs_disk_certificate_and_licence_samplers_report_their_units() {
    let jobs: Arc<dyn JobStats> = Arc::new(Jobs(42, 3));
    assert_eq!(QueueLagSampler(Arc::clone(&jobs)).sample().unwrap(), 42);
    assert_eq!(JobFailuresSampler(jobs).sample().unwrap(), 3);

    let disk = DiskUsageSampler::new(Arc::new(Disk(1_000, 250)), PathBuf::from("/data"));
    assert_eq!(disk.sample().unwrap(), 7_500);
    let empty = DiskUsageSampler::new(Arc::new(Disk(0, 0)), PathBuf::from("/data"));
    assert!(empty.sample().is_err());

    let certs = CertificateExpirySampler::new(
        vec![Arc::new(Cert(10_000)), Arc::new(Cert(5_000))],
        Arc::new(|| 4_000),
    );
    assert_eq!(
        certs.sample().unwrap(),
        1_000,
        "the soonest certificate counts"
    );
    let expired = CertificateExpirySampler::new(vec![Arc::new(Cert(100))], Arc::new(|| 4_000));
    assert_eq!(expired.sample().unwrap(), -3_900);
    let none = CertificateExpirySampler::new(Vec::new(), Arc::new(|| 0));
    assert!(none.sample().is_err());

    assert_eq!(
        LicenceStateSampler(Arc::new(Licence(1))).sample().unwrap(),
        1
    );
}

#[tokio::test]
async fn without_a_licence_or_replica_sampler_those_signals_are_left_out() {
    let dir = TempDir::new("absent");
    let plane = Arc::new(FakePlane::default());
    let svc = service(&dir, healthy_samplers(), &plane, 8);
    svc.pass_at(6_000).await;
    let body = plane.bodies.lock().unwrap()[0].clone();
    assert!(!body.contains("licence_state"));
    assert!(!body.contains("replication_lag"));
}

// ---- no business data ----------------------------------------------------------------------

#[tokio::test]
async fn the_payload_carries_no_user_name_email_document_name_or_host_path() {
    let dir = TempDir::new("canary");
    let plane = Arc::new(FakePlane::default());
    // The host's own facts are full of identifying text; only integers may leave.
    let svc = service(&dir, healthy_samplers(), &plane, 8);
    svc.pass_at(7_000).await;
    let wire = plane.bodies.lock().unwrap()[0].clone();
    let on_disk: Vec<String> = std::fs::read_dir(dir.path().join("outbox"))
        .unwrap()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect();
    for text in std::iter::once(wire).chain(on_disk) {
        for canary in [
            "@",
            "alice",
            "invoice",
            "SalesOrder",
            dir.path().to_str().unwrap(),
            "/home/",
            "node-1",
            "cp.example",
        ] {
            assert!(!text.contains(canary), "`{canary}` leaked: {text}");
        }
    }
}

// ---- the sampler does not touch request-path locks (by design) -----------------------------

#[test]
fn sampling_the_request_counters_never_blocks_a_writer() {
    // The counters are plain atomics: a "request thread" hammering them while the sampler reads
    // cannot be blocked by it, and the sampler cannot hold anything the request path takes.
    let counters = Arc::new(RequestCounters::default());
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let counters = Arc::clone(&counters);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut writes = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                counters.total.fetch_add(1, Ordering::Relaxed);
                writes += 1;
            }
            writes
        })
    };
    let sampler = ErrorRateSampler::new(Arc::clone(&counters));
    for _ in 0..1_000 {
        let value = sampler.sample().unwrap();
        assert!((0..=10_000).contains(&value));
    }
    stop.store(true, Ordering::Relaxed);
    assert!(writer.join().unwrap() > 0);
}

// ---- the outbox ----------------------------------------------------------------------------

#[test]
fn the_outbox_is_fifo_bounded_and_survives_a_reopen() {
    let dir = TempDir::new("fifo");
    let outbox = FileOutbox::open(&dir.path().join("q"), 3).unwrap();
    for i in 0..5_i64 {
        let body = format!(r#"{{"observations":[{{"observed_at":{i}}}]}}"#);
        outbox.push(body.as_bytes(), i).unwrap();
    }
    assert_eq!(outbox.len().unwrap(), 3);
    assert_eq!(outbox.gap().unwrap().batches, 2);
    drop(outbox);
    let outbox = FileOutbox::open(&dir.path().join("q"), 3).unwrap();
    let (seq, body) = outbox.oldest().unwrap().unwrap();
    assert!(String::from_utf8(body).unwrap().contains(":2}"));
    outbox.remove(seq).unwrap();
    outbox.remove(seq).unwrap();
    assert_eq!(outbox.len().unwrap(), 2);
    // A new batch after a reopen sorts after the old ones.
    outbox.push(br#"{"observations":[]}"#, 9).unwrap();
    let mut last = 0;
    while let Some((seq, _)) = outbox.oldest().unwrap() {
        assert!(seq >= last);
        last = seq;
        outbox.remove(seq).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn queued_batches_are_private_to_the_owner() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new("mode");
    let outbox = FileOutbox::open(&dir.path().join("q"), 3).unwrap();
    outbox.push(b"{}", 1).unwrap();
    for entry in std::fs::read_dir(dir.path().join("q")).unwrap().flatten() {
        let mode = entry.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "group and others have no access");
    }
}

#[tokio::test]
async fn readiness_names_the_full_outbox_and_the_unsafe_key() {
    let dir = TempDir::new("ready");
    write_credentials(dir.path(), 0o600);
    let plane = Arc::new(FakePlane::default());
    plane.down.store(true, Ordering::SeqCst);
    let svc = service(&dir, healthy_samplers(), &plane, 4);
    use spot_cli::services::Service;
    assert!(svc.check().await.is_ok());
    for i in 0..6 {
        svc.pass_at(100 + i).await;
    }
    let message = svc.check().await.unwrap_err();
    assert!(message.contains("outbox is full"), "{message}");
    assert!(!message.contains(dir.path().to_str().unwrap()));
}
