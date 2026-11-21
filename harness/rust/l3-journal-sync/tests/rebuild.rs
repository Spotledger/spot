//! The rebuild's feed adoption on a real node's database (WP-RTB-01.T6).
//!
//! What is proved today: on the database of a stopped `spot serve` node, `adopt_position` moves
//! the outbox head forward and refuses to move it back, and `open_generation` opens a fork's new
//! generation at zero. The full rebuild (drop the database, rebuild from the journal and the
//! snapshot, equal fingerprint, a consumer bookmark still valid, roll-forward, point in time,
//! interrupted at three points, tampered/missing/wrong-tenant journal, erased subject, running
//! site refused) needs the journal service's store, keys and snapshot service (WP-RTB-01.T3 and
//! T5) behind the host's `Deps`; the orchestration is proved over fakes in
//! `crates/framework/spot-journal-rebuild-host/tests/rebuild.rs` and the adoption itself against
//! PostgreSQL in `crates/backends/spot-postgres/tests/feed_adopt.rs`.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_journal_sync::{Cluster, NodeSpec, Role};
use spot_postgres::{PgBackend, PgConfig, adopt_position, open_generation};

fn spec(name: &str) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role: Role::Standalone,
        extra_config_toml: String::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adoption_moves_the_head_forward_only_and_a_fork_opens_at_zero() {
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("solo")).await.is_none() {
        println!("SKIPPED: no PostgreSQL available (set SPOT_TEST_PG_URL)");
        return;
    }
    // A rebuild is offline: the site is stopped while the feed is touched.
    cluster.node_mut("solo").stop().await;
    let node = cluster.node("solo");
    let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
    let config = PgConfig::from_url(&url)
        .unwrap()
        .with_dbname(node.db_name());
    let backend = PgBackend::connect_unchecked(config).unwrap();

    adopt_position(&backend, 1, 41).await.unwrap();
    let client = backend.pool().get().await.unwrap();
    let head = |sql: &'static str| {
        let client = &client;
        async move { client.query_one(sql, &[]).await.unwrap().get::<_, i64>(0) }
    };
    assert_eq!(
        head("SELECT last_seq FROM spot_feed_state WHERE log = 'outbox'").await,
        41
    );
    // Never backwards.
    assert!(adopt_position(&backend, 1, 10).await.is_err());
    assert_eq!(
        head("SELECT last_seq FROM spot_feed_state WHERE log = 'outbox'").await,
        41
    );
    // A fork opens the next generation at zero.
    open_generation(&backend, 2).await.unwrap();
    assert_eq!(
        head("SELECT value::bigint FROM spot_meta WHERE key = 'feed_generation'").await,
        2
    );
    assert_eq!(
        head("SELECT last_seq FROM spot_feed_state WHERE log = 'outbox'").await,
        0
    );
    assert!(open_generation(&backend, 2).await.is_err());
}
