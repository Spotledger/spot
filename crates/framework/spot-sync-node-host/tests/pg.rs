//! The PostgreSQL pieces on a real database: the cursor stores (monotonic, per tenant, the host
//! refuses a missing table) and the outbox reader (whole transactions, the mapper's classes).
//! Gated on `SPOT_TEST_PG_URL` (skipped with a message when unset, a failure when
//! `SPOT_REQUIRE_PG=1`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::sync::Arc;
use std::time::Duration;

use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_sync_node_host::applier::CursorStore;
use spot_sync_node_host::pg::{DDL, OwnWritesMapper, PgCursors, PgOutboxReader, STATEMENTS};
use spot_sync_node_host::push::{OutboxSource, PushCursor, PushCursorStore};

const POOL: usize = 6;

const OUTBOX_DDL: &str = "CREATE TABLE IF NOT EXISTS spot_outbox (\
    id uuid PRIMARY KEY, tx uuid NOT NULL, ordinal integer NOT NULL, topic text NOT NULL, \
    subject_entity text NOT NULL, subject_id uuid NOT NULL, subject_version bigint, \
    schema_version integer NOT NULL, payload text NOT NULL, captured_at timestamptz NOT NULL, \
    xid xid8 NOT NULL); \
    CREATE TABLE IF NOT EXISTS spot_outbox_pub (\
    generation bigint NOT NULL, seq bigint NOT NULL, event_id uuid NOT NULL, \
    published_at timestamptz NOT NULL, PRIMARY KEY (generation, seq))";

struct Fixture {
    rt: tokio::runtime::Runtime,
    db: TestDb,
}

fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

fn bridge(config: PgConfig) -> Arc<Bridge> {
    let mut config = config;
    config.pool_max_size = POOL;
    let backend = PgBackend::connect_unchecked(config).expect("backend");
    Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "sync_node_host_test".to_owned(),
                workers: 2,
                max_in_flight: 4,
                queue_capacity: 64,
                default_deadline: Duration::from_secs(10),
                max_deadline: Duration::from_secs(30),
            },
            &backend,
        )
        .expect("bridge"),
    )
}

#[test]
fn every_statement_that_touches_the_cursor_table_carries_the_tenant() {
    for sql in STATEMENTS {
        if sql.contains("spot_sync_node_cursor") && !sql.contains("to_regclass") {
            assert!(
                sql.contains("tenant = $1") || sql.trim_start().starts_with("INSERT"),
                "{sql}"
            );
        }
    }
    assert!(
        DDL.to_ascii_lowercase()
            .starts_with("create table if not exists spot_sync_node_cursor")
    );
}

#[test]
fn the_cursors_are_monotonic_per_tenant_and_the_table_is_required() {
    let Some(f) = fixture() else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let bridge = bridge(f.db.config());
    let acme = PgCursors::new(Arc::clone(&bridge), "acme").unwrap();
    // The host never creates the table: it is missing until the migrator ran.
    let error = acme.require_table().unwrap_err();
    assert!(error.message.contains("spot migrate"));
    f.rt.block_on(async {
        f.db.raw_client().await.batch_execute(DDL).await.unwrap();
    });
    acme.require_table().unwrap();

    let mut serve = acme.serve();
    assert_eq!(serve.load().unwrap(), 0);
    serve.store(5).unwrap();
    serve.store(3).unwrap();
    assert_eq!(
        serve.load().unwrap(),
        5,
        "a lower value never replaces a higher one"
    );
    let mut push = acme.push();
    push.store(PushCursor {
        seq: 4,
        position: 90,
    })
    .unwrap();
    assert_eq!(
        push.load().unwrap(),
        PushCursor {
            seq: 4,
            position: 90
        }
    );

    // Another tenant has its own rows.
    let other = PgCursors::new(bridge, "globex").unwrap();
    assert_eq!(other.serve().load().unwrap(), 0);
    assert_eq!(other.push().load().unwrap(), PushCursor::default());
}

#[test]
fn the_cursor_store_refuses_to_be_built_inside_a_runtime() {
    let Some(f) = fixture() else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let bridge = bridge(f.db.config());
    let refused = f.rt.block_on(async { PgCursors::new(bridge, "acme") });
    assert!(refused.is_err());
}

#[test]
fn the_outbox_reader_returns_whole_transactions_and_only_forwarded_classes() {
    let Some(f) = fixture() else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    f.rt.block_on(async {
        let client = f.db.raw_client().await;
        client.batch_execute(OUTBOX_DDL).await.unwrap();
        // Three events in transaction A (positions 1..3), one bookkeeping event (4), two in B.
        client
            .batch_execute(
                "INSERT INTO spot_outbox (id, tx, ordinal, topic, subject_entity, subject_id, \
                    subject_version, schema_version, payload, captured_at, xid) VALUES \
                 (md5('e1')::uuid, md5('a')::uuid, 0, 'doc.saved', 'Customer', md5('c1')::uuid, 1, 1, '{}', now(), pg_current_xact_id()), \
                 (md5('e2')::uuid, md5('a')::uuid, 1, 'doc.saved', 'Customer', md5('c2')::uuid, 1, 1, '{}', now(), pg_current_xact_id()), \
                 (md5('e3')::uuid, md5('a')::uuid, 2, 'doc.saved', 'Invoice', md5('i1')::uuid, 2, 1, '{}', now(), pg_current_xact_id()), \
                 (md5('e4')::uuid, md5('b')::uuid, 0, 'sys_condition_raised', 'Condition', md5('k')::uuid, 1, 1, '{}', now(), pg_current_xact_id()), \
                 (md5('e5')::uuid, md5('c')::uuid, 0, 'doc.saved', 'Invoice', md5('i2')::uuid, 1, 1, '{}', now(), pg_current_xact_id()); \
                 INSERT INTO spot_outbox_pub (generation, seq, event_id, published_at) VALUES \
                 (1, 1, md5('e1')::uuid, now()), (1, 2, md5('e2')::uuid, now()), (1, 3, md5('e3')::uuid, now()), \
                 (1, 4, md5('e4')::uuid, now()), (1, 5, md5('e5')::uuid, now())",
            )
            .await
            .unwrap();
    });
    let bridge = bridge(f.db.config());
    let mut reader = PgOutboxReader::new(bridge, 1, OwnWritesMapper).unwrap();
    // A page of 1 row still returns the whole of transaction A.
    let entries = reader.read_after(0, 1).unwrap();
    assert_eq!(
        entries.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(entries.iter().all(|e| e.owned && e.version >= 1));
    assert!(entries[0].name.starts_with("Customer/"));
    // The bookkeeping topic is not forwarded; the rest after it are.
    let rest = reader.read_after(3, 10).unwrap();
    assert_eq!(rest.iter().map(|e| e.position).collect::<Vec<_>>(), vec![5]);
    assert_eq!(reader.queued_after(0).unwrap(), 5);
    assert_eq!(reader.queued_after(5).unwrap(), 0);
}
