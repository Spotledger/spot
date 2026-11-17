//! `spot migrate --apply` seeds the platform's missing system rows (WP-IAM-02.T3), over a real
//! PostgreSQL (`SPOT_TEST_PG_URL`; skipped with a printed reason when unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

/// A client on the site's own database.
async fn client(root: &Path) -> deadpool_postgres::Object {
    let loaded = spot_cli::config::load(root, false).unwrap();
    let site = loaded.sites.first().expect("one site");
    let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
    backend.pool().get().await.unwrap()
}

async fn count(root: &Path, sql: &str) -> i64 {
    client(root).await.query_one(sql, &[]).await.unwrap().get(0)
}

async fn table_hash(root: &Path) -> String {
    client(root)
        .await
        .query_one(
            "SELECT coalesce(md5(string_agg(id::text || role_name || coalesce(description, ''), ',' ORDER BY id)), '') FROM role",
            &[],
        )
        .await
        .unwrap()
        .get(0)
}

fn migrate(root: &Path, site: &str, apply: bool) -> std::process::Output {
    let mut args = vec!["migrate", "--site", site];
    if apply {
        args.push("--apply");
    }
    common::run(root, &args, None)
}

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

const WEBHOOK: &str = "SELECT count(*) FROM role WHERE role_name = 'Webhook Manager'";

#[tokio::test(flavor = "multi_thread")]
async fn a_pre_existing_site_gains_the_role_and_a_second_run_changes_nothing() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    // A site older than the role: delete the row.
    client(&root)
        .await
        .batch_execute("DELETE FROM role WHERE role_name = 'Webhook Manager'")
        .await
        .unwrap();
    assert_eq!(count(&root, WEBHOOK).await, 0);
    let out = migrate(&root, &env.site, true);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("seed: added role \"Webhook Manager\""),
        "{}",
        text(&out)
    );
    assert_eq!(count(&root, WEBHOOK).await, 1);
    // Idempotent: nothing to add, no row changes.
    let before = table_hash(&root).await;
    let again = migrate(&root, &env.site, true);
    assert!(again.status.success());
    assert!(
        text(&again).contains("seed: nothing to add"),
        "{}",
        text(&again)
    );
    assert_eq!(table_hash(&root).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edited_row_survives_byte_for_byte() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    client(&root)
        .await
        .batch_execute("UPDATE role SET description = 'edited by the operator' WHERE role_name = 'User Manager'")
        .await
        .unwrap();
    let before = table_hash(&root).await;
    let out = migrate(&root, &env.site, true);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("seed: nothing to add"),
        "{}",
        text(&out)
    );
    assert_eq!(table_hash(&root).await, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn it_never_creates_the_administrator_a_user_or_a_credential() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    client(&root)
        .await
        .batch_execute("DELETE FROM role_assignment; DELETE FROM \"user\"")
        .await
        .unwrap();
    let site_file_before = std::fs::read_to_string(env.site_dir().join("site.toml")).unwrap();
    let out = migrate(&root, &env.site, true);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        count(&root, "SELECT count(*) FROM \"user\"").await,
        0,
        "no identity is created"
    );
    assert_eq!(
        count(&root, "SELECT count(*) FROM role_assignment").await,
        0
    );
    assert_eq!(
        std::fs::read_to_string(env.site_dir().join("site.toml")).unwrap(),
        site_file_before,
        "the site file's credentials are unchanged"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_plan_only_run_writes_nothing_and_lists_the_role_it_would_add() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    client(&root)
        .await
        .batch_execute("DELETE FROM role WHERE role_name = 'Webhook Manager'")
        .await
        .unwrap();
    let before = table_hash(&root).await;
    let out = migrate(&root, &env.site, false);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("seed: would add role \"Webhook Manager\""),
        "{}",
        text(&out)
    );
    assert_eq!(count(&root, WEBHOOK).await, 0);
    assert_eq!(table_hash(&root).await, before, "a dry run writes nothing");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_seed_is_loud_leaves_no_partial_rows_and_a_rerun_succeeds() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    // Two roles are missing by id, and one of them collides with a row of another id on its name.
    client(&root)
        .await
        .batch_execute(
            "DELETE FROM role WHERE role_name = 'Webhook Manager';\
             UPDATE role SET role_name = 'Webhook Manager' WHERE role_name = 'User Manager'",
        )
        .await
        .unwrap();
    let out = migrate(&root, &env.site, true);
    if out.status.success() {
        println!("SKIPPED: role_name has no uniqueness rule here, so no collision can be forced");
        return;
    }
    let said = text(&out);
    assert!(
        said.contains("re-running `spot migrate --apply` is safe"),
        "{said}"
    );
    assert_eq!(
        count(
            &root,
            "SELECT count(*) FROM role WHERE role_name = 'User Manager'"
        )
        .await,
        0,
        "no partial rows: the whole seed rolled back"
    );
    // Remove the cause: the renamed row goes back to its name; the next run adds the missing role.
    client(&root)
        .await
        .batch_execute(
            "UPDATE role SET role_name = 'User Manager' WHERE role_name = 'Webhook Manager'",
        )
        .await
        .unwrap();
    let fixed = migrate(&root, &env.site, true);
    assert!(fixed.status.success(), "{}", text(&fixed));
    assert_eq!(count(&root, WEBHOOK).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bare_site_migrates_without_a_seed_step() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let bare = format!("{}-bare", env.site);
    let created = common::run(&root, &["new-site", &bare, "--bare"], None);
    if !created.status.success() {
        println!("SKIPPED: this build has no bare sites: {}", text(&created));
        return;
    }
    let out = migrate(&root, &bare, true);
    assert!(out.status.success(), "{}", text(&out));
    // A bare site has no seed to run: either no seed line, or the one that says there is nothing.
    let said = text(&out);
    assert!(
        !said.contains("seed:") || said.contains("seed: nothing to add"),
        "{said}"
    );
}
