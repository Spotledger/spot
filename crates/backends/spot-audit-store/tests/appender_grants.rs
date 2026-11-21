//! The grants of the appender tables (WP-AI-11.T7): the role the host connects as runs the
//! appender and cannot rewrite its index; `verify` reports the grants; `install_guard` repairs
//! them. Every test needs a server and skips cleanly (printing `SKIPPED:`) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime};
use spot_audit_chain::{AuditStore, CheckpointSigner, LocalSigner, genesis_hash};
use spot_audit_store::{
    APPENDER_TABLE_DDL, PROTECTION_INCOMPLETE, PgAppenderStore, PgAuditStore, TABLE_DDL,
    TABLES_MISSING, install_guard, verify,
};
use spot_doc_audit::{
    ActorRef, AppenderStore, AuditSource, DocAuditError, DocAuditEvent, DocOperation, LaneAppender,
    LaneConfig, LaneHead, OutboxFeed, SiteCheckpoint, lane_site, outbox_event_of,
};
use spot_postgres::testing::TestDb;
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{OutboxRecord, RecordId};
use tokio_postgres::Client;

const SITE: &str = "site-a";

/// A pool whose every connection runs as the application role (`SET ROLE` at start-up), which is
/// how a production host connects.
fn role_pool(db: &TestDb) -> Pool {
    let config = db.config();
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .options("-c role=spot_audit_app")
        .application_name("spot-audit-store-role-test");
    let manager = Manager::from_config(
        driver,
        tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(4)
        .runtime(Runtime::Tokio1)
        .build()
        .expect("the pool builds")
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([7u8; 32]))
}

fn checkpoint(at: i64) -> SiteCheckpoint {
    let config = LaneConfig::new(2, 1);
    let heads: Vec<LaneHead> = (0..2)
        .map(|lane| LaneHead {
            lane,
            seq: u64::try_from(at).unwrap_or(0),
            head_hash: genesis_hash(&lane_site(SITE, lane)),
        })
        .collect();
    SiteCheckpoint::sign(&*signer(), SITE, &config, &heads, at).expect("checkpoint")
}

fn event(id: &str) -> DocAuditEvent {
    DocAuditEvent {
        site: SITE.to_owned(),
        doctype: "invoice".to_owned(),
        id: id.to_owned(),
        version: 1,
        operation: DocOperation::Create,
        actor: ActorRef {
            actor: format!("user:{}", RecordId::new_v7()),
            on_behalf_of: None,
            component: None,
        },
        at: 1_700_000_000_000_000,
        changed_fields: vec![],
        diff: vec![],
        hidden_changes: false,
        command_id: None,
        source: AuditSource::Api,
        truncated: false,
        diff_digest: None,
        method: None,
        correction_half: None,
        via: None,
        node: None,
        outcome: None,
        error_class: None,
        error_code: None,
    }
}

struct Outbox(Vec<OutboxRecord>);

impl OutboxFeed for Outbox {
    fn read<'a>(
        &'a self,
        after: FeedPosition,
        limit: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<OutboxRecord>, DocAuditError>> + Send + 'a>> {
        Box::pin(async move {
            Ok(self
                .0
                .iter()
                .filter(|r| r.position > after)
                .take(limit as usize)
                .cloned()
                .collect())
        })
    }
}

fn outbox(n: u64) -> Outbox {
    Outbox(
        (1..=n)
            .map(|i| OutboxRecord {
                position: FeedPosition::new(1, i),
                transaction: RecordId::new_v7(),
                ordinal: 0,
                event: outbox_event_of(&event(&RecordId::new_v7().to_string())).unwrap(),
            })
            .collect(),
    )
}

async fn refused(client: &Client, sql: &str) {
    let error = client.batch_execute(sql).await.expect_err(sql);
    assert_eq!(error.code().map(|c| c.code()), Some("42501"), "{sql}");
}

/// (a) Every statement the appender runs works as the role, after `install_guard`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_application_role_runs_the_whole_appender() {
    let Some(db) = common::db().await else { return };
    // The superuser prepares: tables, role, grants, triggers.
    let _ = common::store(&db).await;
    let pool = role_pool(&db);

    let state = PgAppenderStore::new(pool.clone(), SITE);
    // Every statement: bookmark insert and update, event insert/read-back/conflict, document
    // insert (identity column) and repeat, site checkpoint insert and update.
    state
        .set_bookmark(0, FeedPosition::new(1, 1))
        .await
        .unwrap();
    state
        .set_bookmark(0, FeedPosition::new(1, 2))
        .await
        .unwrap();
    state
        .set_bookmark(0, FeedPosition::new(1, 2))
        .await
        .unwrap();
    assert_eq!(state.bookmark(0).await.unwrap(), FeedPosition::new(1, 2));
    state.record_appended("ev-1", 0, 1).await.unwrap();
    state.record_appended("ev-1", 0, 1).await.unwrap();
    assert!(
        state.record_appended("ev-1", 0, 2).await.is_err(),
        "a conflict is refused"
    );
    assert_eq!(state.appended("ev-1").await.unwrap(), Some((0, 1)));
    state
        .record_document_entry("Invoice", "INV-1", 0, 1)
        .await
        .unwrap();
    state
        .record_document_entry("Invoice", "INV-1", 0, 1)
        .await
        .unwrap();
    state
        .record_document_entry("Invoice", "INV-1", 0, 2)
        .await
        .unwrap();
    assert_eq!(
        state.document_entries("Invoice", "INV-1").await.unwrap(),
        vec![(0, 1), (0, 2)]
    );
    state.set_site_checkpoint(checkpoint(100)).await.unwrap();
    state.set_site_checkpoint(checkpoint(200)).await.unwrap();
    assert_eq!(
        state.site_checkpoint().await.unwrap(),
        Some(checkpoint(200))
    );

    // And a real drain over the chain store, both as the role.
    let chain = Arc::new(PgAuditStore::new(pool.clone()));
    let appender = LaneAppender::new(
        Arc::clone(&chain),
        "site-b",
        0,
        LaneConfig::new(1, 1),
        signer(),
        Arc::new(PgAppenderStore::new(pool, "site-b")) as Arc<dyn AppenderStore>,
    );
    let report = appender
        .drain(&outbox(3), 100)
        .await
        .expect("the drain works as the role");
    assert_eq!(report.appended, 3);
    assert_eq!(
        chain
            .range(&lane_site("site-b", 0), 1, 100)
            .await
            .unwrap()
            .len(),
        3
    );
}

/// (b) The role cannot rewrite what it must not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_role_cannot_update_delete_or_truncate_the_indexes() {
    let Some(db) = common::db().await else { return };
    let _ = common::store(&db).await;
    let client = common::raw(&db).await;
    client
        .batch_execute("SET ROLE spot_audit_app")
        .await
        .unwrap();
    for table in ["spot_audit_event_index", "spot_audit_doc_index"] {
        refused(&client, &format!("UPDATE {table} SET lane = 9")).await;
        refused(&client, &format!("DELETE FROM {table}")).await;
        refused(&client, &format!("TRUNCATE {table}")).await;
    }
    for table in ["spot_audit_bookmark", "spot_audit_site_checkpoint"] {
        refused(&client, &format!("DELETE FROM {table}")).await;
        refused(&client, &format!("TRUNCATE {table}")).await;
    }
    // The chain's refusals still hold.
    for sql in [
        "UPDATE spot_audit_chain SET at = 3",
        "DELETE FROM spot_audit_chain",
        "TRUNCATE spot_audit_chain",
    ] {
        refused(&client, sql).await;
    }
}

/// (c) `verify` reports a widened grant and `install_guard` repairs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_widened_grant_is_reported_and_repaired() {
    let Some(db) = common::db().await else { return };
    let store = common::store(&db).await;
    let pool = store.pool().clone();
    let client = common::raw(&db).await;
    let found = verify(&pool).await.unwrap();
    assert!(found.appender_tables && found.appender_grants, "{found:?}");
    assert!(found.is_complete());

    for widen in [
        "GRANT UPDATE ON spot_audit_event_index TO spot_audit_app",
        "GRANT DELETE ON spot_audit_bookmark TO spot_audit_app",
        "GRANT SELECT ON spot_audit_doc_index TO PUBLIC",
        "REVOKE INSERT ON spot_audit_site_checkpoint FROM spot_audit_app",
    ] {
        client.batch_execute(widen).await.unwrap();
        let found = verify(&pool).await.unwrap();
        assert!(!found.appender_grants, "{widen}");
        assert!(found.appender_tables, "{widen}: the tables are still there");
        let error = found.require_complete().unwrap_err();
        assert_eq!(error.code, PROTECTION_INCOMPLETE, "{widen}");
        assert!(
            error.message.contains("spot_audit_appender_grants"),
            "{widen}"
        );

        install_guard(&pool).await.unwrap();
        assert!(
            verify(&pool).await.unwrap().is_complete(),
            "{widen}: repaired"
        );
    }
}

/// (d) A missing appender table: `verify` says so and `install_guard` refuses and changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_appender_table_fails_the_install_and_changes_nothing() {
    let Some(db) = common::db().await else { return };
    let client = common::raw(&db).await;
    // The chain tables and three of the four appender tables.
    for (_, ddl) in TABLE_DDL.iter().chain(APPENDER_TABLE_DDL.iter().skip(1)) {
        client.batch_execute(ddl).await.unwrap();
    }
    let pool = common::pool(&db, 2);

    let found = verify(&pool).await.unwrap();
    assert!(!found.appender_tables);
    assert!(!found.appender_grants, "no grants without every table");
    assert!(found.missing().contains(&"spot_audit_appender_tables"));

    let acl = || async {
        client
            .query(
                "SELECT relname::text, relacl::text FROM pg_class \
                 WHERE relname LIKE 'spot\\_audit\\_%' AND relkind = 'r' ORDER BY 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{:?} {:?}",
                    r.get::<_, String>(0),
                    r.get::<_, Option<String>>(1)
                )
            })
            .collect::<Vec<_>>()
    };
    let before = acl().await;
    let error = install_guard(&pool).await.unwrap_err();
    assert_eq!(error.code, TABLES_MISSING);
    assert!(
        error.message.contains("spot_audit_bookmark"),
        "{}",
        error.message
    );
    assert_eq!(acl().await, before, "no grant changed");
    let n: i64 = client
        .query_one(
            "SELECT count(*) FROM pg_proc WHERE proname = 'spot_audit_chain_guard'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 0, "no guard function was created");
}

/// (e) `install_guard` twice changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_guard_twice_changes_nothing() {
    let Some(db) = common::db().await else { return };
    let store = common::store(&db).await;
    let pool = store.pool().clone();
    let client = common::raw(&db).await;
    let acl = || async {
        client
            .query(
                "SELECT relname::text, relacl::text FROM pg_class \
                 WHERE relname LIKE 'spot\\_audit\\_%' AND relkind = 'r' ORDER BY 1",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{:?} {:?}",
                    r.get::<_, String>(0),
                    r.get::<_, Option<String>>(1)
                )
            })
            .collect::<Vec<_>>()
    };
    install_guard(&pool).await.unwrap();
    let once = acl().await;
    install_guard(&pool).await.unwrap();
    assert_eq!(acl().await, once);
    assert_eq!(once.len(), 6, "{once:?}");
    assert!(verify(&pool).await.unwrap().is_complete());
}
