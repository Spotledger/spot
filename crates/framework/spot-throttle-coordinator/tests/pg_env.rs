//! The one gate every PostgreSQL test of this crate goes through.
//!
//! `SPOT_TEST_PG_URL` names a server the tests may use; `SPOT_REQUIRE_PG=1` turns a missing
//! server into a failure instead of a skip, so a CI run can never pass by skipping the
//! PostgreSQL tests. This file is the only place in the crate's tests that reads either
//! variable, and no message here carries the URL: only the variable names.
//!
//! Each test then gets its own scratch database (`TestDb`), so parallel tests and repeated runs
//! on one server never share a table; [`key_prefix`] gives each one a bucket-key prefix of its
//! own on top of that.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;

/// The variable naming the test server (its admin `postgres://` URL).
pub const URL_VAR: &str = "SPOT_TEST_PG_URL";
/// The variable that makes a missing test server a failure instead of a skip.
pub const REQUIRE_VAR: &str = "SPOT_REQUIRE_PG";
/// Set by the proof below to make `gate_probe` do one thing in a child process.
const PROBE_VAR: &str = "SPOT_PG_ENV_PROBE";

/// Whether a missing server is a failure here (CI) or a skip (a plain developer run).
fn required() -> bool {
    std::env::var(REQUIRE_VAR).as_deref() == Ok("1")
}

/// The test server's URL, or `None` after printing one `SKIPPED:` line.
///
/// A `SPOT_TEST_PG_URL` that is set but blank counts as unset: an empty value names no server,
/// and treating it as one would skip quietly in exactly the run the variable exists for.
///
/// # Panics
///
/// When `SPOT_REQUIRE_PG=1` and no URL is set — the message names both variables, so a CI log
/// says what to set — and when a URL is set that is not a URL this platform can connect with.
/// Neither message repeats the URL.
pub fn database() -> Option<String> {
    let url = std::env::var(URL_VAR)
        .ok()
        .filter(|url| !url.trim().is_empty());
    let Some(url) = url else {
        if required() {
            panic!(
                "{REQUIRE_VAR}=1 but {URL_VAR} is unset: this run must reach the PostgreSQL \
                 tests of spot-throttle-coordinator. Set {URL_VAR} to the test server's admin \
                 URL, or unset {REQUIRE_VAR} to skip them."
            );
        }
        println!(
            "SKIPPED: {URL_VAR} is unset; the PostgreSQL tests of spot-throttle-coordinator did not run"
        );
        return None;
    };
    // Parsed, not connected: the URL is valid before a test spends a connection on it, and a
    // malformed one fails here rather than as a confusing pool error later. `PgConfig::from_url`
    // reports what is wrong with the URL without repeating it.
    if let Err(error) = PgConfig::from_url(&url) {
        panic!("{URL_VAR} is set but is not a usable PostgreSQL URL: {error}");
    }
    Some(url)
}

/// A fresh scratch database on the test server, or `None` (the `SKIPPED:` line was printed).
///
/// `TestDb` creates `spot_test_<uuidv7>`, drops it when the test ends and sweeps databases an
/// earlier run left behind, so repeated and parallel runs on one server never collide.
pub async fn test_db() -> Option<TestDb> {
    database()?;
    // `TestDb` cannot be handed a URL: it re-reads `SPOT_TEST_PG_URL`, which the gate above has
    // just proved is set and parseable. A `None` here would be a change in that harness, not a
    // missing server, so it is loud.
    let db = TestDb::create().await;
    assert!(
        db.is_some(),
        "{URL_VAR} is set and usable, so the harness must have created the scratch database"
    );
    db
}

/// A bucket-key prefix unique to this scratch database.
///
/// The database name ends in a UUIDv7, whose tail differs between databases created in the same
/// millisecond (its leading bits are a timestamp), so two tests — in parallel, or in a later run
/// against the same server — key different buckets even before the database itself separates
/// them.
pub fn key_prefix(db: &TestDb) -> String {
    let name = db.name().trim_start_matches("spot_test_");
    let start = name.len().saturating_sub(12);
    name.chars().skip(start).collect()
}

// ---------------------------------------------------------------- the gate, proven

/// The probe the proof below runs in a child process: nothing unless `SPOT_PG_ENV_PROBE` is set,
/// because an environment variable set in this process would race every other test in it.
#[test]
fn gate_probe() {
    let Ok(mode) = std::env::var(PROBE_VAR) else {
        return;
    };
    match mode.as_str() {
        "url" => {
            assert!(
                database().is_some(),
                "a set {URL_VAR} must be returned, not skipped"
            );
            println!("PROBE: url");
        }
        "skip" => {
            assert!(
                database().is_none(),
                "an unset {URL_VAR} must skip, not fail"
            );
            println!("PROBE: skip");
        }
        "require" => {
            // Must not return: `SPOT_REQUIRE_PG=1` with no URL is the failure CI relies on.
            let _ = database();
            println!("PROBE: require-returned");
        }
        other => panic!("unknown {PROBE_VAR}={other}"),
    }
}

/// The probe's libtest name in *this* binary: `gate_probe` at the root of the file's own test
/// binary, `pg_env::gate_probe` when the file is a module of another test binary.
fn probe_name() -> String {
    match module_path!().split_once("::") {
        // A test binary's `module_path!()` starts with the crate's name, which libtest leaves
        // out of a test's name.
        Some((_crate, rest)) => format!("{rest}::gate_probe"),
        None => "gate_probe".to_owned(),
    }
}

/// The gate, proven from the outside: three runs of this very test binary as child processes,
/// each with the environment the parent chooses. Well under a second, and no server needed.
#[test]
fn the_gate_runs_skips_and_fails_closed() {
    // A URL that parses and is never connected to: this proof is about the gate, not the server.
    const URL: &str = "postgres://spot:spot@127.0.0.1:1/postgres";
    let exe = std::env::current_exe().expect("the test binary's own path");
    let probe = probe_name();
    let run = |mode: &str, url: Option<&str>, require: Option<&str>| {
        let mut child = std::process::Command::new(&exe);
        child
            .args([probe.as_str(), "--exact", "--nocapture"])
            .env(PROBE_VAR, mode);
        match url {
            Some(url) => child.env(URL_VAR, url),
            None => child.env_remove(URL_VAR),
        };
        match require {
            Some(require) => child.env(REQUIRE_VAR, require),
            None => child.env_remove(REQUIRE_VAR),
        };
        child.output().expect("the child test binary runs")
    };

    // A server and `SPOT_REQUIRE_PG=1`: the URL is returned and the test runs.
    let output = run("url", Some(URL), Some("1"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a set URL must run: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("PROBE: url"), "{stdout}");

    // No server, no requirement: one `SKIPPED:` line and `None`.
    let output = run("skip", None, None);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "an unset URL must skip: {stdout}");
    assert!(stdout.contains("PROBE: skip"), "{stdout}");
    assert_eq!(
        stdout.matches("SKIPPED:").count(),
        1,
        "one line, saying the PostgreSQL tests did not run: {stdout}"
    );

    // No server and `SPOT_REQUIRE_PG=1`: a panic that names both variables.
    let output = run("require", None, Some("1"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "SPOT_REQUIRE_PG=1 without a URL must fail, not pass: {stdout}"
    );
    assert!(
        !stdout.contains("PROBE: require-returned"),
        "the gate returned instead of panicking: {stdout}"
    );
    assert!(
        stderr.contains(URL_VAR) && stderr.contains(REQUIRE_VAR),
        "the failure must name both variables: {stderr}"
    );
}
