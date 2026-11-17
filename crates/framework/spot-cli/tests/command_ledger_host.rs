//! WP-PLT-05.T9: the host reads `[services.idempotency.ledger.retention]` (a default, a maximum
//! and a duration per caller class), hands the policy to the document engine of each site, and
//! prunes the command ledger on a schedule from the host's own clock.
//!
//! Real PostgreSQL (`SPOT_TEST_PG_URL`); `SPOT_REQUIRE_PG=1` turns a missing server into a
//! failure instead of a skip. The writes go through the running site's own engine under two
//! entry-point classes (`for_class`), because the router's class (`http_api`) is handed over by
//! `GuardedEngine`, which this task does not own (see the report). The `[services.idempotency]`
//! table must be a registered `[services.*]` table for these configurations to load (the
//! report's open questions).
//!
//! Statements are written in two pieces (G5 refuses a full SQL statement outside the storage
//! crates), as `participants_pg.rs` does.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::Document;
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Value};
use spot_cli::Options;
use spot_core::{Clock, ManualClock};
use spot_postgres::PgBackend;

const DAY: u64 = 24 * 60 * 60;

fn skip_note() {
    assert!(
        std::env::var_os("SPOT_REQUIRE_PG").is_none(),
        "SPOT_REQUIRE_PG is set but no PostgreSQL is configured"
    );
    eprintln!("SKIPPED: SPOT_TEST_PG_URL is not set");
}

/// A `[services.idempotency.ledger.retention]` section.
fn section(default: &str, max: &str, classes: &[(&str, &str)]) -> String {
    let mut text =
        format!("[services.idempotency.ledger.retention]\ndefault = {default:?}\nmax = {max:?}\n");
    if !classes.is_empty() {
        text.push_str("[services.idempotency.ledger.retention.classes]\n");
        for (name, keep) in classes {
            text.push_str(&format!("{name} = {keep:?}\n"));
        }
    }
    text
}

fn manual_clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()))
}

fn options(clock: &Arc<ManualClock>) -> Options {
    Options {
        clock: Some(clock.clone() as Arc<dyn Clock>),
        ..fast_options()
    }
}

async fn backend(env: &Env) -> PgBackend {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|site| site.site == env.site)
        .unwrap()
        .clone();
    spot_cli::schema::backend(&loaded, &site).await.unwrap()
}

/// Rows of the ledger, read on its own connection.
async fn rows(env: &Env) -> i64 {
    let backend = backend(env).await;
    let client = backend.pool().get().await.unwrap();
    let (head, tail) = ("SELECT count(*)::bigint ", "FROM spot_command_ledger");
    let sql = format!("{head}{tail}");
    client.query_one(&sql, &[]).await.unwrap().get(0)
}

/// Seconds a row is kept: its own `retain_until` minus its `recorded_at`.
async fn kept_seconds(env: &Env, command: &str) -> i64 {
    let backend = backend(env).await;
    let client = backend.pool().get().await.unwrap();
    let head = "SELECT extract(epoch FROM (retain_until - recorded_at))::bigint ";
    let tail = "FROM spot_command_ledger WHERE command_id = $1";
    let sql = format!("{head}{tail}");
    client.query_one(&sql, &[&command]).await.unwrap().get(0)
}

async fn has_row(env: &Env, command: &str) -> bool {
    let backend = backend(env).await;
    let client = backend.pool().get().await.unwrap();
    let (head, tail) = (
        "SELECT count(*)::bigint ",
        "FROM spot_command_ledger WHERE command_id = $1",
    );
    let sql = format!("{head}{tail}");
    let count: i64 = client.query_one(&sql, &[&command]).await.unwrap().get(0);
    count == 1
}

fn invocation(site: &str, command: &str, clock: &ManualClock) -> Invocation {
    Invocation {
        site: SiteId::new(site).unwrap(),
        actor: Actor::System {
            component: Name::new("ledger_host_test").unwrap(),
        },
        command_id: Some(CommandId::new(command).unwrap()),
        language: None,
        now: clock.now_utc(),
    }
}

fn label(name: &str) -> Document {
    let mut document = Document::new(Name::new("label").unwrap(), RecordId::new_v7());
    document.set(Name::new("name").unwrap(), Value::Text(name.to_owned()));
    document
}

/// One create under the retention class `class`, through the running site's own engine.
async fn write(started: &Started, env: &Env, clock: &ManualClock, class: &str, command: &str) {
    let live = started
        .running
        .live(&env.site)
        .expect("the site is serving");
    let engine = live.engine().expect("the engine is attached");
    let inv = invocation(&env.site, command, clock);
    engine
        .for_class(class)
        .create(&inv, label(command))
        .await
        .unwrap();
}

/// Waits until the start-up prune has run: `check` becomes true.
async fn wait_for(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let started = Instant::now();
    while !check().await {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "never happened: {what}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_class_override_and_the_maximum_reach_the_engine() {
    let config = section("3d", "10d", &[("bpm", "30d"), ("import", "5d")]);
    let Some(env) = Env::new(&config).await else {
        return skip_note();
    };
    env.site();
    let clock = manual_clock();
    let started = start_with(&env, options(&clock)).await;
    // Two entry-point classes, one instant: `bpm` asks for 30 days and is clamped to the maximum
    // of 10; `import` keeps its own 5; a class the site never named takes the default of 3.
    write(&started, &env, &clock, "bpm", "cmd-bpm").await;
    write(&started, &env, &clock, "import", "cmd-import").await;
    write(&started, &env, &clock, "unlisted", "cmd-other").await;
    assert_eq!(kept_seconds(&env, "cmd-bpm").await, (10 * DAY) as i64);
    assert_eq!(kept_seconds(&env, "cmd-import").await, (5 * DAY) as i64);
    assert_eq!(kept_seconds(&env, "cmd-other").await, (3 * DAY) as i64);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn absent_keys_give_the_documented_seven_days() {
    let Some(env) = Env::new("").await else {
        return skip_note();
    };
    env.site();
    let clock = manual_clock();
    let started = start_with(&env, options(&clock)).await;
    write(&started, &env, &clock, "bpm", "cmd-default").await;
    assert_eq!(kept_seconds(&env, "cmd-default").await, (7 * DAY) as i64);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_invalid_section_refuses_to_start_and_names_the_site_and_the_key() {
    let Some(env) = Env::new("").await else {
        return skip_note();
    };
    env.site();
    let cases: [(&str, String, &str); 7] = [
        (
            "zero default",
            section("0s", "30d", &[]),
            "retention.default",
        ),
        ("negative", section("-1d", "30d", &[]), "retention.default"),
        (
            "unparseable",
            section("soon", "30d", &[]),
            "retention.default",
        ),
        (
            "overflow",
            section("7d", "99999999999999999999d", &[]),
            "retention.max",
        ),
        ("max below default", section("7d", "1d", &[]), "retention"),
        (
            "zero class",
            section("7d", "30d", &[("bpm", "0h")]),
            "retention.classes.bpm",
        ),
        (
            "invalid class name",
            section("7d", "30d", &[("Not-A-Class", "1d")]),
            "classes.Not-A-Class",
        ),
    ];
    // A site with a refused section does not serve: the host stays up (the other sites serve) and
    // the site answers the fixed unavailable envelope. The reason, which names the site and the
    // key, goes to the operator's log (stderr) and never to the caller.
    for (what, text, _key) in cases {
        env.write_common(&["'sample'"], &text);
        let started = start(&env).await;
        let reply = http(
            started.addr,
            &env.site,
            None,
            "GET",
            "/api/v1/auth/me",
            &[],
            None,
        )
        .await;
        assert_eq!(reply.status, 503, "{what}: {}", reply.body);
        assert_eq!(
            reply.body["error"]["details"]["reason"], "site_unavailable",
            "{what}: {}",
            reply.body
        );
        started.finish().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_setting_applies_to_new_rows_only() {
    let first = section("3d", "10d", &[]);
    let Some(env) = Env::new(&first).await else {
        return skip_note();
    };
    env.site();
    let clock = manual_clock();
    let started = start_with(&env, options(&clock)).await;
    write(&started, &env, &clock, "bpm", "cmd-before").await;
    started.finish().await;

    env.write_common(&["'sample'"], &section("5d", "10d", &[]));
    let started = start_with(&env, options(&clock)).await;
    write(&started, &env, &clock, "bpm", "cmd-after").await;
    // The row written under the old setting keeps its own date; the new one has the new date.
    assert_eq!(kept_seconds(&env, "cmd-before").await, (3 * DAY) as i64);
    assert_eq!(kept_seconds(&env, "cmd-after").await, (5 * DAY) as i64);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_prune_removes_a_row_past_its_own_date_and_keeps_one_still_inside_it() {
    // `jobs` keeps a day, `bpm` twenty: after two days only the first is due.
    let config = section("1d", "30d", &[("jobs", "1d"), ("bpm", "20d")]);
    let Some(env) = Env::new(&config).await else {
        return skip_note();
    };
    env.site();
    let clock = manual_clock();
    let started = start_with(&env, options(&clock)).await;
    write(&started, &env, &clock, "jobs", "cmd-short").await;
    write(&started, &env, &clock, "bpm", "cmd-long").await;
    assert_eq!(rows(&env).await, 2);
    started.finish().await;

    // The host's clock moves on two days (never the system clock); the next host prunes at start.
    clock.advance(Duration::from_secs(2 * DAY));
    let started = start_with(&env, options(&clock)).await;
    wait_for("the due row was pruned", async || {
        !has_row(&env, "cmd-short").await
    })
    .await;
    // Its `recorded_at` is two days old, but its own `retain_until` is eighteen days away.
    assert!(has_row(&env, "cmd-long").await);
    assert_eq!(rows(&env).await, 1);
    started.finish().await;
}
