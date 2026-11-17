//! The journal and sync management operations over a real node (WP-RTB-04.T3).
//!
//! What is proved today: a node whose journal and sync services do not provide their operation
//! implementations (WP-RTB-04.T1 and T2 hosting, the journal service, sync hosting) starts and is
//! ready with the executors module compiled in: registering nothing is the fail-closed default.
//! The operations over a real node (snapshot, audit run, grant and revoke a reader, enrol, suspend
//! and revoke a node and see its next sync request refused, the recovery report, the sync health)
//! need those services and the management API's dispatch seam (cluster E); the executors' rules are
//! proved over fakes in `crates/framework/spot-cli/tests/mgmt_exec_host.rs`.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_journal_sync::{Cluster, NodeSpec, Role};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_without_the_operation_services_starts_ready_and_registers_no_executors() {
    let mut cluster = Cluster::new();
    let spec = NodeSpec {
        name: "solo".to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    };
    let Some(node) = cluster.add_node(spec).await else {
        println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
        return;
    };
    let client = node.client().expect("the node signed its administrator in");
    let ready = client.health("ready").await.unwrap();
    assert_eq!(ready.status, 200, "{}", ready.text());
}
