//! WP-PRD-01.T50: `spot audit verify` runs `IdentityChain::verify` for an operator. Real
//! PostgreSQL (skipped without SPOT_TEST_PG_URL); the command runs as a process.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use spot_cli::audit::chain::{ChainAudit, IdentityChain, scope_of};
use spot_cli::audit::{AuditRecord, IdentityAudit};
use spot_postgres::{PgBackend, PgConfig};
use std::process::Output;
use std::sync::Arc;
use support::{Env, text};

fn record(n: u64) -> AuditRecord {
    AuditRecord {
        at: 1_800_000_000 + i64::try_from(n).unwrap(),
        site: "site".into(),
        kind: "sign_in_failed".into(),
        user: Some(format!("zed-user-{n}")),
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

fn pg_of(env: &Env) -> PgConfig {
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    loaded.admin.clone().with_dbname(&site.database)
}

async fn backend_of(env: &Env) -> PgBackend {
    PgBackend::connect(pg_of(env)).await.unwrap()
}

async fn append_n(env: &Env, count: u64) -> IdentityChain {
    let (sink, chain) = ChainAudit::open(pg_of(env), &env.site, None)
        .await
        .unwrap()
        .expect("the site's audit store is prepared by new-site");
    let sink = Arc::new(sink);
    for n in 1..=count {
        let sink = sink.clone();
        tokio::task::spawn_blocking(move || sink.append(&record(n)))
            .await
            .unwrap()
            .unwrap();
    }
    chain
}

async fn tamper(backend: &PgBackend, sql: &str) {
    let client = backend.pool().get().await.unwrap();
    client
        .batch_execute(&format!(
            "BEGIN; SET LOCAL spot.audit_maintenance = 'on'; {sql}; COMMIT"
        ))
        .await
        .unwrap();
}

/// Runs `spot audit verify` and checks nothing sensitive reached the output.
fn verify(env: &Env, extra: &[&str]) -> (i32, String) {
    let mut args = vec!["audit", "verify", "--site", env.site.as_str()];
    args.extend_from_slice(extra);
    let output: Output = env.spot(&args);
    let out = text(&output);
    for secret in ["zed-user", "name-", "ab12cd34", "bad_credentials", "@"] {
        assert!(!out.contains(secret), "output leaked `{secret}`: {out}");
    }
    (output.status.code().unwrap(), out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_intact_chain_verifies_and_the_summary_names_the_count_and_head() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let (code, out) = verify(&env, &[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("identity chain: 0 entries checked, head 0: ok"),
        "{out}"
    );
    append_n(&env, 7).await;
    let (code, out) = verify(&env, &[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("identity chain: 7 entries checked, head 7: ok"),
        "{out}"
    );
    // A later range verifies, anchored on the entry before it.
    let (code, out) = verify(&env, &["--from", "4"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("4 entries checked, head 7: ok"), "{out}");
    // A range past the head checked nothing: it could not verify, which is not a pass.
    let (code, out) = verify(&env, &["--from", "8"]);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("--from is past the chain head"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_altered_removed_or_swapped_entry_exits_2_naming_the_sequence() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    append_n(&env, 6).await;
    let backend = backend_of(&env).await;
    let scope = scope_of(&env.site);

    // Altered body.
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET record = replace(record, 'name-4', 'name-X') \
             WHERE site = '{scope}' AND seq = 4"
        ),
    )
    .await;
    let (code, out) = verify(&env, &[]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("identity chain: BROKEN"), "{out}");
    assert!(out.contains("seq 4: audit.hash_mismatch"), "{out}");
    assert!(out.contains("first broken sequence 4"), "{out}");
    assert!(!out.contains("name-X"), "{out}");
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET record = replace(record, 'name-X', 'name-4') \
             WHERE site = '{scope}' AND seq = 4"
        ),
    )
    .await;
    assert_eq!(verify(&env, &[]).0, 0);

    // Swapped entries.
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET seq = -3 WHERE site = '{scope}' AND seq = 3; \
             UPDATE spot_audit_chain SET seq = 3 WHERE site = '{scope}' AND seq = 2; \
             UPDATE spot_audit_chain SET seq = 2 WHERE site = '{scope}' AND seq = -3"
        ),
    )
    .await;
    let (code, out) = verify(&env, &[]);
    assert_eq!(code, 2, "{out}");
    assert!(
        out.contains("first broken sequence 2") || out.contains("first broken sequence 3"),
        "{out}"
    );
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET seq = -3 WHERE site = '{scope}' AND seq = 3; \
             UPDATE spot_audit_chain SET seq = 3 WHERE site = '{scope}' AND seq = 2; \
             UPDATE spot_audit_chain SET seq = 2 WHERE site = '{scope}' AND seq = -3"
        ),
    )
    .await;
    assert_eq!(verify(&env, &[]).0, 0);

    // Removed middle entry.
    tamper(
        &backend,
        &format!("DELETE FROM spot_audit_chain WHERE site = '{scope}' AND seq = 3"),
    )
    .await;
    let (code, out) = verify(&env, &[]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("audit.seq_gap"), "{out}");
    assert!(out.contains("first broken sequence 4"), "{out}");
    // A range after the break does not see it.
    let (code, out) = verify(&env, &["--from", "5"]);
    assert_eq!(code, 0, "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usage_errors_exit_1_and_a_missing_site_or_store_exits_3() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    for bad in ["0", "-1", "x"] {
        let (code, out) = verify(&env, &["--from", bad]);
        assert_eq!(code, 1, "--from {bad}: {out}");
    }
    let output = env.spot(&["audit", "verify", "--site", "no-such-site"]);
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert!(text(&output).contains("not verified"));

    // An unprepared audit store: drop it as the owner with the maintenance switch on.
    let backend = backend_of(&env).await;
    let client = backend.pool().get().await.unwrap();
    client
        .batch_execute(
            "BEGIN; SET LOCAL spot.audit_maintenance = 'on'; DROP TABLE spot_audit_chain; COMMIT",
        )
        .await
        .unwrap();
    let (code, out) = verify(&env, &[]);
    assert_eq!(code, 3, "{out}");
    assert!(out.contains("not verified"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_is_listed_in_help_and_a_missing_site_is_a_usage_error() {
    let Some(env) = Env::new().await else { return };
    let help = text(&env.spot(&["--help"]));
    assert!(help.contains("audit"), "{help}");
    let output = env.spot(&["audit", "verify"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}
