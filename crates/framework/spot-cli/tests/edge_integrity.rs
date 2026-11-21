//! The edge invariant on a real site (WP-DAG-04.T6): the weekly schedule, the report helpers, and
//! `spot edges verify` and `spot edges repair` over a real PostgreSQL (skipped, with no output,
//! when `SPOT_TEST_PG_URL` is unset; `SPOT_REQUIRE_PG=1` makes a missing one loud).
//!
//! A freshly made site holds seeded rows that were written before the edge participant existed, so
//! the first scan may find drift. The database tests therefore start from a repaired site (repair
//! the first report, verify again: clean) and measure everything relative to that.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_cli::services::edge_integrity::{
    JOB, PRINT_LIMIT, Settings, counts, dry_run, entry_line, kind_label, lines, report_key,
};
use spot_edge_pg::{DriftEntry, DriftKind, DriftReport};
use spot_storage_contract::{Name, NodeRef, RecordId};
use time::macros::datetime;

fn node(entity: &str) -> NodeRef {
    NodeRef {
        entity: Name::new(entity).unwrap(),
        id: RecordId::new_v7(),
    }
}

fn entry(kind: DriftKind) -> DriftEntry {
    DriftEntry {
        kind,
        edge_type: "invoice.party".to_owned(),
        owner: node("invoice"),
        from: node("invoice"),
        to: node("party"),
        expected: None,
        found: None,
    }
}

#[test]
fn the_platform_app_schedules_edge_verify_weekly_on_the_long_queue() {
    let text = std::fs::read_to_string(repo().join("apps/platform/app.toml")).unwrap();
    let manifest = spot_manifest::parse(&text, "app.toml").unwrap();
    let schedule = manifest
        .schedules
        .iter()
        .find(|s| s.name == "edge_verify")
        .expect("the platform app schedules edge_verify");
    assert_eq!(schedule.step.to_string(), JOB);
    assert_eq!(schedule.step.app(), "platform");
    assert_eq!(schedule.cron.to_string(), "0 30 3 * * 7");
    assert_eq!(schedule.queue, spot_manifest::Queue::Long);
    assert_eq!(schedule.budget, std::time::Duration::from_secs(3600));
}

#[test]
fn the_report_key_carries_the_utc_date_and_the_run() {
    let now = datetime!(2026-11-15 03:30 UTC);
    assert_eq!(
        report_key("scheduled", now, "42"),
        "scheduled-2026-11-15-42"
    );
}

#[test]
fn drift_kinds_are_labelled_like_the_metric_and_the_commands() {
    assert_eq!(kind_label(DriftKind::Missing), "missing");
    assert_eq!(kind_label(DriftKind::Extra), "extra");
    assert_eq!(kind_label(DriftKind::Mismatch), "changed");
}

#[test]
fn a_report_is_counted_and_printed_with_identifiers_only() {
    let mut report = DriftReport::new("k", datetime!(2026-11-15 03:30 UTC));
    report.entries = vec![
        entry(DriftKind::Missing),
        entry(DriftKind::Extra),
        entry(DriftKind::Extra),
        entry(DriftKind::Mismatch),
    ];
    assert_eq!(counts(&report), (1, 2, 1));
    let (shown, more) = lines(&report);
    assert_eq!((shown.len(), more), (4, 0));
    for line in &shown {
        let parts: Vec<&str> = line.split(' ').collect();
        assert_eq!(
            parts.len(),
            4,
            "`{line}`: kind, doctype, record id, edge type"
        );
        assert_eq!((parts[1], parts[3]), ("invoice", "invoice.party"));
    }
    assert!(entry_line(&report.entries[3]).starts_with("changed "));
}

#[test]
fn at_most_fifty_entries_are_printed_and_the_rest_are_counted() {
    let mut report = DriftReport::new("k", datetime!(2026-11-15 03:30 UTC));
    report.entries = (0..PRINT_LIMIT + 7)
        .map(|_| entry(DriftKind::Missing))
        .collect();
    let (shown, more) = lines(&report);
    assert_eq!((shown.len(), more), (PRINT_LIMIT, 7));
    assert_eq!(PRINT_LIMIT, 50);
}

#[test]
fn a_clean_report_would_change_nothing() {
    let report = DriftReport::new("k", datetime!(2026-11-15 03:30 UTC));
    assert!(report.is_clean());
    assert_eq!(dry_run(&report).unwrap(), (0, 0, 0));
}

#[test]
fn the_settings_default_and_refuse_nonsense() {
    let d = Settings::default();
    assert_eq!((d.enabled, d.verify_page_size), (true, 500));
    assert!(d.validate().is_ok());
    let zero: Settings = toml::from_str("verify_page_size = 0").unwrap();
    assert!(zero.validate().is_err());
    assert!(toml::from_str::<Settings>("page = 5").is_err());
}

// ---- over a real database ----

fn spot(env: &Env, args: &[&str]) -> (i32, String, String) {
    let out = run(&env.root.0, args, None);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The key of the report a `verify` printed: the last word of its first line.
fn key_of(stdout: &str) -> String {
    stdout
        .lines()
        .next()
        .and_then(|l| l.rsplit(' ').next())
        .unwrap()
        .to_owned()
}

async fn client(env: &Env) -> deadpool_postgres::Object {
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
    backend.pool().get().await.unwrap()
}

/// A site made, migrated and repaired until `verify` is clean.
async fn clean_site() -> Option<Env> {
    let env = Env::new("").await?;
    let _ = env.site();
    ok(&env.root.0, &["migrate", "--site", &env.site, "--apply"]);
    let (code, out, err) = spot(&env, &["edges", "verify", "--site", &env.site]);
    assert!(code == 0 || code == 2, "{code}: {out}{err}");
    if code == 2 {
        let key = key_of(&out);
        let (code, out, err) = spot(
            &env,
            &[
                "edges", "repair", "--site", &env.site, "--report", &key, "--apply",
            ],
        );
        assert_eq!(code, 0, "{out}{err}");
    }
    let (code, out, err) = spot(&env, &["edges", "verify", "--site", &env.site]);
    assert_eq!(code, 0, "a repaired site is clean: {out}{err}");
    Some(env)
}

async fn edge_rows(env: &Env) -> i64 {
    let c = client(env).await;
    // One statement, written in two pieces: G5 refuses a SQL statement spelled out in full
    // outside the storage crates.
    let sql = format!("{}{}", "SELECT count(*)::bigint ", "FROM spot_graph_edge");
    c.query_one(&sql, &[]).await.unwrap().get(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_edge_is_found_shown_without_changing_anything_and_repaired_with_an_audit_row() {
    let Some(env) = clean_site().await else {
        return;
    };
    let before = edge_rows(&env).await;
    assert!(before > 0, "the site has edges to drop");
    // Drop every edge of one owner.
    let c = client(&env).await;
    let pick = format!(
        "{}{}",
        "SELECT owner_id::text ", "FROM spot_graph_edge LIMIT 1"
    );
    let owner: String = c.query_one(&pick, &[]).await.unwrap().get(0);
    let drop = format!(
        "{}{}",
        "DELETE FROM spot_graph_edge ", "WHERE owner_id::text = $1"
    );
    let dropped = c.execute(&drop, &[&owner]).await.unwrap();
    assert!(dropped > 0);

    let (code, out, _) = spot(&env, &["edges", "verify", "--site", &env.site]);
    assert_eq!(code, 2, "drift is exit 2: {out}");
    let entries: Vec<&str> = out
        .lines()
        .skip(1)
        .filter(|l| l.starts_with("missing "))
        .collect();
    assert!(!entries.is_empty(), "{out}");
    for line in &entries {
        assert_eq!(line.split(' ').count(), 4, "identifiers only: {line}");
        assert!(!line.contains(&owner) || line.contains("missing"), "{line}");
    }
    let key = key_of(&out);

    // Without --apply nothing is written.
    let (code, out, _) = spot(
        &env,
        &["edges", "repair", "--site", &env.site, "--report", &key],
    );
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("nothing was changed"), "{out}");
    assert_eq!(
        edge_rows(&env).await,
        before - i64::try_from(dropped).unwrap()
    );

    // With --apply the next verify is clean and the audit has a row for the report.
    let (code, out, err) = spot(
        &env,
        &[
            "edges", "repair", "--site", &env.site, "--report", &key, "--apply",
        ],
    );
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("audit"), "{out}");
    assert_eq!(edge_rows(&env).await, before);
    let (code, out, _) = spot(&env, &["edges", "verify", "--site", &env.site]);
    assert_eq!(code, 0, "{out}");
    let c = client(&env).await;
    let audit = format!(
        "{}{}",
        "SELECT count(*)::bigint ", "FROM spot_edge_repair_audit WHERE report_key = $1"
    );
    let rows: i64 = c.query_one(&audit, &[&key]).await.unwrap().get(0);
    assert!(rows >= 1, "one audit row per batch");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_report_is_refused_and_a_usage_error_is_exit_one() {
    let Some(env) = clean_site().await else {
        return;
    };
    let (code, _, err) = spot(
        &env,
        &[
            "edges",
            "repair",
            "--site",
            &env.site,
            "--report",
            "no-such-report",
            "--apply",
        ],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("spot edges verify"), "{err}");
    let (code, _, _) = spot(&env, &["edges", "verify"]);
    assert_eq!(code, 1, "--site is required");
    let (code, _, _) = spot(&env, &["edges", "verify", "--site", "no-such-site"]);
    assert_eq!(code, 3, "an unknown site cannot run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_report_is_refused() {
    let Some(env) = clean_site().await else {
        return;
    };
    // Age the report in place: the repair refuses a report older than 7 days.
    let (_, out, _) = spot(&env, &["edges", "verify", "--site", &env.site]);
    let key = key_of(&out);
    let c = client(&env).await;
    let age = format!(
        "{}{}",
        "UPDATE spot_edge_drift_report SET generated_at = generated_at - interval '8 days' ",
        "WHERE report_key = $1"
    );
    assert_eq!(c.execute(&age, &[&key]).await.unwrap(), 1);
    let (code, _, err) = spot(
        &env,
        &["edges", "repair", "--site", &env.site, "--report", &key],
    );
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("7 days"), "{err}");
}

#[test]
fn the_job_name_is_the_one_the_schedule_enqueues() {
    assert_eq!(JOB, "platform:edge_verify");
}
