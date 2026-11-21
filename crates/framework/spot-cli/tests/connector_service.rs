//! The connector service (WP-INT-05.T3), driven through its public API with connector components
//! written as text (the fixtures of `spot-connectors`' own tests): runs and the run log, retry then
//! dead letter with an alert, the durable vault (sealed at rest, per site, rotation, write-only),
//! per-connector egress (empty allow-list, private and metadata addresses), and a schedule slot
//! that two nodes turn into one job.
//!
//! No database is needed: the directory, run log and sealed store are the in-memory ones. The
//! PostgreSQL tables are exercised by the compile bundle's database run (`SPOT_TEST_PG_URL`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "../../spot-connectors/tests/common/mod.rs"]
mod fixtures;

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

use serde_json::json;
use spot_cli::services::apps::limits::{AppLimits, LimitTable};
use spot_cli::services::apps::store::MemoryBundleStore;
use spot_cli::services::apps::verify::Trust;
use spot_cli::services::connectors::defs::{
    ConnectorDef, ConnectorDirectory, MemoryDirectory, ScheduleDef, State,
};
use spot_cli::services::connectors::egress::PerConnectorHttp;
use spot_cli::services::connectors::runner::{AlertSink, Connectors, DEAD_LETTER, DISABLED, Parts};
use spot_cli::services::connectors::runs::{MemoryRunLog, Outcome};
use spot_cli::services::connectors::vault::{DurableVault, MemorySealedStore, SealedStore};
use spot_cli::services::connectors::{ConnectorsService, Settings};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_connectors::{
    ConnectorHost, ConnectorMonitor, ConnectorRuntime, CredentialRef, CredentialVault, HttpContext,
    HttpRequest, HttpService, MonitorConfig,
};
use spot_core::ManualClock;
use spot_egress::TestResolver;
use spot_jobs::worker::HandlerResult;
use spot_secrets::{MemoryKeyring, SecretString};
use spot_wasm::HostConfig;
use time::macros::datetime;

const SITE: &str = "site_a";

#[derive(Default)]
struct Alerts(Mutex<Vec<(String, String)>>);

impl AlertSink for Alerts {
    fn dead_letter(&self, _site: &str, connector: &str, code: &str) {
        self.0
            .lock()
            .unwrap()
            .push((connector.to_owned(), code.to_owned()));
    }
}

/// Deduplicates on (name, key), as the job store does.
#[derive(Default)]
struct DedupSubmit(Mutex<HashSet<String>>);

impl JobSubmit for DedupSubmit {
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async move {
            let key = format!("{}|{}", job.name, job.idempotency_key);
            Ok(self.0.lock().unwrap().insert(key))
        })
    }
}

struct Rig {
    engine: Arc<Connectors>,
    directory: Arc<MemoryDirectory>,
    runs: Arc<MemoryRunLog>,
    alerts: Arc<Alerts>,
    clock: Arc<ManualClock>,
}

fn def(name: &str) -> ConnectorDef {
    ConnectorDef {
        name: name.to_owned(),
        digest: "a".repeat(64),
        endpoints: Vec::new(),
        auth: None,
        secrets: Vec::new(),
        schedule: None,
        mapping: None,
        state: State::Enabled,
    }
}

fn rig(limits: LimitTable) -> Rig {
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-12 10:00 UTC)));
    let directory = Arc::new(MemoryDirectory::new());
    let runs = Arc::new(MemoryRunLog::new());
    let alerts = Arc::new(Alerts::default());
    let keyring = Arc::new(MemoryKeyring::new("k").unwrap());
    let vault = Arc::new(DurableVault::new(
        SITE,
        keyring,
        Arc::new(MemorySealedStore::new()),
    ));
    let http = Arc::new(PerConnectorHttp::new(
        SITE,
        Arc::new(TestResolver::new()),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    ));
    let host = Arc::new(ConnectorHost::new(
        Arc::new(ConnectorRuntime::new(HostConfig::default()).unwrap()),
        vault,
        Arc::clone(&http) as Arc<dyn HttpService>,
        Arc::new(ConnectorMonitor::new(MonitorConfig::default())),
    ));
    let engine = Arc::new(Connectors::new(Parts {
        site: SITE.to_owned(),
        host,
        http,
        directory: Arc::clone(&directory) as Arc<dyn ConnectorDirectory>,
        runs: Arc::clone(&runs) as Arc<dyn spot_cli::services::connectors::runs::RunLog>,
        alerts: Arc::clone(&alerts) as Arc<dyn AlertSink>,
        store: Arc::new(MemoryBundleStore::new()),
        trust: Arc::new(Trust::none(false)),
        clock: Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
        limits,
        max_concurrent_runs: 1,
    }));
    Rig {
        engine,
        directory,
        runs,
        alerts,
        clock,
    }
}

async fn add(rig: &Rig, definition: ConnectorDef, fixture: &fixtures::Fixture) {
    rig.directory.put(SITE, definition.clone());
    rig.engine
        .install_verified(definition, &fixtures::component(fixture))
        .await
        .unwrap();
}

fn args(connector: &str) -> serde_json::Value {
    Connectors::job_args(connector, "echo", &json!({"n": 1}))
}

// ---- runs -------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_run_succeeds_and_is_logged_without_its_payload() {
    let rig = rig(LimitTable::default());
    add(&rig, def("echo"), &fixtures::ECHO).await;
    assert_eq!(
        rig.engine.run_job(&args("echo"), 1, false).await,
        HandlerResult::Ok
    );
    let log = rig.runs.all();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].outcome, Outcome::Ok);
    assert!(log[0].code.is_none());
    assert!(!format!("{log:?}").contains("\"n\""));
    assert!(rig.engine.status()[0].last_success_age_secs.is_some());
    let _ = &rig.clock;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_upstream_retries_then_dead_letters_with_an_alert() {
    let mut limits = LimitTable::default();
    limits.apps.insert(
        "spin".to_owned(),
        AppLimits {
            fuel: Some(10_000),
            ..AppLimits::default()
        },
    );
    let rig = rig(limits);
    add(&rig, def("spin"), &fixtures::SPIN).await;
    // Out of fuel is a retryable failure: retried while attempts remain...
    assert!(matches!(
        rig.engine.run_job(&args("spin"), 1, false).await,
        HandlerResult::Retry(_)
    ));
    // ...and on the last attempt the connector goes to dead letter and an alert is raised.
    assert_eq!(
        rig.engine.run_job(&args("spin"), 3, true).await,
        HandlerResult::Fail(DEAD_LETTER.to_owned())
    );
    assert_eq!(rig.directory.state(SITE, "spin"), Some(State::DeadLetter));
    assert_eq!(rig.alerts.0.lock().unwrap().len(), 1);
    let outcomes: Vec<_> = rig.runs.all().iter().map(|r| r.outcome).collect();
    assert_eq!(outcomes, vec![Outcome::Retry, Outcome::DeadLetter]);
    // A dead-lettered connector runs no more, and the server (this task) is still serving.
    assert_eq!(
        rig.engine.run_job(&args("spin"), 1, false).await,
        HandlerResult::Fail(DISABLED.to_owned())
    );
    assert!(!rig.engine.degradation().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_connector_is_refused() {
    let rig = rig(LimitTable::default());
    assert_eq!(
        rig.engine.run_job(&args("nobody"), 1, false).await,
        HandlerResult::Fail(DISABLED.to_owned())
    );
}

// ---- schedule ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn two_nodes_ticking_one_slot_queue_one_job() {
    let submit = Arc::new(DedupSubmit::default());
    let mut nodes = Vec::new();
    for _ in 0..2 {
        let rig = rig(LimitTable::default());
        let mut scheduled = def("echo");
        scheduled.schedule = Some(ScheduleDef {
            every_seconds: 3_600,
            operation: "echo".to_owned(),
            input: json!({}),
        });
        add(&rig, scheduled, &fixtures::ECHO).await;
        let keyring = Arc::new(MemoryKeyring::new("k").unwrap());
        let admin = Arc::new(DurableVault::new(
            SITE,
            keyring,
            Arc::new(MemorySealedStore::new()),
        ));
        nodes.push(ConnectorsService::new(
            Settings::default(),
            Arc::clone(&rig.engine),
            Arc::clone(&submit) as Arc<dyn JobSubmit>,
            admin,
            Arc::clone(&rig.clock) as Arc<dyn spot_core::Clock>,
        ));
    }
    let first = nodes[0].schedule_tick().await.unwrap();
    let second = nodes[1].schedule_tick().await.unwrap();
    assert_eq!(
        first + second,
        1,
        "one slot, one job, whichever node ticked first"
    );
}

// ---- vault ------------------------------------------------------------------------------------

fn reference(name: &str) -> CredentialRef {
    CredentialRef::new(name).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_is_sealed_at_rest_scoped_to_its_site_and_rotatable() {
    let store = Arc::new(MemorySealedStore::new());
    let keyring = Arc::new(MemoryKeyring::new("k").unwrap());
    let vault = DurableVault::new(
        SITE,
        Arc::clone(&keyring),
        Arc::clone(&store) as Arc<dyn SealedStore>,
    );
    vault
        .put("crm_token", &SecretString::new("s3cr3t-canary-token"))
        .await
        .unwrap();

    // At rest the value is not there in the clear.
    let raw = store.raw(SITE, "crm_token").unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("s3cr3t-canary-token"));
    assert!(vault.contains("crm_token").await.unwrap());

    // The owning site reads it; another tenant does not.
    let ok = vault
        .resolve_for(SITE, &reference("crm_token"), 0)
        .await
        .unwrap();
    assert_eq!(ok.expose(), "s3cr3t-canary-token");
    assert!(
        vault
            .resolve_for("site_b", &reference("crm_token"), 0)
            .await
            .is_err()
    );

    // A second site with the same connector and reference holds its own value.
    let other = DurableVault::new(
        "site_b",
        Arc::clone(&keyring),
        Arc::clone(&store) as Arc<dyn SealedStore>,
    );
    assert!(
        other
            .resolve_for("site_b", &reference("crm_token"), 0)
            .await
            .is_err()
    );

    // Replacing the value takes effect at the next read; rotating the key re-seals it.
    vault
        .put("crm_token", &SecretString::new("rotated"))
        .await
        .unwrap();
    keyring.rotate().unwrap();
    assert_eq!(vault.reseal_all().await.unwrap(), 1);
    let after = vault
        .resolve_for(SITE, &reference("crm_token"), 0)
        .await
        .unwrap();
    assert_eq!(after.expose(), "rotated");

    // A blob copied to another reference fails to open (the associated data binds it).
    store.put(SITE, "other_ref", raw).await.unwrap();
    assert!(
        vault
            .resolve_for(SITE, &reference("other_ref"), 0)
            .await
            .is_err()
    );
}

// ---- egress -----------------------------------------------------------------------------------

fn request(url: &str) -> HttpRequest {
    HttpRequest {
        method: "GET".to_owned(),
        url: url.to_owned(),
        headers: Vec::new(),
        body: None,
    }
}

fn ctx(connector: &str) -> HttpContext {
    HttpContext {
        tenant: SITE.to_owned(),
        actor: Some(format!("connector:{connector}")),
        now_micros: 0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connector_with_an_empty_allow_list_cannot_connect() {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response(
        "api.example.com",
        vec![vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]],
    );
    let http = PerConnectorHttp::new(
        SITE,
        resolver,
        Arc::new(ManualClock::new(datetime!(2026-11-12 10:00 UTC))),
    );
    http.set_endpoints("closed", &[]);
    let error = http
        .send(request("https://api.example.com/"), &ctx("closed"))
        .await;
    assert_eq!(error.unwrap_err().code(), "connectors.egress_denied");
    // A connector nobody configured is closed too.
    let unknown = http
        .send(request("https://api.example.com/"), &ctx("ghost"))
        .await;
    assert!(unknown.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_and_metadata_addresses_are_refused_even_when_listed() {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response(
        "rebind.example.com",
        vec![vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))]],
    );
    resolver.add_response(
        "meta.example.com",
        vec![vec![IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))]],
    );
    let http = PerConnectorHttp::new(
        SITE,
        resolver,
        Arc::new(ManualClock::new(datetime!(2026-11-12 10:00 UTC))),
    );
    http.set_endpoints(
        "open",
        &[
            "rebind.example.com:443".to_owned(),
            "meta.example.com:80".to_owned(),
            "169.254.169.254:80".to_owned(),
        ],
    );
    for url in [
        "https://rebind.example.com/",
        "http://meta.example.com/",
        // A redirect target is judged as a request of its own.
        "http://169.254.169.254/latest/meta-data",
        "https://not-listed.example.com/",
    ] {
        let result = http.send(request(url), &ctx("open")).await;
        assert!(result.is_err(), "{url}");
    }
}
