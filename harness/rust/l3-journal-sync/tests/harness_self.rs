//! The harness proves itself (WP-RTB-01.T1): every behaviour `src/` promises, checked against
//! the real mechanism (a real loopback proxy, a real `FolderStore`, a real `spot serve`
//! process where PostgreSQL is configured). No journal or sync business logic is exercised
//! here — that is explicitly out of scope for this task.
//!
//! Tests that need PostgreSQL skip with a printed reason when `SPOT_TEST_PG_URL` is unset,
//! following `spot_postgres::testing::TestDb::create`'s own convention: never faked, never
//! weakened.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::time::Duration;

use l3_journal_sync::{Cluster, FlakyStore, NodeSpec, Role, role_fragment, tamper, wait_until};
use spot_journal_store::{FolderStore, JournalStore, Layout, MemoryStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn spec(name: &str, role: Role) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role,
        extra_config_toml: String::new(),
    }
}

// ------------------------------------------------------------------- no PostgreSQL needed

#[test]
fn role_fragment_builds_one_fragment_per_role_in_one_place() {
    let standalone = role_fragment(&spec("solo", Role::Standalone));
    assert!(standalone.contains("[node]"), "{standalone}");
    assert!(standalone.contains("role = \"standalone\""), "{standalone}");

    let hub = role_fragment(&spec("center", Role::Hub));
    assert!(hub.contains("role = \"hub\""), "{hub}");

    let edge = role_fragment(&spec(
        "branch",
        Role::Edge {
            hub: "center".to_owned(),
        },
    ));
    assert!(edge.contains("role = \"edge\""), "{edge}");
    assert!(edge.contains("hub = \"center\""), "{edge}");

    // `extra_config_toml` is carried through verbatim, after the role lines.
    let mut with_extra = spec("solo", Role::Standalone);
    with_extra.extra_config_toml = "[services.events]\nenabled = true\n".to_owned();
    let fragment = role_fragment(&with_extra);
    assert!(fragment.contains("role = \"standalone\""), "{fragment}");
    assert!(fragment.contains("[services.events]"), "{fragment}");
    assert!(fragment.contains("enabled = true"), "{fragment}");
}

#[tokio::test]
async fn flaky_store_fails_exactly_as_configured() {
    // Fail the next two calls, then succeed.
    let counted = FlakyStore::new(MemoryStore::new(), 11).fail_next(2);
    let key = |n: u64| {
        Layout::tail_chunk("acme", 1, n)
            .expect("a valid key")
            .to_key()
    };
    assert!(counted.put_new(&key(1), b"a".to_vec()).await.is_err());
    assert!(counted.put_new(&key(2), b"b".to_vec()).await.is_err());
    assert!(counted.put_new(&key(3), b"c".to_vec()).await.is_ok());

    // Fail only keys under a given prefix.
    let prefixed = FlakyStore::new(MemoryStore::new(), 11).fail_prefix("acme/tail/");
    assert!(prefixed.put_new(&key(1), b"x".to_vec()).await.is_err());
    let elsewhere = Layout::tail_chunk("other-tenant", 1, 1)
        .expect("a valid key")
        .to_key();
    assert!(prefixed.put_new(&elsewhere, b"y".to_vec()).await.is_ok());

    // Fail a put whose payload is past the configured byte limit, succeed under it.
    let limited = FlakyStore::new(MemoryStore::new(), 11).fail_after_bytes(4);
    assert!(limited.put_new(&key(1), b"ab".to_vec()).await.is_ok());
    assert!(limited.put_new(&key(2), b"abcdef".to_vec()).await.is_err());
}

#[test]
fn tamper_flip_byte_changes_exactly_one_byte_of_a_real_store_object() {
    let dir = std::env::temp_dir().join(format!(
        "l3_journal_sync_harness_self_tamper_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = FolderStore::new(&dir).expect("open a folder store");
    let key = Layout::tail_chunk("acme", 1, 1)
        .expect("a valid key")
        .to_key();
    let original = b"the quick brown fox".to_vec();
    let meta = block_on(store.put_new(&key, original.clone())).expect("put the object");
    let path = dir.join(&key);

    tamper::flip_byte(&path, 4).expect("flip a byte");

    let after = std::fs::read(&path).expect("read the tampered object");
    assert_eq!(after.len(), original.len());
    let differences: Vec<usize> = (0..original.len())
        .filter(|&i| original[i] != after[i])
        .collect();
    assert_eq!(differences, vec![4], "exactly one byte must differ");
    // The store's own etag (computed from the tampered bytes) now disagrees with what it
    // reported on write, which is exactly what a recovery check reads as corruption.
    let new_meta = block_on(store.head(&key))
        .expect("head the tampered object")
        .expect("the object still exists");
    assert_ne!(
        new_meta.etag, meta.etag,
        "the etag must change with the bytes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tamper_truncate_and_delete_object_bypass_the_store_guards() {
    let dir = std::env::temp_dir().join(format!(
        "l3_journal_sync_harness_self_tamper_bypass_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = FolderStore::new(&dir).expect("open a folder store");
    // A segment: the guarded `delete` refuses it; the tamper helper does not.
    let segment = Layout::segment("acme", 1, 1, 10)
        .expect("a valid segment key")
        .to_key();
    block_on(store.put_new(&segment, b"sealed record".to_vec())).expect("put the segment");
    assert!(
        block_on(store.delete(&segment)).is_err(),
        "the guarded delete must refuse a segment"
    );
    assert!(
        tamper::delete_object(&dir, &segment).expect("tamper delete"),
        "the tamper helper must remove it anyway"
    );
    assert!(!dir.join(&segment).exists());

    let chunk = Layout::tail_chunk("acme", 1, 1)
        .expect("a valid key")
        .to_key();
    block_on(store.put_new(&chunk, b"0123456789".to_vec())).expect("put the chunk");
    tamper::truncate(&dir.join(&chunk), 4).expect("truncate");
    let shortened = std::fs::read(dir.join(&chunk)).expect("read the truncated object");
    assert_eq!(shortened, b"0123");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_loopback_proxy_blocks_bytes_both_ways_and_heal_restores_them() {
    // The same mechanism `Node::partition_from`/`Node::heal` use, proven directly: an echo
    // server behind the proxy, so both directions of one connection are exercised.
    let upstream = TcpListener::bind("127.0.0.1:0").await.expect("bind echo");
    let upstream_addr = upstream.local_addr().expect("echo addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = upstream.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if socket.write_all(&buffer[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    let proxy = l3_journal_sync::Proxy::spawn(upstream_addr).await;
    let mut client = TcpStream::connect(proxy.addr()).await.expect("connect");

    client.write_all(b"open").await.expect("write open");
    let mut buffer = [0u8; 4];
    client.read_exact(&mut buffer).await.expect("read open");
    assert_eq!(&buffer, b"open");

    proxy.pause();
    tokio::time::sleep(Duration::from_millis(80)).await;
    client.write_all(b"stuck").await.expect("write stuck");
    let mut probe = [0u8; 1];
    let blocked = tokio::time::timeout(Duration::from_millis(300), client.read(&mut probe)).await;
    assert!(
        blocked.is_err(),
        "bytes crossed a paused proxy: {blocked:?}"
    );

    proxy.resume();
    let mut buffer = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut buffer))
        .await
        .expect("timed out reading after heal")
        .expect("read after heal");
    assert_eq!(&buffer, b"stuck");
}

/// Drives a boxed store future to completion on the current thread's runtime, for the
/// synchronous `#[test]`s above that only need one or two calls.
fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a current-thread runtime")
        .block_on(future)
}

// ----------------------------------------------------------------------- needs PostgreSQL

/// Prints the skip reason and returns `true` when there is nothing to test here, following
/// `spot_postgres::testing::TestDb::create`'s own convention: never faked.
fn skip_without_pg(test: &str) -> bool {
    if std::env::var("SPOT_TEST_PG_URL").is_err() {
        println!("SKIPPED: {test}: set SPOT_TEST_PG_URL (see spot_postgres::testing::TestDb)");
        true
    } else {
        false
    }
}

#[tokio::test]
async fn a_node_starts_and_answers_health_ready() {
    if skip_without_pg("a_node_starts_and_answers_health_ready") {
        return;
    }
    let mut cluster = Cluster::new();
    let Some(node) = cluster.add_node(spec("solo", Role::Standalone)).await else {
        println!("SKIPPED: a_node_starts_and_answers_health_ready: no PostgreSQL available");
        return;
    };
    assert!(node.is_running());
    let client = node.client().expect("the node signed its administrator in");
    let reply = client.health("ready").await.expect("health request");
    assert_eq!(reply.status, 200, "{}", reply.text());
}

#[tokio::test]
async fn kill_and_start_keeps_its_data() {
    // What this proves: an un-awaited stop (`Node::kill`, see its doc comment for the honest
    // limits of that name) followed by `Node::start` brings the same node back with the same
    // data. It does NOT prove a crash was survived mid-request — see
    // `stop_closes_the_old_address_but_kill_does_not_wait_to` for the one real, provable
    // difference between `kill` and `stop` this harness can demonstrate today.
    if skip_without_pg("kill_and_start_keeps_its_data") {
        return;
    }
    let mut cluster = Cluster::new();
    if cluster
        .add_node(spec("solo", Role::Standalone))
        .await
        .is_none()
    {
        println!("SKIPPED: kill_and_start_keeps_its_data: no PostgreSQL available");
        return;
    }
    let ids = l3_journal_sync::seed::documents(cluster.node("solo"), 2)
        .await
        .expect("seed documents");
    assert_eq!(ids.len(), 2);

    let node = cluster.node_mut("solo");
    node.kill();
    assert!(!node.is_running());
    node.start().await;
    assert!(node.is_running());

    let client = node.client().expect("signed back in after start");
    for id in &ids {
        let reply = client
            .get(&format!("/api/v1/resource/project/{id}"))
            .await
            .expect("read the document back");
        assert_eq!(
            reply.status,
            200,
            "document {id} did not survive kill+start: {}",
            reply.text()
        );
    }
}

#[tokio::test]
async fn stop_closes_the_old_address_but_kill_does_not_wait_to() {
    if skip_without_pg("stop_closes_the_old_address_but_kill_does_not_wait_to") {
        return;
    }
    let mut cluster = Cluster::new();
    if cluster
        .add_node(spec("solo", Role::Standalone))
        .await
        .is_none()
    {
        println!(
            "SKIPPED: stop_closes_the_old_address_but_kill_does_not_wait_to: \
             no PostgreSQL available"
        );
        return;
    }

    // `Node::stop` is awaited: by the time it returns, the host's serving loop has actually
    // ended, so a fresh connection to the address it bound is refused. This is the one real,
    // provable difference from `Node::kill` (see its doc comment): `kill` returns before any
    // such guarantee can hold, because there is nothing for the caller to await.
    let node = cluster.node_mut("solo");
    let addr = node
        .api_addr()
        .expect("the node is running and has an address");
    node.stop().await;
    let refused = TcpStream::connect(addr).await;
    assert!(
        refused.is_err(),
        "a connection to {addr} succeeded after an awaited `stop`: {refused:?}"
    );

    // `kill` is a plain, non-async call: the signature itself is proof a caller cannot await
    // its drain the way `stop`'s caller can. Starting straight back up with no wait is still
    // safe (see `kill_and_start_keeps_its_data` for the data side of that); this test does
    // not assert whether the pre-kill address still answers in the background, because that
    // would be a flaky assertion on an unawaited drain's timing — exactly the documented gap
    // in `Node::kill`'s doc comment and this task's `## Open questions`.
    node.start().await;
    node.kill();
    assert!(!node.is_running());
    node.start().await;
    assert!(node.is_running());
}

#[tokio::test]
async fn partition_from_blocks_sync_traffic_both_ways_and_heal_restores_it() {
    if skip_without_pg("partition_from_blocks_sync_traffic_both_ways_and_heal_restores_it") {
        return;
    }
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("hub", Role::Hub)).await.is_none() {
        println!(
            "SKIPPED: partition_from_blocks_sync_traffic_both_ways_and_heal_restores_it: \
             no PostgreSQL available"
        );
        return;
    }
    let edge_spec = spec(
        "edge",
        Role::Edge {
            hub: "hub".to_owned(),
        },
    );
    if cluster.add_node(edge_spec).await.is_none() {
        println!(
            "SKIPPED: partition_from_blocks_sync_traffic_both_ways_and_heal_restores_it: \
             no PostgreSQL available for the second node"
        );
        return;
    }

    let hub_sync = cluster.node("hub").sync_addr_for("edge").await;
    let mut client = TcpStream::connect(hub_sync)
        .await
        .expect("dial the hub's sync address");
    client.write_all(b"before").await.expect("write before");
    let mut buffer = [0u8; 6];
    client.read_exact(&mut buffer).await.expect("read before");
    assert_eq!(&buffer, b"before");

    cluster
        .node("hub")
        .partition_from(cluster.node("edge"))
        .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    client.write_all(b"during").await.expect("write during");
    let mut probe = [0u8; 1];
    let blocked = tokio::time::timeout(Duration::from_millis(300), client.read(&mut probe)).await;
    assert!(
        blocked.is_err(),
        "bytes crossed a partitioned node: {blocked:?}"
    );

    cluster.node("hub").heal(cluster.node("edge"));
    let mut buffer = [0u8; 6];
    tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut buffer))
        .await
        .expect("timed out after heal")
        .expect("read after heal");
    assert_eq!(&buffer, b"during");
}

#[tokio::test]
async fn partitioning_two_nodes_leaves_a_third_nodes_traffic_flowing() {
    if skip_without_pg("partitioning_two_nodes_leaves_a_third_nodes_traffic_flowing") {
        return;
    }
    // The pairwise fix (lead's review #2 of this PR): a 3-node cluster, hub + two edges;
    // partitioning the hub from one edge must not touch the hub's traffic with the other.
    let mut cluster = Cluster::new();
    if cluster.add_node(spec("hub", Role::Hub)).await.is_none() {
        println!(
            "SKIPPED: partitioning_two_nodes_leaves_a_third_nodes_traffic_flowing: \
             no PostgreSQL available"
        );
        return;
    }
    for name in ["edge-a", "edge-b"] {
        let edge_spec = spec(
            name,
            Role::Edge {
                hub: "hub".to_owned(),
            },
        );
        if cluster.add_node(edge_spec).await.is_none() {
            println!(
                "SKIPPED: partitioning_two_nodes_leaves_a_third_nodes_traffic_flowing: \
                 no PostgreSQL available for `{name}`"
            );
            return;
        }
    }

    let hub_to_a = cluster.node("hub").sync_addr_for("edge-a").await;
    let hub_to_b = cluster.node("hub").sync_addr_for("edge-b").await;
    let mut client_a = TcpStream::connect(hub_to_a)
        .await
        .expect("dial the hub's proxy for edge-a");
    let mut client_b = TcpStream::connect(hub_to_b)
        .await
        .expect("dial the hub's proxy for edge-b");

    cluster
        .node("hub")
        .partition_from(cluster.node("edge-a"))
        .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    // The partitioned pair (hub, edge-a) is blocked.
    client_a.write_all(b"stuck").await.expect("write stuck");
    let mut probe = [0u8; 1];
    let blocked_a =
        tokio::time::timeout(Duration::from_millis(300), client_a.read(&mut probe)).await;
    assert!(
        blocked_a.is_err(),
        "bytes crossed the partitioned pair (hub, edge-a): {blocked_a:?}"
    );

    // The unrelated pair (hub, edge-b) still flows, proving the partition is pairwise.
    client_b.write_all(b"flows").await.expect("write flows");
    let mut buffer = [0u8; 5];
    tokio::time::timeout(Duration::from_millis(500), client_b.read_exact(&mut buffer))
        .await
        .expect("the uninvolved pair (hub, edge-b) must not be blocked")
        .expect("read from the uninvolved pair");
    assert_eq!(&buffer, b"flows");

    cluster.node("hub").heal(cluster.node("edge-a"));
}

#[tokio::test]
async fn two_nodes_have_distinct_databases() {
    if skip_without_pg("two_nodes_have_distinct_databases") {
        return;
    }
    let mut cluster = Cluster::new();
    if cluster
        .add_node(spec("a", Role::Standalone))
        .await
        .is_none()
    {
        println!("SKIPPED: two_nodes_have_distinct_databases: no PostgreSQL available");
        return;
    }
    if cluster
        .add_node(spec("b", Role::Standalone))
        .await
        .is_none()
    {
        println!(
            "SKIPPED: two_nodes_have_distinct_databases: no PostgreSQL available for the \
             second node"
        );
        return;
    }
    let names: BTreeSet<&str> = cluster
        .nodes()
        .iter()
        .map(l3_journal_sync::Node::db_name)
        .collect();
    assert_eq!(names.len(), 2, "the two nodes must not share a database");
}

#[tokio::test]
async fn fingerprint_is_stable_and_changes_with_the_data() {
    if skip_without_pg("fingerprint_is_stable_and_changes_with_the_data") {
        return;
    }
    let mut cluster = Cluster::new();
    if cluster
        .add_node(spec("solo", Role::Standalone))
        .await
        .is_none()
    {
        println!(
            "SKIPPED: fingerprint_is_stable_and_changes_with_the_data: no PostgreSQL available"
        );
        return;
    }
    // The running host's own services (the scheduler, the metadata poller) write to its
    // database on their own schedule, so two fingerprints taken while it serves are not
    // guaranteed to agree even with no caller activity between them. Stop it first: the
    // fingerprint still reads the database directly (a plain driver connection, independent
    // of the host), and nothing is writing to it while it is stopped.
    cluster.node_mut("solo").stop().await;
    let before = l3_journal_sync::fingerprint(cluster.node("solo"))
        .await
        .expect("fingerprint before seeding");
    let again = l3_journal_sync::fingerprint(cluster.node("solo"))
        .await
        .expect("fingerprint again, nothing changed");
    assert_eq!(before, again, "the same data must fingerprint the same way");

    cluster.node_mut("solo").start().await;
    l3_journal_sync::seed::documents(cluster.node("solo"), 1)
        .await
        .expect("seed one document");
    cluster.node_mut("solo").stop().await;
    let after = l3_journal_sync::fingerprint(cluster.node("solo"))
        .await
        .expect("fingerprint after seeding");
    assert_ne!(before, after, "new data must change the fingerprint");
}

#[tokio::test]
async fn read_conditions_and_read_audit_are_empty_before_the_tables_exist() {
    if skip_without_pg("read_conditions_and_read_audit_are_empty_before_the_tables_exist") {
        return;
    }
    let mut cluster = Cluster::new();
    if cluster
        .add_node(spec("solo", Role::Standalone))
        .await
        .is_none()
    {
        println!(
            "SKIPPED: read_conditions_and_read_audit_are_empty_before_the_tables_exist: \
             no PostgreSQL available"
        );
        return;
    }
    let node = cluster.node("solo");
    // No journal or sync behaviour exists on `dev` yet (WP-RTB-01.T1 is pure test
    // infrastructure), so neither table exists: the helpers read that as "nothing yet", not
    // as an error.
    let conditions = l3_journal_sync::read_conditions(node)
        .await
        .expect("read conditions");
    assert!(conditions.is_empty());
    let audit = l3_journal_sync::read_audit(node, "journal.sealed")
        .await
        .expect("read audit");
    assert!(audit.is_empty());
}

#[tokio::test]
async fn wait_until_waits_at_most_the_given_timeout() {
    // No PostgreSQL needed, but kept next to the PostgreSQL tests: it is the helper every one
    // of them calls to reach readiness.
    let started = std::time::Instant::now();
    let result = wait_until(Duration::from_millis(200), || async { false }).await;
    assert!(
        result.is_err(),
        "a condition that never holds must time out"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "must not return before the timeout"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "must not overshoot the timeout by much"
    );

    let mut calls = 0;
    let result = wait_until(Duration::from_secs(5), || {
        calls += 1;
        async move { calls >= 3 }
    })
    .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn teardown_leaves_no_database_or_directory_behind() {
    if skip_without_pg("teardown_leaves_no_database_or_directory_behind") {
        return;
    }
    let (db_name, root) = {
        let mut cluster = Cluster::new();
        let Some(node) = cluster.add_node(spec("solo", Role::Standalone)).await else {
            println!(
                "SKIPPED: teardown_leaves_no_database_or_directory_behind: no PostgreSQL available"
            );
            return;
        };
        (node.db_name().to_owned(), node.root().to_path_buf())
    };
    // `cluster` is dropped: the node's own `Drop` should have removed both by now.
    assert!(
        !root.exists(),
        "the node's root {} was not removed",
        root.display()
    );

    // The same sweep `TestDb::create` runs checks `pg_database` directly; reuse a throwaway
    // admin connection the same way the rest of this harness does.
    let Some(scratch) = spot_postgres::testing::TestDb::create().await else {
        println!(
            "SKIPPED: teardown_leaves_no_database_or_directory_behind: lost PostgreSQL mid-test"
        );
        return;
    };
    let admin = scratch.admin_client().await;
    let rows = admin
        .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&db_name])
        .await
        .expect("query pg_database");
    assert!(
        rows.is_empty(),
        "the node's database {db_name} was not dropped"
    );
}

#[tokio::test]
async fn a_failed_start_removes_its_root() {
    if skip_without_pg("a_failed_start_removes_its_root") {
        return;
    }
    let bad_repo = std::env::temp_dir().join(format!(
        "l3_journal_sync_harness_self_bad_repo_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&bad_repo);
    std::fs::create_dir_all(&bad_repo).expect("create the repo stand-in");
    // No `apps/sample` under this "repository": `Node::create` panics after the root exists,
    // which must unwind through the root guard.

    let before = list_l3js_roots();
    let node_spec = spec("doomed", Role::Standalone);
    let repo_for_task = bad_repo.clone();
    let outcome =
        tokio::spawn(async move { l3_journal_sync::Node::create(&repo_for_task, node_spec).await })
            .await;
    assert!(
        outcome.is_err(),
        "the start should have panicked on the missing apps/sample directory"
    );
    let after = list_l3js_roots();
    // Only this test's own node name (`doomed`, unique to this test) distinguishes its root
    // from one a concurrently running test legitimately creates in the same process: a plain
    // before/after diff of every `l3_journal_sync_*` root is racy against the other tests in
    // this binary, which is exactly what made this test flaky under the default (parallel)
    // test runner before this fix.
    let leaked: Vec<&String> = after
        .difference(&before)
        .filter(|name| name.contains("_doomed_"))
        .collect();
    assert!(
        leaked.is_empty(),
        "a failed start left a root behind: {leaked:?}"
    );

    let _ = std::fs::remove_dir_all(&bad_repo);
}

fn list_l3js_roots() -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return set;
    };
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str()
            && name.starts_with("l3_journal_sync_")
            && !name.contains("harness_self")
        {
            set.insert(name.to_owned());
        }
    }
    set
}
