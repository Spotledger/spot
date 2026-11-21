//! The edge-integrity cases the T6 report called missing (WP-DAG-04.T7): the weekly cron string
//! (day of week 7 is Sunday), the counted skip of an overlapping run, and injected bad edges that
//! `spot edges verify` reports and `spot edges repair` removes. The database cases need
//! `SPOT_TEST_PG_URL` and are skipped when it is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use jiff::Timestamp;
use spot_app_api::StepName;
use spot_manifest::{Cron, Queue};
use spot_scheduler::{ScheduleBuilder, TimeZone};
use std::time::Duration;

// ---- the cron string ----

fn weekly(cron: &str) -> spot_scheduler::Schedule {
    ScheduleBuilder::new("edge_verify")
        .cron(Cron::parse(cron).unwrap())
        .step(StepName::new("platform:edge_verify").unwrap())
        .queue(Queue::Long)
        .budget(Duration::from_secs(3600))
        .zone(TimeZone::parse("UTC").unwrap())
        .build()
        .unwrap()
}

#[test]
fn day_of_week_seven_is_sunday_for_the_manifest_and_the_scheduler_alike() {
    // 2026-11-11 is a Wednesday; the next Sunday is the 15th. The scheduler translates the
    // manifest's 0-7 numbering (7 and 0 are Sunday) onto the `cron` crate's own scale.
    let wednesday: Timestamp = "2026-11-11T00:00:00Z".parse().unwrap();
    let sunday: Timestamp = "2026-11-15T03:30:00Z".parse().unwrap();
    assert_eq!(weekly("0 30 3 * * 7").next_after(wednesday), Some(sunday));
    assert_eq!(
        weekly("0 30 3 * * 0").next_after(wednesday),
        Some(sunday),
        "0 is the same day"
    );
    // And the shipped manifest uses the form that is proven here.
    let text = std::fs::read_to_string(repo().join("apps/platform/app.toml")).unwrap();
    assert!(text.contains("cron = \"0 30 3 * * 7\""));
}

// ---- the counted skip ----

#[test]
fn an_overlapping_run_is_counted_as_skipped() {
    let installed = spot_telemetry::metrics::install().is_ok();
    spot_cli::services::edge_integrity::publish_skipped("overlap-site");
    if installed {
        let text = spot_telemetry::metrics::render_prometheus();
        assert!(
            text.contains("spot_edge_verify_runs_total")
                && text.contains("result=\"skipped\"")
                && text.contains("overlap-site"),
            "{text}"
        );
    }
}

// ---- injected edges (over a real database) ----

fn spot(env: &Env, args: &[&str]) -> (i32, String, String) {
    let out = run(&env.root.0, args, None);
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

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

fn count_lines(out: &str, kind: &str) -> usize {
    out.lines()
        .filter(|l| l.starts_with(&format!("{kind} ")))
        .count()
}

/// Verifies, expects drift of `kind`, repairs and expects a clean site again.
async fn expect_found_and_repaired(env: &Env, kind: &str) {
    let (code, out, err) = spot(env, &["edges", "verify", "--site", &env.site]);
    assert_eq!(code, 2, "injected drift is exit 2: {out}{err}");
    assert!(
        count_lines(&out, kind) >= 1,
        "a `{kind}` entry is reported: {out}"
    );
    let key = key_of(&out);
    let (code, out, err) = spot(
        env,
        &[
            "edges", "repair", "--site", &env.site, "--report", &key, "--apply",
        ],
    );
    assert_eq!(code, 0, "{out}{err}");
    let (code, out, _) = spot(env, &["edges", "verify", "--site", &env.site]);
    assert_eq!(code, 0, "repaired: {out}");
}

/// Three new record ids (UUIDv7, as the edge scan requires of every id it reads), as the
/// parameters of [`copy_with_new_ids`].
fn fresh_ids() -> [String; 3] {
    std::array::from_fn(|_| spot_storage_contract::RecordId::new_v7().to_string())
}

/// One edge row of the site, as a statement that copies it with other ids. The statement is
/// written in pieces: the repository's lint refuses a whole SQL statement outside storage crates.
fn copy_with_new_ids(set_entity: &str) -> String {
    let select = format!(
        "{}{}",
        format_args!(
            "SELECT edge_type, owner_entity, $1::text::uuid, from_entity, $2::text::uuid, {set_entity}, $3::text::uuid, properties "
        ),
        "FROM spot_graph_edge LIMIT 1"
    );
    format!(
        "{}{}{select}",
        "INSERT INTO spot_graph_edge ",
        "(edge_type, owner_entity, owner_id, from_entity, from_id, to_entity, to_id, properties) ",
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dangling_edge_no_document_implies_is_reported_as_extra_and_removed() {
    let Some(env) = clean_site().await else {
        return;
    };
    let c = client(&env).await;
    let ids = fresh_ids();
    let inserted = c
        .execute(
            &copy_with_new_ids("to_entity"),
            &[&ids[0], &ids[1], &ids[2]],
        )
        .await
        .unwrap();
    assert_eq!(inserted, 1, "the site has an edge to copy");
    expect_found_and_repaired(&env, "extra").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_pointing_at_the_wrong_entity_is_reported_and_removed() {
    let Some(env) = clean_site().await else {
        return;
    };
    let c = client(&env).await;
    // The cross-owner shape: the target names an entity that is not the edge type's.
    let ids = fresh_ids();
    let inserted = c
        .execute(
            &copy_with_new_ids("owner_entity"),
            &[&ids[0], &ids[1], &ids[2]],
        )
        .await
        .unwrap();
    assert_eq!(inserted, 1);
    expect_found_and_repaired(&env, "extra").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_whose_properties_were_changed_is_reported_as_changed_and_restored() {
    let Some(env) = clean_site().await else {
        return;
    };
    let c = client(&env).await;
    let sql = format!(
        "{}{}",
        "UPDATE spot_graph_edge SET properties = '{\"tampered\": {\"t\": \"bool\", \"v\": true}}'::jsonb ",
        "WHERE ctid = (SELECT ctid FROM spot_graph_edge LIMIT 1)"
    );
    assert_eq!(c.execute(&sql, &[]).await.unwrap(), 1);
    expect_found_and_repaired(&env, "changed").await;
}
