//! The hosted journal audit's footprint on real nodes (WP-RTB-01.T7).
//!
//! What is proved today: two nodes each have their own, empty `spot_journal_audit_result` table
//! (created by `spot migrate`, never by the service) and no scratch database is left on the
//! server. The audit itself (rebuild into a scratch database, mismatch detection, the recovery
//! report) needs the journal service's `AuditWiring` port (WP-RTB-01.T3/T5); the database parts
//! are proved in `crates/backends/spot-journal-audit-host-pg/tests/audit_host.rs`, and the
//! rebuild and compare in `spot-journal-audit-run`'s own suite. When the port lands this file
//! gains the end-to-end cases (clean audit marks the snapshot verified, a direct SQL update of one
//! live row raises `JOURNAL_AUDIT_MISMATCH` naming that record, two sites audited in one run never
//! see each other's scratch) using the tamper helpers.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_journal_sync::{Cluster, NodeSpec, Role};
use spot_postgres::{PgBackend, PgConfig};

fn spec(name: &str) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_nodes_have_their_own_empty_result_tables_and_no_leaked_scratch_database() {
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("one")).await.is_none()
        || cluster.add_node(spec("two")).await.is_none()
    {
        println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
        return;
    }
    for name in ["one", "two"] {
        let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
        let config = PgConfig::from_url(&url)
            .unwrap()
            .with_dbname(cluster.node(name).db_name());
        let backend = PgBackend::connect_unchecked(config).unwrap();
        let client = backend.pool().get().await.unwrap();
        let rows = client
            .query_one("SELECT count(*) FROM spot_journal_audit_result", &[])
            .await
            .unwrap_or_else(|e| panic!("{name}: the result table is missing: {e}"));
        assert_eq!(rows.get::<_, i64>(0), 0, "{name}: nothing audited yet");
        let leaked = client
            .query_one(
                "SELECT count(*) FROM pg_database WHERE datname LIKE 'spot\\_audit\\_%'",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            leaked.get::<_, i64>(0),
            0,
            "no scratch database outlives a run"
        );
    }
}
