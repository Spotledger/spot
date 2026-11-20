//! WP-STO-12.T4: `spot extension list|plan|install` hosts the audited `extension.install`
//! operation. Real PostgreSQL (skipped without SPOT_TEST_PG_URL, a failure under
//! SPOT_REQUIRE_PG=1 through the shared `Env`); the command runs as a process.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use spot_postgres::{PgBackend, PgConfig};
use support::{Env, text};

fn pg_of(env: &Env) -> PgConfig {
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    loaded.admin.clone().with_dbname(&site.database)
}

/// The extensions created in the site's database, by name.
async fn present(env: &Env) -> Vec<String> {
    let backend = PgBackend::connect(pg_of(env)).await.unwrap();
    let client = backend.pool().get().await.unwrap();
    client
        .query("SELECT extname FROM pg_extension ORDER BY extname", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect()
}

/// Runs the command and checks that nothing secret reached the output.
fn run(env: &Env, args: &[&str]) -> (i32, String) {
    let output = env.spot(args);
    let out = text(&output);
    let password = env
        .url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('@').next())
        .and_then(|creds| creds.split_once(':'))
        .map(|(_, p)| p.to_owned())
        .unwrap_or_default();
    assert!(!out.contains(&env.url), "the URL reached the output: {out}");
    if !password.is_empty() {
        assert!(!out.contains(&password), "the password reached the output");
    }
    (output.status.code().unwrap(), out)
}

fn hash_of(out: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix("plan_hash: "))
        .expect("a plan_hash line")
        .trim()
        .to_owned()
}

const EXT: &str = "btree_gist";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_shows_what_is_installed_and_what_is_not() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let (code, out) = run(&env, &["extension", "list", "--site", &env.site]);
    assert_eq!(code, 0, "{out}");
    for name in ["postgis", "btree_gist", "pg_trgm", "unaccent"] {
        assert!(out.contains(name), "{out}");
    }
    let line = out.lines().find(|l| l.starts_with(EXT)).unwrap();
    if present(&env).await.iter().any(|e| e == EXT) {
        assert!(!line.contains("installed: -"), "{line}");
    } else {
        assert!(line.contains("installed: -"), "{line}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_prints_a_hash_and_changes_nothing() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let before = present(&env).await;
    let (code, out) = run(
        &env,
        &["extension", "plan", "--site", &env.site, "--name", EXT],
    );
    assert_eq!(code, 0, "{out}");
    assert!(!hash_of(&out).is_empty());
    assert_eq!(present(&env).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_with_the_hash_creates_it_and_a_second_run_is_a_no_op() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let site = env.site.clone();
    let (_, out) = run(&env, &["extension", "plan", "--site", &site, "--name", EXT]);
    let hash = hash_of(&out);
    let args = [
        "extension",
        "install",
        "--site",
        &site,
        "--name",
        EXT,
        "--plan-hash",
        &hash,
    ];
    let (code, out) = run(&env, &args);
    assert_eq!(code, 0, "{out}");
    assert!(present(&env).await.iter().any(|e| e == EXT));
    // The same hash again: nothing to do, still exit 0 (the library re-decides under its lock).
    let (code, out) = run(&env, &args);
    assert!(code == 0 || out.contains(codes_stale()), "{out}");
    // The audit chain still verifies.
    let (code, out) = run(&env, &["audit", "verify", "--site", &site]);
    assert_eq!(code, 0, "{out}");
}

fn codes_stale() -> &'static str {
    spot_pg_migrate::codes::EXTENSION_PLAN_STALE
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hash_from_before_another_install_is_stale_and_exits_2() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let site = env.site.clone();
    let (_, out) = run(&env, &["extension", "plan", "--site", &site, "--name", EXT]);
    let stale = hash_of(&out);
    // Another install changes what the database holds.
    let backend = PgBackend::connect(pg_of(&env)).await.unwrap();
    backend
        .pool()
        .get()
        .await
        .unwrap()
        .batch_execute(&format!("CREATE EXTENSION IF NOT EXISTS {EXT}"))
        .await
        .unwrap();
    let (code, out) = run(
        &env,
        &[
            "extension",
            "install",
            "--site",
            &site,
            "--name",
            EXT,
            "--plan-hash",
            &stale,
        ],
    );
    assert_eq!(code, 2, "{out}");
    assert!(out.contains(codes_stale()), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_name_off_the_allow_list_is_refused_before_any_connection() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    // Point the host at a database that cannot be reached: the refusal must not need it.
    let common = std::fs::read_to_string(env.root.join("common.toml")).unwrap();
    std::fs::write(
        env.root.join("common.toml"),
        common.replace(&env.url, "postgres://nobody:none@127.0.0.1:1/none"),
    )
    .unwrap();
    let (code, out) = run(
        &env,
        &["extension", "plan", "--site", &env.site, "--name", "hstore"],
    );
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("migrate.extension_not_allowed"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_without_a_plan_hash_is_a_usage_error_naming_plan() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let output = env.spot(&["extension", "install", "--site", &env.site, "--name", EXT]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("spot extension plan"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_site_is_a_usage_error() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let output = env.spot(&["extension", "list", "--site", "no-such-site"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}
