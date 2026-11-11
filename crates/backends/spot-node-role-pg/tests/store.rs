//! The node role store on a real database. Skips (a `SKIPPED:` line from `TestDb::create`) when
//! `SPOT_TEST_PG_URL` is unset; with `SPOT_REQUIRE_PG=1` a missing server fails.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::pool;
use spot_node_role_pg::{
    PgNodeRoleStore, StoredRole, create_table_for_tests, drop_table_for_tests, schema,
};
use spot_postgres::testing::TestDb;

const FP: &str = "sha256:abcdef0123456789";

async fn ready() -> Option<(TestDb, deadpool_postgres::Pool, PgNodeRoleStore)> {
    let db = TestDb::create().await?;
    let p = pool(&db);
    create_table_for_tests(&p).await.unwrap();
    let store = PgNodeRoleStore::new(p.clone());
    Some((db, p, store))
}

#[test]
fn the_ddl_is_one_create_table_if_not_exists() {
    assert!(schema::DDL.starts_with("CREATE TABLE IF NOT EXISTS spot_node_role_state"));
    assert_eq!(schema::DDL.matches(';').count(), 0);
    assert_eq!(schema::DDL.matches("CREATE ").count(), 1);
    assert_eq!(schema::TABLE, "spot_node_role_state");
}

#[test]
fn debug_and_display_never_show_the_fingerprint_or_site() {
    let s = StoredRole {
        role: "edge".into(),
        hub_fingerprint: Some(FP.into()),
    };
    let d = format!("{s:?}");
    assert!(!d.contains(FP) && d.contains("<redacted>"));
}

#[tokio::test]
async fn unknown_site_is_none_and_upsert_round_trips() {
    let Some((_db, _p, store)) = ready().await else {
        return;
    };
    assert_eq!(store.read("site-a").await.unwrap(), None);
    store.upsert("site-a", "edge", Some(FP), 100).await.unwrap();
    let got = store.read("site-a").await.unwrap().unwrap();
    assert_eq!(got.role, "edge");
    assert_eq!(got.hub_fingerprint.as_deref(), Some(FP));
}

#[tokio::test]
async fn second_upsert_updates_time_and_keeps_or_replaces_the_fingerprint() {
    let Some((db, _p, store)) = ready().await else {
        return;
    };
    store.upsert("s", "edge", Some(FP), 100).await.unwrap();
    store.upsert("s", "edge", None, 200).await.unwrap();
    let got = store.read("s").await.unwrap().unwrap();
    assert_eq!(got.hub_fingerprint.as_deref(), Some(FP));
    let at: i64 = db
        .raw_client()
        .await
        .query_one(
            "SELECT updated_at FROM spot_node_role_state WHERE site = 's'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(at, 200);
    store
        .upsert("s", "edge", Some("sha256:new"), 300)
        .await
        .unwrap();
    let got = store.read("s").await.unwrap().unwrap();
    assert_eq!(got.hub_fingerprint.as_deref(), Some("sha256:new"));
}

#[tokio::test]
async fn two_sites_are_independent() {
    let Some((_db, _p, store)) = ready().await else {
        return;
    };
    store.upsert("a", "hub", None, 1).await.unwrap();
    store.upsert("b", "edge", Some(FP), 2).await.unwrap();
    assert_eq!(store.read("a").await.unwrap().unwrap().role, "hub");
    assert_eq!(
        store.read("a").await.unwrap().unwrap().hub_fingerprint,
        None
    );
    assert_eq!(store.read("b").await.unwrap().unwrap().role, "edge");
}

#[tokio::test]
async fn a_missing_table_is_refused_then_recreated() {
    let Some((_db, p, store)) = ready().await else {
        return;
    };
    drop_table_for_tests(&p).await.unwrap();
    let r = store.read("x").await.unwrap_err();
    assert_eq!(r.code(), "node_role_pg.table_missing");
    let w = store.upsert("x", "hub", None, 1).await.unwrap_err();
    assert_eq!(w.code(), "node_role_pg.table_missing");
    create_table_for_tests(&p).await.unwrap();
    store.upsert("x", "hub", None, 1).await.unwrap();
    assert!(store.read("x").await.unwrap().is_some());
}

#[tokio::test]
async fn the_ddl_applied_twice_is_harmless() {
    let Some((_db, p, store)) = ready().await else {
        return;
    };
    store.upsert("k", "hub", None, 1).await.unwrap();
    create_table_for_tests(&p).await.unwrap();
    create_table_for_tests(&p).await.unwrap();
    assert!(store.read("k").await.unwrap().is_some());
}

#[tokio::test]
async fn errors_hold_neither_site_nor_fingerprint() {
    let Some((_db, p, store)) = ready().await else {
        return;
    };
    drop_table_for_tests(&p).await.unwrap();
    let site = "secret-site-name";
    let e = store.upsert(site, "edge", Some(FP), 1).await.unwrap_err();
    let text = format!("{e} {e:?}");
    assert!(!text.contains(site) && !text.contains(FP));
    let e = store.read(site).await.unwrap_err();
    let text = format!("{e} {e:?}");
    assert!(!text.contains(site));
}
