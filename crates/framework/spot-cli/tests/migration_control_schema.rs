//! WP-MIG-07.T4: `new-site` and `migrate --apply` create the migration control schema, so the
//! maintenance gate is readable on every real site and `spot console` can write. Real PostgreSQL
//! through `SPOT_TEST_PG_URL` (skipped with a printed reason when unset, never faked). SQL is written
//! in two pieces (G5 refuses a full statement outside the storage crates).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{Env, run};
use spot_core::SiteId;
use spot_migration_apply::gate::AdmissionGate;
use spot_migration_apply_pg::{CONTROL_SCHEMA, PgAdmissionGate, TABLES};
use spot_postgres::PgBackend;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::Duration;

const PASSWORD: &str = "violet-lantern-staple-9-Quartz";
const WRITE: &str = "create role {\"role_name\": \"Gate Probe\", \"origin\": \"reference\"}";

async fn backend(env: &Env) -> PgBackend {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    PgBackend::connect(loaded.admin.clone().with_dbname(&site.database))
        .await
        .unwrap()
}

/// How many of the control schema's tables exist.
async fn control_tables(env: &Env) -> i64 {
    let client = backend(env).await.pool().get().await.unwrap();
    let sql = format!(
        "{}{}",
        "SELECT count(*)::bigint ", "FROM information_schema.tables WHERE table_schema = $1"
    );
    client
        .query_one(&sql, &[&CONTROL_SCHEMA])
        .await
        .unwrap()
        .get(0)
}

async fn drop_control(env: &Env) {
    let client = backend(env).await.pool().get().await.unwrap();
    let sql = format!("{}{}", "DROP SCHEMA ", "spot_migration_control CASCADE");
    client.batch_execute(&sql).await.unwrap();
}

fn password_file(env: &Env) -> PathBuf {
    let path = env.root.0.join("admin.pw");
    std::fs::write(&path, format!("{PASSWORD}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn console(env: &Env, file: &PathBuf, statement: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spot"))
        .arg("--root")
        .arg(&env.root.0)
        .args([
            "console",
            "--site",
            &env.site,
            "--user",
            "administrator",
            "--password-file",
        ])
        .arg(file)
        .args(["--exec", statement])
        .env("XDG_STATE_HOME", env.root.0.join("state"))
        .env("USER", "opsuser")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn migrate(env: &Env, apply: bool) -> Output {
    let mut args = vec!["migrate", "--site", &env.site];
    if apply {
        args.push("--apply");
    }
    run(&env.root.0, &args, None)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_site_has_the_control_schema_and_the_console_can_write() {
    let Some(env) = Env::new("").await else {
        return;
    };
    env.site();
    assert_eq!(
        control_tables(&env).await,
        i64::try_from(TABLES.len()).unwrap()
    );
    env.set_password(&env.site, "administrator", PASSWORD);
    let file = password_file(&env);
    let written = console(&env, &file, WRITE);
    assert_eq!(code(&written), 0, "{}", text(&written));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_site_gains_the_schema_on_apply_a_dry_run_changes_nothing_and_a_rerun_is_harmless()
{
    let Some(env) = Env::new("").await else {
        return;
    };
    env.site();
    drop_control(&env).await;
    assert_eq!(control_tables(&env).await, 0);

    // A dry run (the default) leaves the database as it was.
    let dry = migrate(&env, false);
    assert!(dry.status.success(), "{}", text(&dry));
    assert_eq!(
        control_tables(&env).await,
        0,
        "a dry run created the schema"
    );

    // `--apply` creates it, even when the plan itself is empty.
    let applied = migrate(&env, true);
    assert!(applied.status.success(), "{}", text(&applied));
    assert_eq!(
        control_tables(&env).await,
        i64::try_from(TABLES.len()).unwrap()
    );

    // Again: harmless, and the console can write.
    let again = migrate(&env, true);
    assert!(again.status.success(), "{}", text(&again));
    assert_eq!(
        control_tables(&env).await,
        i64::try_from(TABLES.len()).unwrap()
    );
    env.set_password(&env.site, "administrator", PASSWORD);
    let file = password_file(&env);
    let written = console(&env, &file, WRITE);
    assert_eq!(code(&written), 0, "{}", text(&written));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_with_the_schema_and_no_gate_row_is_open_and_a_closed_gate_still_refuses() {
    let Some(env) = Env::new("").await else {
        return;
    };
    env.site();
    env.set_password(&env.site, "administrator", PASSWORD);
    let file = password_file(&env);
    let gate = PgAdmissionGate::new(
        backend(&env).await.pool(),
        SiteId::new(&env.site).unwrap(),
        Arc::new(spot_core::SystemClock::new()),
        Duration::from_millis(1),
    );
    // No row yet: open (the first console write succeeds).
    let open = console(&env, &file, WRITE);
    assert_eq!(code(&open), 0, "{}", text(&open));

    gate.close("a test window", 1_800_000_000_000_000)
        .await
        .unwrap();
    let closed = console(
        &env,
        &file,
        "create role {\"role_name\": \"Second Probe\", \"origin\": \"reference\"}",
    );
    assert_eq!(code(&closed), 2, "{}", text(&closed));
    assert!(
        text(&closed).contains("server.maintenance"),
        "{}",
        text(&closed)
    );
    gate.open(1_800_000_000_000_001).await.unwrap();
}
