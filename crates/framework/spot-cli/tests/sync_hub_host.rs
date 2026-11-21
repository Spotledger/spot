//! The sync hub service (WP-RTB-02.T4): its settings (off unless configured, every key bounded),
//! and its start-up checks (a taken port, a missing relay core, an unreadable certificate each
//! make the required service fail, which makes a hub site not ready).
//!
//! The end-to-end suite with a real `spot serve`, PostgreSQL and a TLS client (hello, pull,
//! push, revocation, two tenants, restart) needs the enrolment, relay and sink modules of
//! WP-RTB-02.T3/T1/T2/T8 to provide `HubParts`, and a certificate generator the host crate does
//! not depend on yet: see the task report. The behaviours of the join itself (limits, ownership of
//! queues, tenancy, acknowledgement by pull) are proven in `spot-sync-hub-host`'s own tests
//! against the real `spot-sync-http` router.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use spot_cli::services::Service;
use spot_cli::services::sync_hub::{HubParts, Settings, SyncHub, TlsFiles};
use spot_core::SystemClock;
use spot_sync_http::{NodeDirectory, StaticDirectory};
use spot_sync_hub_host::{RefusalAudit, TenantSinks};
use spot_sync_protocol::NodeId;

struct NoAudit;
impl RefusalAudit for NoAudit {
    fn refused(&self, _who: &str, _reason: &str) {}
}

fn tls(dir: &std::path::Path) -> TlsFiles {
    TlsFiles {
        cert: dir.join("hub.pem"),
        key: dir.join("hub.key"),
        client_ca: dir.join("ca.pem"),
    }
}

fn settings(listen: Option<&str>, dir: &std::path::Path) -> Settings {
    Settings {
        listen: listen.map(str::to_owned),
        tls: Some(tls(dir)),
        ..Settings::default()
    }
}

fn parts(with_core: bool) -> Arc<HubParts> {
    struct Core;
    impl spot_sync_hub_host::HubCore for Core {
        fn tick(&mut self, _now_ms: i64) -> Result<(), String> {
            Ok(())
        }
        fn window(
            &mut self,
            _dest: &str,
            _max: usize,
        ) -> Result<Vec<spot_sync_protocol::Record>, String> {
            Ok(Vec::new())
        }
        fn ack_served(&mut self, _dest: &str, _through: u64, _now_ms: i64) -> Result<(), String> {
            Ok(())
        }
        fn destinations(&self) -> Vec<String> {
            Vec::new()
        }
    }
    let directory: Arc<dyn NodeDirectory> = Arc::new(StaticDirectory::new());
    Arc::new(HubParts {
        directory,
        core: Mutex::new(if with_core {
            Some(Box::new(Core))
        } else {
            None
        }),
        sinks: Arc::new(TenantSinks::new()),
        hub_node: NodeId::new("hub.test#hub_main").unwrap(),
        metadata_epoch: 0,
        audit: Arc::new(NoAudit),
        seen: None,
    })
}

#[test]
fn a_hub_site_without_a_listener_or_certificates_is_refused_naming_the_key() {
    let dir = std::env::temp_dir();
    let error = settings(None, &dir).resolve().unwrap_err();
    assert!(error.0.contains("services.sync_hub.listen"), "{}", error.0);
    let mut no_tls = settings(Some("127.0.0.1:0"), &dir);
    no_tls.tls = None;
    assert!(
        no_tls
            .resolve()
            .unwrap_err()
            .0
            .contains("services.sync_hub.tls")
    );
    assert!(settings(Some("not-an-address"), &dir).resolve().is_err());
    assert!(settings(Some("127.0.0.1:7443"), &dir).resolve().is_ok());
}

#[test]
fn every_limit_has_a_bound_and_a_default() {
    let dir = std::env::temp_dir();
    let resolved = settings(Some("127.0.0.1:7443"), &dir).resolve().unwrap();
    assert_eq!(resolved.limits.per_node_connections, 2);
    assert_eq!(resolved.limits.push_per_second, 20);
    assert_eq!(resolved.limits.global_connections, 2_000);
    assert_eq!(resolved.server.max_push_bytes, 64 * 1024 * 1024);
    assert_eq!(resolved.server.long_poll.as_secs(), 25);
    for (key, apply) in [
        (
            "per_node_connections",
            (|s: &mut Settings| s.per_node_connections = Some(0)) as fn(&mut Settings),
        ),
        ("per_node_connections", |s| {
            s.per_node_connections = Some(17)
        }),
        ("push_per_second", |s| s.push_per_second = Some(0)),
        ("global_connections", |s| {
            s.global_connections = Some(100_001)
        }),
        ("max_body_mib", |s| s.max_body_mib = Some(257)),
        ("max_batch_records", |s| s.max_batch_records = Some(0)),
        ("long_poll_secs", |s| s.long_poll_secs = Some(61)),
        ("heartbeat_secs", |s| s.heartbeat_secs = Some(0)),
        ("audit_interval_secs", |s| s.audit_interval_secs = Some(9)),
        ("window", |s| s.window = Some(0)),
        ("drive_interval_ms", |s| s.drive_interval_ms = Some(19)),
    ] {
        let mut s = settings(Some("127.0.0.1:7443"), &dir);
        apply(&mut s);
        let error = s.resolve().unwrap_err();
        assert!(error.0.contains(key), "{key}: {}", error.0);
    }
}

#[test]
fn an_unknown_key_in_the_table_is_refused() {
    let parsed: Result<Settings, _> = toml::from_str("listen = \"127.0.0.1:1\"\nsurprise = 1\n");
    assert!(parsed.is_err());
}

fn hub(listen: &str, with_core: bool) -> SyncHub {
    let dir = std::env::temp_dir();
    SyncHub::new(
        settings(Some(listen), &dir).resolve().unwrap(),
        dir,
        "hub.test".to_owned(),
        parts(with_core),
        Arc::new(SystemClock::new()),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_port_makes_the_hub_not_ready() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = taken.local_addr().unwrap().to_string();
    let service = hub(&address, true);
    let error = service.check().await.unwrap_err();
    assert!(error.contains("cannot listen"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_relay_core_or_unreadable_certificate_makes_the_hub_not_ready() {
    let service = hub("127.0.0.1:0", false);
    let error = service.check().await.unwrap_err();
    assert!(error.contains("relay core"), "{error}");
    let service = hub("127.0.0.1:0", true);
    let error = service.check().await.unwrap_err();
    assert!(error.contains("certificate"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_service_is_required_and_named_sync_hub() {
    let service = hub("127.0.0.1:0", true);
    assert_eq!(service.name(), "sync_hub");
    assert_eq!(service.level(), spot_cli::services::Level::Required);
}
