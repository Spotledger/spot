//! The hosted management API: settings and defaults (no database needed), and the durable pieces
//! over a real PostgreSQL server (`TestDb`; skipped with a printed reason when `SPOT_TEST_PG_URL` is
//! unset, never faked): replay refused after a restart, the command store round trip.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use deadpool_postgres::{Pool, PoolConfig, Runtime};
use serde_json::{Value, json};
use spot_cli::services::SiteCtx;
use spot_cli::services::mgmt_api::{MgmtHandlers, ORDER, PgCommandStore, Settings, register};
use spot_core::{Clock, ManualClock, SystemClock};
use spot_mgmt_api::lifecycle::{CommandRecord, CommandState, CommandStore, StoreError};
use spot_mgmt_api::ports::{MemoryAuditSink, NoApprovals, OperatorAllowList, OperatorEntry};
use spot_mgmt_api::stubs::{stub_catalogue, stub_handlers};
use spot_mgmt_api::tls::PeerCertificate;
use spot_mgmt_api::{Deps, MAX_CLOCK_SKEW_SECS, router, wire};
use spot_postgres::testing::TestDb;
use spot_signed_command::{Command, NonceStore, Signer, SoftwareSigner, sign};
use spot_signed_command_pg::PgNonceStore;
use tokio_postgres::NoTls;
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;

fn ctx(table: Option<&str>, assembling: bool) -> SiteCtx<'static> {
    let mut settings = BTreeMap::new();
    if let Some(text) = table {
        settings.insert("mgmt_api".to_owned(), text.parse::<toml::Value>().unwrap());
    }
    SiteCtx::new(
        "site",
        Duration::from_secs(1),
        Arc::new(SystemClock::new()),
        settings,
        assembling,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn it_is_off_by_default_the_port_stays_closed_and_the_aggregator_is_provided() {
    let mut c = ctx(None, true);
    register(&mut c).await.unwrap();
    // The aggregator exists so other modules may add operations whether or not the API is on.
    assert!(c.port::<MgmtHandlers>().is_some());
    let defaults = Settings::default();
    assert!(!defaults.enabled);
    assert_eq!(defaults.listen, "127.0.0.1:9443");
    assert_eq!(defaults.store, "postgres");
    assert_eq!(ORDER, 1000);
}

fn enabled(extra: &str) -> Settings {
    let text = format!(
        "enabled = true\nserver_cert = \"/c.pem\"\nserver_key = \"/k.pem\"\noperator_ca = \"/ca.pem\"\n\
         administrator_keys = \"/keys\"\noperators = [\"CN=alice\"]\n{extra}"
    );
    toml::from_str(&text).unwrap()
}

#[test]
fn a_non_loopback_address_needs_allow_remote() {
    assert!(enabled("").validate().is_ok());
    let refused = enabled("listen = \"0.0.0.0:9443\"").validate().unwrap_err();
    assert!(refused.contains("allow_remote"), "{refused}");
    assert!(
        enabled("listen = \"0.0.0.0:9443\"\nallow_remote = true")
            .validate()
            .is_ok()
    );
    assert!(enabled("listen = \"not an address\"").validate().is_err());
}

#[test]
fn the_memory_store_is_refused_outside_the_development_profile() {
    let refused = enabled("store = \"memory\"").validate().unwrap_err();
    assert!(refused.contains("development"), "{refused}");
    assert!(enabled("store = \"memory\"\ndev = true").validate().is_ok());
    assert!(enabled("store = \"redis\"").validate().is_err());
}

#[test]
fn an_enabled_service_without_its_files_or_operators_is_refused() {
    let mut missing = enabled("");
    missing.server_key = None;
    assert!(missing.validate().unwrap_err().contains("server_key"));
    let mut none = enabled("");
    none.operators.clear();
    assert!(none.validate().unwrap_err().contains("operators"));
    assert!(toml::from_str::<Settings>("enabled = true\nsurprise = 1").is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_with_bad_settings_stops_the_site_naming_the_setting() {
    let mut c = ctx(Some("enabled = true\nlisten = \"0.0.0.0:9443\""), true);
    let error = register(&mut c).await.unwrap_err().to_string();
    assert!(error.contains("services.mgmt_api"), "{error}");
}

// ---- PostgreSQL ------------------------------------------------------------------------------

fn pool_of(db: &TestDb) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.pool = Some(PoolConfig::new(4));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

fn record(id: &str) -> CommandRecord {
    CommandRecord {
        id: id.to_owned(),
        operation: "site.status".to_owned(),
        principal: "alice".to_owned(),
        state: CommandState::Accepted,
        progress: Vec::new(),
        result: None,
        error_code: None,
        audit_ref: String::new(),
        created_at: NOW,
        finished_at: None,
        idempotency_key: format!("key-{id}"),
        body_digest: "d".to_owned(),
        cancellable: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_store_round_trips_and_refuses_duplicates() {
    let Some(db) = TestDb::create().await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let store = PgCommandStore::new(pool_of(&db));
    store.prepare().await.unwrap();
    store.insert(record("a"), NOW).await.unwrap();
    assert_eq!(
        store.insert(record("a"), NOW).await,
        Err(StoreError::Duplicate)
    );
    store.start("a", "audit-1").await.unwrap();
    let running = store.get("a").await.unwrap().unwrap();
    assert_eq!(
        (running.state, running.audit_ref.as_str()),
        (CommandState::Running, "audit-1")
    );
    store
        .progress(
            "a",
            spot_mgmt_api::lifecycle::ProgressLine {
                step: "copy".into(),
                percent: 40,
                at: NOW,
            },
        )
        .await
        .unwrap();
    store
        .finish(
            "a",
            CommandState::Succeeded,
            Some(json!({ "ok": true })),
            None,
            NOW + 1,
        )
        .await
        .unwrap();
    // A finished command does not move again.
    store
        .finish(
            "a",
            CommandState::Failed,
            None,
            Some("mgmt.internal".into()),
            NOW + 2,
        )
        .await
        .unwrap();
    let done = store.get("a").await.unwrap().unwrap();
    assert_eq!(done.state, CommandState::Succeeded);
    assert_eq!(done.result, Some(json!({ "ok": true })));
    assert_eq!(done.progress.len(), 1);
    assert!(
        store
            .find_by_idempotency("alice", "key-a")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .find_by_idempotency("bob", "key-a")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(store.purge(NOW + 10).await.unwrap(), 1);
}

/// The host-level restart test: a signed command is accepted, the host is rebuilt on the same
/// database (a new process, new in-memory state), and the same command is refused as a replay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replay_is_refused_after_the_host_is_rebuilt_on_the_same_database() {
    let Some(db) = TestDb::create().await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let signer = SoftwareSigner::from_seed(&[5u8; 32]).unwrap();
    let command = Command::new(
        "site.backup.run",
        json!({ "site": "site-1", "target": "local" }),
        "site-1",
        signer.key_id(),
        NOW - 5,
        NOW + 300,
    );
    let body = wire::encode(&sign(&command, &signer).unwrap(), &[]).unwrap();

    let build = || {
        let pool = pool_of(&db);
        let nonces = Arc::new(PgNonceStore::with_skew(pool.clone(), MAX_CLOCK_SKEW_SECS));
        let commands = Arc::new(PgCommandStore::new(pool));
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(
            time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
        ));
        (nonces, commands, clock)
    };
    let post = |app: axum::Router, key: &'static str, body: String| async move {
        let request = Request::builder()
            .method("POST")
            .uri("/mgmt/v1/commands")
            .header("idempotency-key", key)
            .extension(PeerCertificate::new("CN=alice", "fp"))
            .body(Body::from(body))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
        )
    };
    let deps =
        |nonces: Arc<PgNonceStore>, commands: Arc<PgCommandStore>, clock: Arc<dyn Clock>| Deps {
            catalogue: Arc::new(stub_catalogue().unwrap()),
            handlers: stub_handlers(),
            nonces: nonces as Arc<dyn NonceStore>,
            administrator_keys: vec![signer.public_key_bytes()],
            operator_ca: Vec::new(),
            operators: OperatorAllowList::new(vec![OperatorEntry::new("CN=alice", "alice")]),
            approvals: Arc::new(NoApprovals),
            clock,
            audit: Arc::new(MemoryAuditSink::new()),
            commands,
            site_id: "site-1".to_owned(),
        };

    let (nonces, commands, clock) = build();
    nonces.prepare().await.unwrap();
    commands.prepare().await.unwrap();
    let (status, _) = post(router(deps(nonces, commands, clock)), "one", body.clone()).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    // "Restart": everything in memory is rebuilt; only the database remains.
    let (nonces, commands, clock) = build();
    let (status, answer) = post(router(deps(nonces, commands, clock)), "two", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{answer}");
    assert_eq!(answer["error"]["code"], "mgmt.replayed");
}
