//! WP-PRD-01.T16: the identity audit trail is a hash chain, so an altered, removed or reordered
//! record is detected, and the host uses it as the production sink. Real PostgreSQL (skipped
//! without SPOT_TEST_PG_URL).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use spot_cli::audit::chain::{ChainAudit, IdentityChain, IdentityChainReady, scope_of};
use spot_cli::audit::{AuditRecord, CapturingAudit, FileAudit, IdentityAudit};
use spot_cli::{Options, Running};
use spot_http::health::ReadinessCheck;
use spot_login::LoginConfig;
use spot_postgres::{PgBackend, PgConfig};
use spot_sessions::SessionPolicy;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";
const WRONG: &str = "definitely-not-the-password-1";

fn record(n: u64, kind: &str) -> AuditRecord {
    AuditRecord {
        at: 1_800_000_000 + i64::try_from(n).unwrap(),
        site: "site".into(),
        kind: kind.into(),
        user: Some(format!("user-{n}")),
        attempted: Some(format!("name-{n}")),
        source: Some("ab12cd34".into()),
        reason: Some("bad_credentials".into()),
        session: None,
        count: Some(n),
        until: None,
        key_id: None,
        path: None,
    }
}

/// The site's database config, the way the host connects to it.
fn pg_of(env: &Env) -> PgConfig {
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    loaded.admin.clone().with_dbname(&site.database)
}

async fn backend_of(env: &Env) -> PgBackend {
    PgBackend::connect(pg_of(env)).await.unwrap()
}

async fn open(
    env: &Env,
    mirror: Option<Arc<dyn IdentityAudit>>,
) -> (Arc<ChainAudit>, IdentityChain) {
    let (sink, chain) = ChainAudit::open(pg_of(env), &env.site, mirror)
        .await
        .unwrap()
        .expect("the site's audit store is prepared by new-site");
    (Arc::new(sink), chain)
}

/// Runs a statement as the table owner with the documented maintenance switch on, the way only
/// a superuser (or a restore) can reach the chain.
async fn tamper(backend: &PgBackend, sql: &str) {
    let client = backend.pool().get().await.unwrap();
    client
        .batch_execute(&format!(
            "BEGIN; SET LOCAL spot.audit_maintenance = 'on'; {sql}; COMMIT"
        ))
        .await
        .unwrap();
}

async fn append_n(sink: &Arc<ChainAudit>, from: u64, count: u64) {
    for n in from..from + count {
        let sink = sink.clone();
        tokio::task::spawn_blocking(move || sink.append(&record(n, "sign_in_failed")))
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn n_identity_events_make_n_chained_entries_and_verify_passes() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let (sink, chain) = open(&env, None).await;
    assert_eq!(chain.scope(), scope_of(&env.site));
    assert_eq!(chain.head().await.unwrap(), 0);
    append_n(&sink, 1, 12).await;
    let report = chain.verify().await.unwrap();
    assert!(report.is_ok(), "{report:?}");
    assert_eq!((report.checked, report.head), (12, 12));
    // The entry body is the record's existing serialisation, nothing added.
    let backend = backend_of(&env).await;
    let client = backend.pool().get().await.unwrap();
    let row = client
        .query_one(
            "SELECT record FROM spot_audit_chain WHERE site = $1 AND seq = 3",
            &[&scope_of(&env.site)],
        )
        .await
        .unwrap();
    let stored: Json = serde_json::from_str(&row.get::<_, String>(0)).unwrap();
    assert_eq!(stored["kind"], "identity.sign_in_failed");
    assert_eq!(stored["actor"], "user:user-3");
    let body: Json =
        serde_json::from_str(stored["payload"]["record"]["Text"].as_str().unwrap()).unwrap();
    assert_eq!(
        body,
        serde_json::to_value(record(3, "sign_in_failed")).unwrap(),
        "the existing serialisation, nothing added"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_altered_removed_or_reordered_entry_fails_verification_naming_it() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let (sink, chain) = open(&env, None).await;
    append_n(&sink, 1, 6).await;
    let backend = backend_of(&env).await;
    let scope = scope_of(&env.site);

    // Without the maintenance switch even the table's owner is refused.
    let client = backend.pool().get().await.unwrap();
    assert!(
        client
            .batch_execute(&format!(
                "UPDATE spot_audit_chain SET record = '{{}}' WHERE site = '{scope}' AND seq = 2"
            ))
            .await
            .is_err()
    );
    assert!(chain.verify().await.unwrap().is_ok());

    // Altered body.
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET record = replace(record, 'name-4', 'name-X') \
             WHERE site = '{scope}' AND seq = 4"
        ),
    )
    .await;
    let report = chain.verify().await.unwrap();
    assert_eq!(
        report.failures,
        vec![(4, "audit.hash_mismatch")],
        "{report:?}"
    );
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET record = replace(record, 'name-X', 'name-4') \
             WHERE site = '{scope}' AND seq = 4"
        ),
    )
    .await;
    assert!(chain.verify().await.unwrap().is_ok());

    // Removed entry.
    tamper(
        &backend,
        &format!("DELETE FROM spot_audit_chain WHERE site = '{scope}' AND seq = 3"),
    )
    .await;
    let report = chain.verify().await.unwrap();
    assert!(!report.is_ok());
    assert_eq!(report.failures[0].0, 4, "{report:?}");
    assert!(
        report
            .failures
            .iter()
            .any(|(_, code)| *code == "audit.seq_gap")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_entries_swapped_in_place_are_detected() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let (sink, chain) = open(&env, None).await;
    append_n(&sink, 1, 5).await;
    let backend = backend_of(&env).await;
    let scope = scope_of(&env.site);
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET seq = -3 WHERE site = '{scope}' AND seq = 3; \
             UPDATE spot_audit_chain SET seq = 3 WHERE site = '{scope}' AND seq = 2; \
             UPDATE spot_audit_chain SET seq = 2 WHERE site = '{scope}' AND seq = -3"
        ),
    )
    .await;
    let report = chain.verify().await.unwrap();
    assert!(!report.is_ok(), "{report:?}");
    assert!(
        report.failures.iter().all(|(seq, _)| (2..=4).contains(seq)),
        "{report:?}"
    );
    assert!(
        report
            .failures
            .iter()
            .any(|(_, code)| *code == "audit.link_broken")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_sinks_appending_concurrently_keep_one_valid_chain_and_a_restart_resumes() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let (first, chain) = open(&env, None).await;
    let (second, _) = open(&env, None).await;
    let mut threads = Vec::new();
    for t in 0..6u64 {
        let sink = if t % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        threads.push(std::thread::spawn(move || {
            for n in 0..10 {
                sink.append(&record(t * 100 + n, "sign_in_failed")).unwrap();
            }
        }));
    }
    for thread in threads {
        thread.join().unwrap();
    }
    let report = chain.verify().await.unwrap();
    assert!(report.is_ok(), "{report:?}");
    assert_eq!((report.checked, report.head), (60, 60));

    // A restart: both sinks are dropped, a new one resumes from the stored head.
    drop((first, second));
    let (third, chain) = open(&env, None).await;
    append_n(&third, 1000, 3).await;
    let report = chain.verify().await.unwrap();
    assert!(report.is_ok(), "{report:?}");
    assert_eq!((report.checked, report.head), (63, 63));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_chain_append_is_an_error_naming_no_content_and_the_trail_resumes() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let (sink, chain) = open(&env, None).await;
    append_n(&sink, 1, 2).await;
    let backend = backend_of(&env).await;
    // The table goes away: every append now fails.
    backend
        .pool()
        .get()
        .await
        .unwrap()
        .batch_execute("ALTER TABLE spot_audit_chain RENAME TO spot_audit_chain_away")
        .await
        .unwrap();
    let mut secret = record(9, "sign_in_failed");
    secret.attempted = Some("must-not-be-echoed".into());
    let sink2 = sink.clone();
    let error = tokio::task::spawn_blocking(move || sink2.append(&secret))
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        !error.0.contains("must-not-be-echoed") && !error.0.contains("name-"),
        "{error}"
    );
    assert!(
        chain.verify().await.is_err(),
        "a verification that could not run is not a pass"
    );

    backend
        .pool()
        .get()
        .await
        .unwrap()
        .batch_execute("ALTER TABLE spot_audit_chain_away RENAME TO spot_audit_chain")
        .await
        .unwrap();
    // The trail resumes once the table is back.
    append_n(&sink, 3, 1).await;
    assert!(chain.verify().await.unwrap().is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_file_mirror_gets_the_same_records_and_its_failure_does_not_fail_the_append() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let path = env.root.join("mirror").join("identity.log");
    let mirror = Arc::new(FileAudit::open(&path).unwrap());
    let (sink, chain) = open(&env, Some(mirror)).await;
    append_n(&sink, 1, 3).await;
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(text.contains("\"attempted\":\"name-2\""));

    let failing = Arc::new(CapturingAudit::new());
    failing.fail_all(true);
    let (sink, chain2) = open(&env, Some(failing.clone() as Arc<dyn IdentityAudit>)).await;
    append_n(&sink, 4, 2).await;
    assert!(failing.records().is_empty());
    let report = chain.verify().await.unwrap();
    assert!(report.is_ok());
    assert_eq!(chain2.head().await.unwrap(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_reports_the_chain_and_warns_or_fails_on_a_break() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let (sink, chain) = open(&env, None).await;
    let warn = IdentityChainReady::new(&env.site, chain.clone(), false);
    let empty = warn.check().await;
    assert!(empty.ok, "{}", empty.detail);
    append_n(&sink, 1, 4).await;
    let good = warn.check().await;
    assert!(
        good.ok && good.detail.contains("verified through seq 4"),
        "{}",
        good.detail
    );
    let backend = backend_of(&env).await;
    let scope = scope_of(&env.site);
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET record = replace(record, 'name-2', 'name-Z') \
             WHERE site = '{scope}' AND seq = 2"
        ),
    )
    .await;
    // The scan resumed after seq 4 would not see seq 2, so a fresh check starts over.
    let fresh = IdentityChainReady::new(&env.site, chain.clone(), false);
    let warned = fresh.check().await;
    assert!(warned.ok, "warning only by default");
    assert!(
        warned.detail.starts_with("WARNING") && warned.detail.contains("seq 2"),
        "{}",
        warned.detail
    );
    let failed = IdentityChainReady::new(&env.site, chain, true)
        .check()
        .await;
    assert!(!failed.ok, "{}", failed.detail);
}

// ---- the host ------------------------------------------------------------------------------

struct Reply {
    status: u16,
    body: Json,
}

async fn http(addr: SocketAddr, host: &str, method: &str, path: &str, body: Option<Json>) -> Reply {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let raw = String::from_utf8(bytes).unwrap();
    let (headers, payload) = raw.split_once("\r\n\r\n").unwrap();
    Reply {
        status: headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        body: serde_json::from_str(payload).unwrap_or(Json::Null),
    }
}

async fn login(addr: SocketAddr, host: &str, user: &str, password: &str) -> Reply {
    http(
        addr,
        host,
        "POST",
        "/api/v1/auth/login",
        Some(json!({"user": user, "password": password})),
    )
    .await
}

fn options() -> Options {
    Options {
        clock: None,
        login: LoginConfig {
            session_policy: SessionPolicy::DEFAULT,
            cache_ttl: Duration::ZERO,
            ..LoginConfig::for_tests()
        },
    }
}

async fn serve(env: &Env) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = spot_cli::start_with_audit(&env.root, false, options(), None, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_writes_the_chain_with_the_file_beside_it_and_readiness_says_so() {
    let Some(env) = Env::new().await else { return };
    let secret = env
        .ok(&["new-site", &env.site])
        .trim()
        .rsplit(' ')
        .next()
        .unwrap()
        .to_owned();
    let common = env.root.join("common.toml");
    let text = std::fs::read_to_string(&common).unwrap();
    std::fs::write(
        &common,
        format!("{text}\n[audit]\nidentity_file_mirror = true\n"),
    )
    .unwrap();
    let (server, addr, stop) = serve(&env).await;
    assert_eq!(
        login(addr, &env.site, "administrator", WRONG).await.status,
        401
    );
    assert_eq!(
        login(addr, &env.site, "administrator", &secret)
            .await
            .status,
        200
    );

    let ready = http(addr, &env.site, "GET", "/api/health/ready", None).await;
    let rendered = ready.body.to_string();
    assert!(
        rendered.contains(&format!("identity-audit:{}", env.site))
            && rendered.contains("identity audit chain"),
        "{rendered}"
    );

    let backend = backend_of(&env).await;
    let chain = IdentityChain::new(&backend, &scope_of(&env.site));
    let report = chain.verify().await.unwrap();
    assert!(report.is_ok() && report.head >= 2, "{report:?}");
    let client = backend.pool().get().await.unwrap();
    let rows = client
        .query(
            "SELECT record FROM spot_audit_chain WHERE site = $1 ORDER BY seq",
            &[&scope_of(&env.site)],
        )
        .await
        .unwrap();
    let stored: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
    let joined = stored.join("\n");
    assert!(joined.contains("sign_in_failed") && joined.contains("sign_in_succeeded"));
    // Nothing secret reached the chain: not the password attempts, the one-time secret or its id.
    let key_id = secret.split('_').nth(1).unwrap();
    for needle in [secret.as_str(), WRONG, PASSWORD, key_id, "127.0.0.1"] {
        assert!(
            !joined.contains(needle),
            "`{needle}` reached the audit chain"
        );
    }
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();

    // The mirror holds the same records, mode 0600.
    let mirror = env
        .root
        .join("sites")
        .join(&env.site)
        .join("identity-audit.log");
    let file = std::fs::read_to_string(&mirror).unwrap();
    assert_eq!(file.lines().count(), stored.len(), "{file}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_whose_chain_append_fails_is_refused_and_a_failed_one_reads_the_same() {
    let Some(env) = Env::new().await else { return };
    let secret = env
        .ok(&["new-site", &env.site])
        .trim()
        .rsplit(' ')
        .next()
        .unwrap()
        .to_owned();
    let (server, addr, stop) = serve(&env).await;
    let before = login(addr, &env.site, "administrator", WRONG).await;
    assert_eq!(before.status, 401);
    let backend = backend_of(&env).await;
    backend
        .pool()
        .get()
        .await
        .unwrap()
        .batch_execute("ALTER TABLE spot_audit_chain RENAME TO spot_audit_chain_away")
        .await
        .unwrap();
    // The audit outage is invisible to a caller who fails to sign in ...
    let blind = login(addr, &env.site, "administrator", WRONG).await;
    assert_eq!(blind.status, before.status);
    assert_eq!(blind.body["error"]["code"], before.body["error"]["code"]);
    // ... and a correct credential is not accepted without an audit record.
    let refused = login(addr, &env.site, "administrator", &secret).await;
    assert_ne!(refused.status, 200);
    assert!(refused.body["data"]["token"].is_null());
    stop.send(()).unwrap();
    server.handle.join().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migrate_brings_back_an_audit_store_that_is_missing() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let backend = backend_of(&env).await;
    let client = backend.pool().get().await.unwrap();
    client
        .batch_execute("DROP TABLE spot_audit_chain; DROP TABLE spot_audit_checkpoint")
        .await
        .unwrap();
    // `spot migrate --apply` plans the two tables again and installs the guard.
    let output = env.spot(&["migrate", "--site", &env.site, "--apply"]);
    assert!(output.status.success(), "{}", support::text(&output));
    let protection = spot_audit_store::verify(&backend.pool()).await.unwrap();
    assert!(protection.is_complete(), "{:?}", protection.missing());
}
