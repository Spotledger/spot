//! What only a real database can show: the evidence tables refuse rewrites, a restart loses
//! nothing, racing nodes have one winner, a damaged row is refused and a down database is a
//! fixed-sentence refusal.
//!
//! Skips cleanly when `SPOT_TEST_PG_URL` is unset (the outage test needs no server).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use deadpool_postgres::{PoolConfig, Runtime};
use spot_access_review::contract::{fresh_tenant, item, spec};
use spot_access_review::{CampaignStore, DeadlinePolicy, Decision, StoreError};
use spot_access_review_pg::PgCampaignStore;
use spot_postgres::testing::TestDb;
use spot_sod::contract::{RULES_V1, grant};
use spot_sod::{SodStore, SodStoreError};
use tokio_postgres::NoTls;

fn keep() -> Decision {
    Decision::Keep { note: None }
}

fn three() -> Vec<spot_access_review::ReviewItem> {
    vec![
        item("i1", "alice", "bob"),
        item("i2", "carol", "bob"),
        item("i3", "dave", "erin"),
    ]
}

fn sqlstate(e: &tokio_postgres::Error) -> Option<String> {
    e.code().map(|c| c.code().to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_evidence_tables_refuse_every_rewrite_even_from_the_owner() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (campaigns, sod) = common::stores(&db, 4, "ar_guard").await;
    let t = fresh_tenant();
    campaigns
        .open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    campaigns
        .decide(&t, "c1", "i1", "bob", keep(), 0, 1100)
        .await
        .unwrap();
    sod.put_rule_set(&t, RULES_V1, 0, "admin", 10)
        .await
        .unwrap();
    let seq = sod
        .grant_override(&t, grant("zoe", None), 5000, 1000)
        .await
        .unwrap();
    sod.revoke_override(&t, seq, "auditor", "lapsed", 2000)
        .await
        .unwrap();

    let client = db.raw_client().await;
    for sql in [
        "UPDATE spot_access_review_history SET at = at",
        "DELETE FROM spot_access_review_history",
        "UPDATE spot_sod_rule_sets SET toml = ''",
        "DELETE FROM spot_sod_rule_sets",
        "UPDATE spot_sod_overrides SET reason = 'edited'",
        "DELETE FROM spot_sod_overrides",
        "UPDATE spot_sod_override_revocations SET reason = 'edited'",
        "DELETE FROM spot_sod_override_revocations",
        // The snapshot, the spec and the version rule.
        "UPDATE spot_access_review_items SET snapshot = '{}'",
        "UPDATE spot_access_review_items SET version = version + 2",
        "DELETE FROM spot_access_review_items",
        "UPDATE spot_access_review_campaigns SET spec = '{}'",
        "UPDATE spot_access_review_campaigns SET open_events = '[]'",
        "DELETE FROM spot_access_review_campaigns",
    ] {
        let error = client.execute(sql, &[]).await.expect_err(sql);
        assert_eq!(sqlstate(&error).as_deref(), Some("42501"), "{sql}");
    }
    for table in [
        "spot_access_review_history",
        "spot_access_review_items",
        "spot_access_review_campaigns",
        "spot_sod_rule_sets",
        "spot_sod_overrides",
        "spot_sod_override_revocations",
    ] {
        let error = client
            .batch_execute(&format!("TRUNCATE {table} CASCADE"))
            .await
            .expect_err(table);
        assert_eq!(sqlstate(&error).as_deref(), Some("42501"), "{table}");
    }
    // A completed campaign cannot be reopened.
    let done = fresh_tenant();
    campaigns
        .open(
            &done,
            spec("c9", DeadlinePolicy::LeaveOpen),
            vec![item("i1", "a", "b")],
            1000,
        )
        .await
        .unwrap();
    campaigns
        .decide(&done, "c9", "i1", "b", keep(), 0, 1100)
        .await
        .unwrap();
    let error = client
        .execute(
            "UPDATE spot_access_review_campaigns SET status = 'open' WHERE campaign_id = 'c9'",
            &[],
        )
        .await
        .expect_err("reopen");
    assert_eq!(sqlstate(&error).as_deref(), Some("42501"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_application_role_has_no_update_or_delete_on_the_evidence() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (campaigns, _) = common::stores(&db, 2, "ar_role").await;
    let t = fresh_tenant();
    campaigns
        .open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    let client = db.raw_client().await;
    let exists = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_access_review_app')",
            &[],
        )
        .await
        .unwrap()
        .get::<_, bool>(0);
    assert!(exists, "the test server lets prepare create the role");
    for table in [
        "spot_access_review_history",
        "spot_sod_rule_sets",
        "spot_sod_overrides",
        "spot_sod_override_revocations",
    ] {
        for privilege in ["UPDATE", "DELETE", "TRUNCATE"] {
            let has: bool = client
                .query_one(
                    &format!("SELECT has_table_privilege('spot_access_review_app', '{table}', '{privilege}')"),
                    &[],
                )
                .await
                .unwrap()
                .get(0);
            assert!(!has, "{table} grants {privilege}");
        }
        let can_append: bool = client
            .query_one(
                &format!(
                    "SELECT has_table_privilege('spot_access_review_app', '{table}', 'INSERT')"
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(can_append, "{table} cannot be appended to");
    }
    for table in ["spot_access_review_campaigns", "spot_access_review_items"] {
        let has: bool = client
            .query_one(
                &format!(
                    "SELECT has_table_privilege('spot_access_review_app', '{table}', 'DELETE')"
                ),
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(!has, "{table} grants DELETE");
    }
    // As the role itself the rewrites are refused with `permission denied`.
    client
        .batch_execute("SET ROLE spot_access_review_app")
        .await
        .unwrap();
    for sql in [
        "UPDATE spot_access_review_history SET at = at",
        "DELETE FROM spot_access_review_history",
        "UPDATE spot_sod_overrides SET reason = 'x'",
        "DELETE FROM spot_sod_rule_sets",
    ] {
        let error = client.execute(sql, &[]).await.expect_err(sql);
        assert_eq!(sqlstate(&error).as_deref(), Some("42501"), "{sql}");
    }
    client.batch_execute("RESET ROLE").await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_is_idempotent_and_safe_from_two_nodes() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let a = PgCampaignStore::new(common::pool(&db, 2, "prep_a"));
    let b = PgCampaignStore::new(common::pool(&db, 2, "prep_b"));
    let (ra, rb) = tokio::join!(a.prepare(), b.prepare());
    ra.unwrap();
    rb.unwrap();
    a.prepare().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_loses_nothing_and_the_tick_stays_idempotent() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let t = fresh_tenant();
    {
        let (first, sod) = common::stores(&db, 2, "restart_one").await;
        first
            .open(
                &t,
                spec("c1", DeadlinePolicy::RemoveUnreviewed),
                three(),
                1000,
            )
            .await
            .unwrap();
        first
            .decide(&t, "c1", "i1", "bob", keep(), 0, 1100)
            .await
            .unwrap();
        let out = first.tick(&t, "c1", 2000).await.unwrap();
        assert_eq!(out.actions.len(), 2);
        sod.put_rule_set(&t, RULES_V1, 0, "admin", 10)
            .await
            .unwrap();
        sod.grant_override(&t, grant("zoe", None), 5000, 1000)
            .await
            .unwrap();
    }
    // A new process over the same database.
    let (second, sod) = common::stores(&db, 2, "restart_two").await;
    assert!(
        second.tick(&t, "c1", 2000).await.unwrap().is_empty(),
        "already done"
    );
    assert!(second.tick(&t, "c1", 9000).await.unwrap().is_empty());
    let stored = second.load(&t, "c1").await.unwrap();
    assert_eq!(stored.item_versions, vec![1, 1, 1]);
    assert_eq!(
        second.history(&t, "c1", 0, 200).await.unwrap().len(),
        1 + 1 + 2 + 2 + 1,
        "opened, decided, two removed events, two actions, completed"
    );
    assert_eq!(sod.rule_set(&t).await.unwrap().unwrap().version, 1);
    assert_eq!(sod.active_overrides(&t, 1500).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn nodes_racing_for_one_item_have_exactly_one_winner() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (a, _) = common::stores(&db, 4, "race_a").await;
    let (b, _) = common::stores(&db, 4, "race_b").await;
    let t = fresh_tenant();
    a.open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    let a = Arc::new(a);
    let b = Arc::new(b);
    let mut tasks = Vec::new();
    for n in 0..8 {
        let store = if n % 2 == 0 {
            Arc::clone(&a)
        } else {
            Arc::clone(&b)
        };
        tasks.push(tokio::spawn(async move {
            let decision = if n % 3 == 0 {
                Decision::Remove {
                    reason: format!("reason {n}"),
                }
            } else {
                keep()
            };
            store.decide(&t, "c1", "i1", "bob", decision, 0, 1100).await
        }));
    }
    let mut wins = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => wins += 1,
            Err(e) => assert!(
                matches!(e, StoreError::VersionConflict | StoreError::Review(_)),
                "unexpected {e:?}"
            ),
        }
    }
    assert_eq!(wins, 1);
    assert_eq!(a.load(&t, "c1").await.unwrap().item_versions[0], 1);
    // Racing ticks: one change, the rest empty.
    let (x, y) = tokio::join!(a.tick(&t, "c1", 2000), b.tick(&t, "c1", 2000));
    assert!(
        x.unwrap().is_empty() && y.unwrap().is_empty(),
        "LeaveOpen changes nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_campaign_or_rule_set_is_refused_not_served() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (campaigns, sod) = common::stores(&db, 4, "damaged").await;
    let t = fresh_tenant();
    campaigns
        .open(&t, spec("c1", DeadlinePolicy::LeaveOpen), three(), 1000)
        .await
        .unwrap();
    let client = db.raw_client().await;
    // Hand-edit (with the guard off, as a careless operator might) so the status contradicts
    // the item states.
    client
        .batch_execute(
            "ALTER TABLE spot_access_review_campaigns DISABLE TRIGGER spot_access_review_campaigns_rows;\
             UPDATE spot_access_review_campaigns SET status = 'completed';\
             ALTER TABLE spot_access_review_campaigns ENABLE TRIGGER spot_access_review_campaigns_rows;",
        )
        .await
        .unwrap();
    assert_eq!(
        campaigns.load(&t, "c1").await.unwrap_err(),
        StoreError::Corrupt
    );
    assert_eq!(
        campaigns.tick(&t, "c1", 3000).await.unwrap_err(),
        StoreError::Corrupt
    );
    assert_eq!(
        campaigns
            .decide(&t, "c1", "i1", "bob", keep(), 0, 1100)
            .await
            .unwrap_err(),
        StoreError::Corrupt
    );

    // A rule-set version that no longer validates (inserted directly: no writer would store it).
    sod.put_rule_set(&t, RULES_V1, 0, "admin", 10)
        .await
        .unwrap();
    let tenant = t.to_string();
    client
        .execute(
            "INSERT INTO spot_sod_rule_sets (tenant, version, toml, set_by, set_at) VALUES ($1, 2, 'rules = 3', 'x', 1)",
            &[&tenant],
        )
        .await
        .unwrap();
    assert_eq!(sod.rule_set(&t).await.unwrap_err(), SodStoreError::Corrupt);
    assert_eq!(
        sod.grant_override(&t, grant("zoe", None), 5000, 1000)
            .await
            .unwrap_err(),
        SodStoreError::Corrupt,
        "an override is never recorded against a rule set that cannot be read"
    );
}

#[tokio::test]
async fn a_down_database_is_a_fixed_sentence_refusal() {
    // Nothing listens on port 1; the pool never connects until used.
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some("127.0.0.1".to_owned());
    cfg.port = Some(1);
    cfg.dbname = Some("secret-database-name".to_owned());
    cfg.user = Some("secret-user".to_owned());
    cfg.password = Some("secret-password".to_owned());
    cfg.pool = Some(PoolConfig::new(1));
    let pool = cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap();
    let campaigns = PgCampaignStore::new(pool.clone());
    let sod = spot_access_review_pg::PgSodStore::new(pool);
    let t = fresh_tenant();
    let e = campaigns.load(&t, "c1").await.unwrap_err();
    assert_eq!(e, StoreError::Unavailable);
    let e = campaigns
        .decide(&t, "c1", "i1", "bob", keep(), 0, 1)
        .await
        .unwrap_err();
    assert_eq!(
        e,
        StoreError::Unavailable,
        "a decision is refused, never assumed"
    );
    let shown = format!("{e} {e:?} {:?} {:?}", campaigns, sod);
    for secret in ["secret", "127.0.0.1", "5432"] {
        assert!(!shown.contains(secret), "{shown}");
    }
    let e = sod.rule_set(&t).await.unwrap_err();
    assert_eq!(e, SodStoreError::Unavailable);
    // Fail closed for overrides too.
    assert_eq!(
        sod.grant_override(&t, grant("zoe", None), 5000, 1000)
            .await
            .unwrap_err(),
        SodStoreError::Unavailable
    );
}
