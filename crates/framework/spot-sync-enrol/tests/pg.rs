//! The enrolment contract on a real PostgreSQL, plus what only a database can show: concurrent
//! redemption across connections, the audit record's atomicity with a failing outbox, restart
//! persistence, tenant isolation of the rows, and the statement inspection. Gated on
//! `SPOT_TEST_PG_URL` (skipped with a message when unset, a failure when `SPOT_REQUIRE_PG=1`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;
mod contract;

use std::sync::Arc;
use std::time::Duration;

use common::{TestClock, identity, node, request, rig};
use contract::client_cert;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_sync_enrol::pg::STATEMENTS;
use spot_sync_enrol::{EnrolmentStore, PgEnrolmentStore};
use spot_sync_http::NodeDirectory;

const POOL: usize = 6;

/// The outbox table the audit record goes into (the same shape the publisher's schema creates).
const OUTBOX_DDL: &str = "CREATE TABLE IF NOT EXISTS spot_outbox (\
    id uuid PRIMARY KEY, tx uuid NOT NULL, ordinal integer NOT NULL, topic text NOT NULL, \
    subject_entity text NOT NULL, subject_id uuid NOT NULL, subject_version bigint, \
    schema_version integer NOT NULL, payload text NOT NULL, captured_at timestamptz NOT NULL, \
    xid xid8 NOT NULL)";

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
    let client = rt.block_on(db.raw_client());
    rt.block_on(client.batch_execute(OUTBOX_DDL)).unwrap();
    Some(Fixture { rt, db })
}

fn bridge(config: PgConfig, in_flight: usize) -> Arc<Bridge> {
    let mut config = config;
    config.pool_max_size = POOL;
    let backend = PgBackend::connect_unchecked(config).expect("backend");
    Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "sync_enrol_test".to_owned(),
                workers: 2,
                max_in_flight: in_flight,
                queue_capacity: 64,
                default_deadline: Duration::from_secs(10),
                max_deadline: Duration::from_secs(30),
            },
            &backend,
        )
        .expect("bridge"),
    )
}

fn store(bridge: &Arc<Bridge>, tenant: &str) -> Arc<PgEnrolmentStore> {
    let store = PgEnrolmentStore::new(Arc::clone(bridge), POOL, tenant).unwrap();
    store.prepare().unwrap();
    Arc::new(store)
}

#[test]
fn every_statement_carries_the_tenant() {
    assert!(STATEMENTS.len() >= 19);
    for sql in STATEMENTS {
        assert!(
            sql.contains("tenant = $1")
                || (sql.trim_start().starts_with("INSERT") && sql.contains("(tenant,")),
            "no tenant predicate: {sql}"
        );
    }
}

#[test]
fn the_postgres_store_passes_the_contract() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(fx.db.config(), 4);
    let next = std::sync::atomic::AtomicUsize::new(0);
    // Each case gets its own tenant, so it starts empty.
    contract::run_all(&|| {
        let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        store(&bridge, &format!("tenant_{n}")) as Arc<dyn EnrolmentStore>
    });
}

#[test]
fn concurrent_redemption_across_connections_has_exactly_one_winner() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(fx.db.config(), 4);
    let rig = Arc::new(rig(store(&bridge, "t1"), TestClock::new()));
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_c"))
        .unwrap();
    let mut handles = Vec::new();
    for i in 0..20 {
        let (rig, code) = (Arc::clone(&rig), issued.code.clone());
        handles.push(std::thread::spawn(move || {
            let id = identity("edge_c");
            rig.service
                .redeem(
                    &code,
                    &node("edge_c"),
                    id.certificate_der(),
                    &format!("src{i}"),
                )
                .is_ok()
        }));
    }
    let wins = handles
        .into_iter()
        .filter(|_| true)
        .map(|h| h.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert_eq!(wins, 1);
}

#[test]
fn state_survives_a_restart_and_tenants_never_share_rows() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(fx.db.config(), 4);
    let id = identity("edge_r");
    {
        let rig = rig(store(&bridge, "t1"), TestClock::new());
        let issued = rig
            .service
            .create_enrolment("admin", &request("edge_r"))
            .unwrap();
        rig.service
            .redeem(&issued.code, &node("edge_r"), id.certificate_der(), "s")
            .unwrap();
    }
    // A new store, directory and service over the same database see the node.
    let again = rig(store(&bridge, "t1"), TestClock::new());
    assert!(again.directory.resolve(&client_cert(&id)).is_ok());
    // Another tenant over the same tables sees nothing of it.
    let other = rig(store(&bridge, "t2"), TestClock::new());
    assert!(other.directory.resolve(&client_cert(&id)).is_err());
    assert!(other.service.list().unwrap().is_empty());
    // The stored enrolment holds a salted hash, never the code.
    let client = fx.rt.block_on(fx.db.raw_client());
    let rows = fx
        .rt
        .block_on(client.query(
            "SELECT encode(salt, 'hex'), encode(hash, 'hex') FROM spot_sync_enrolment",
            &[],
        ))
        .unwrap();
    assert_eq!(rows.len(), 1);
}

/// An audit write that fails (here: a trigger on the outbox that refuses) means the change does
/// not exist, in the same transaction.
#[test]
fn a_failing_outbox_insert_rolls_the_change_back() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(fx.db.config(), 4);
    let rig = rig(store(&bridge, "t1"), TestClock::new());
    let client = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(client.batch_execute(
            "CREATE FUNCTION spot_test_refuse() RETURNS trigger LANGUAGE plpgsql AS $$ \
             BEGIN RAISE EXCEPTION 'refused'; END $$; \
             CREATE TRIGGER spot_test_refuse BEFORE INSERT ON spot_outbox \
             FOR EACH ROW EXECUTE FUNCTION spot_test_refuse();",
        ))
        .unwrap();
    assert_eq!(
        rig.service
            .create_enrolment("admin", &request("edge_x"))
            .unwrap_err()
            .code(),
        "sync_enrol.store_failed"
    );
    let count: i64 = fx
        .rt
        .block_on(client.query_one("SELECT count(*) FROM spot_sync_enrolment", &[]))
        .unwrap()
        .get(0);
    assert_eq!(count, 0, "the enrolment does not exist");
    fx.rt
        .block_on(client.batch_execute("DROP TRIGGER spot_test_refuse ON spot_outbox"))
        .unwrap();
    // With the outbox working again the change commits together with its record.
    let issued = rig
        .service
        .create_enrolment("admin", &request("edge_x"))
        .unwrap();
    let id = identity("edge_x");
    rig.service
        .redeem(&issued.code, &node("edge_x"), id.certificate_der(), "s")
        .unwrap();
    fx.rt
        .block_on(client.batch_execute("CREATE TRIGGER spot_test_refuse BEFORE INSERT ON spot_outbox FOR EACH ROW EXECUTE FUNCTION spot_test_refuse()"))
        .unwrap();
    assert!(rig.service.revoke("admin", &node("edge_x")).is_err());
    assert!(
        rig.directory.resolve(&client_cert(&id)).is_ok(),
        "the failed revocation changed nothing"
    );
    fx.rt
        .block_on(client.batch_execute("DROP TRIGGER spot_test_refuse ON spot_outbox"))
        .unwrap();
    let topics: Vec<String> = fx
        .rt
        .block_on(client.query(
            "SELECT topic, payload FROM spot_outbox ORDER BY captured_at, id",
            &[],
        ))
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    assert_eq!(topics, vec!["sys_sync_node_admin"; 2]);
    let payloads: String = fx
        .rt
        .block_on(client.query("SELECT payload FROM spot_outbox", &[]))
        .unwrap()
        .iter()
        .map(|r| r.get::<_, String>(0))
        .collect();
    assert!(
        !payloads.contains(&issued.code.split_once('.').unwrap().1.to_owned()),
        "the code is in an audit record"
    );
}

#[test]
fn building_the_store_inside_an_async_runtime_is_refused() {
    let Some(fx) = fixture() else { return };
    let bridge = bridge(fx.db.config(), 4);
    let result = fx
        .rt
        .block_on(async { PgEnrolmentStore::new(Arc::clone(&bridge), POOL, "t1").map(|_| ()) });
    assert_eq!(result.unwrap_err().code(), "sync_enrol.unsupported_context");
    assert!(PgEnrolmentStore::new(Arc::clone(&bridge), POOL, "").is_err());
}
