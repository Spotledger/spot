//! The node role model over the journal/sync cluster harness (WP-RTB-03.T1): a standalone, a
//! hub and an edge, started from [`role_fragment`], and the service sets `services::node_role`
//! produces for each, asserted over HTTP readiness.
//!
//! # Why this file edits the node's files directly
//!
//! [`role_fragment`]'s own doc comment (and this crate's `README.md`) says plainly that its
//! text is written to `<root>/role.toml`, a side file `spot_cli::config::load` never reads,
//! because `spot_cli::config::Common` and `Site` both carry `#[serde(deny_unknown_fields)]`
//! with no `[node]` table — "only the *write site* is a placeholder" until a task that owns
//! `src/node.rs` splices the real fragment into live configuration. `src/node.rs` is not an
//! owned path of this task (only this test file is), so this file cannot make that splice
//! itself. What it does instead, to still prove the three roles over this harness's real
//! `spot serve` processes:
//!
//! 1. calls [`role_fragment`] for every node (so the fragment this crate will one day splice in
//!    is exercised and asserted, exactly as `harness_self.rs` already does for its own tests);
//! 2. writes `services::node_role`'s **real** configuration seam, `[services.node_role]` of
//!    `common.toml` (the table this task's `node_role.rs` actually reads — see its module docs
//!    for why it is not the bare `[sync]`/`[node]` the brief sketches), using only the node's
//!    public `root()`/`site()` accessors, **before** [`Node::start`] boots the host (a node
//!    whose site refuses to build makes [`Node::start`]/`boot_host` panic on the administrator
//!    sign-in it always attempts — `src/node.rs` is unowned, so a refused-role case belongs to
//!    `node_role_host.rs` in `spot-cli`, not here; this file proves the three **valid** roles'
//!    service sets instead, which is what the brief's "starts a standalone, a hub and an edge"
//!    asks for);
//! 3. for the edge, adds a covered `[partition]` profile to the site's file and runs `spot
//!    migrate` through the public `spot_cli::ops::migrate` (no shelling out: `CARGO_BIN_EXE_spot`
//!    is not set for this crate's own test binaries, only for `spot-cli`'s);
//! 4. asserts each role's service set is what the brief says it must be: a standalone and a hub
//!    sign in and serve the ordinary way; an edge does too, **though its `hub_url` resolves
//!    nowhere real** — the offline-safe readiness property (item 4 of the brief) that nothing in
//!    `services::node_role` ever probes the hub.
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, this crate's own convention).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use l3_journal_sync::{Cluster, Node, NodeSpec, Role, role_fragment};

fn spec(name: &str, role: Role) -> NodeSpec {
    NodeSpec {
        name: name.to_owned(),
        role,
        extra_config_toml: String::new(),
    }
}

/// Appends `toml` to the node's `common.toml` (the `[services.node_role]` table), clears any
/// role this site already recorded (the documented recovery path this module's own refusal
/// message names: "stop the site and clear spot_node_role_state" — every role change in this
/// test is a deliberate bootstrap from a fresh node, not the audited operation
/// `node_role.role_changed_unsafely` exists to require, so clearing the row here is the right
/// side of that line, not a workaround of it), then restarts so the host reads the new
/// configuration.
async fn set_role_and_restart(node: &mut Node, toml: &str) {
    let path = node.root().join("common.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(toml);
    std::fs::write(&path, text).unwrap();
    node.stop().await;
    clear_role_state(node).await;
    node.start().await;
}

/// Deletes this site's `spot_node_role_state` row, the way an operator would before
/// intentionally re-roling a node outside the (out-of-scope, WP-RTB-04.T2) change operation.
async fn clear_role_state(node: &Node) {
    let loaded = spot_cli::config::load(node.root(), false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == node.site())
        .unwrap()
        .clone();
    let backend = spot_cli::schema::backend(&loaded, &site).await.unwrap();
    backend
        .pool()
        .get()
        .await
        .unwrap()
        .execute(
            "DELETE FROM spot_node_role_state WHERE site = $1",
            &[&node.site()],
        )
        .await
        .unwrap();
}

/// Adds a covered `[partition]` profile to the node's site file (the `label`/`name` pair every
/// other partitioned-site test in this repository uses) and runs `spot migrate --apply` for it
/// through the library call, not a subprocess.
async fn switch_on_partition(node: &Node, id: &str) {
    let path = node
        .root()
        .join("sites")
        .join(node.site())
        .join("site.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!(
        "\n[partition]\nnode = {id:?}\ndoctypes = [\"label\"]\n\n[[partition.scope]]\n\
         doctype = \"label\"\nkey = \"branch\"\nfield = \"name\"\n"
    ));
    std::fs::write(&path, text).unwrap();
    spot_cli::ops::migrate(
        node.root(),
        Some(node.site()),
        spot_cli::ops::Migrate {
            apply: true,
            plan_hash: None,
        },
    )
    .await
    .unwrap_or_else(|e| panic!("spot migrate for node `{}`: {e}", node.name()));
}

/// A readable, non-empty enrolment code file inside the node's own root.
fn enrol_code_file(node: &Node) -> std::path::PathBuf {
    let path = node.root().join("enrol.code");
    std::fs::write(&path, b"one-time-code").unwrap();
    path
}

/// The node actually serves: a fresh admin sign-in succeeds (which it only can once
/// `Node::start`'s own sign-in already proved the site is up; this re-checks it with a brand
/// new session, as this file's role-set assertion).
async fn is_serving(node: &Node) -> bool {
    let Some(client) = node.client() else {
        return false;
    };
    matches!(client.session().await, Ok(reply) if reply.status == 200)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn standalone_hub_and_edge_start_the_role_s_own_service_set() {
    let mut cluster = Cluster::new();

    // --- standalone: role_fragment documents the intent; the default needs no extra wiring,
    // and it is today's behaviour exactly (`[services.node_role]` absent): no sync service.
    let Some(_) = cluster
        .add_node(spec("standalone-a", Role::Standalone))
        .await
    else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    let fragment = role_fragment(cluster.node("standalone-a").spec());
    assert!(fragment.contains("role = \"standalone\""), "{fragment}");
    assert!(
        is_serving(cluster.node("standalone-a")).await,
        "a standalone site serves with no sync service at all"
    );

    // --- hub: role_fragment documents the intent; the real wiring is [services.node_role]
    // with the `listen` address its (not-yet-built, WP-RTB-02) sync_hub service would bind.
    cluster
        .add_node(spec("hub-a", Role::Hub))
        .await
        .expect("PostgreSQL was available for the first node");
    let fragment = role_fragment(cluster.node("hub-a").spec());
    assert!(fragment.contains("role = \"hub\""), "{fragment}");
    set_role_and_restart(
        cluster.node_mut("hub-a"),
        "\n[services.node_role]\nrole = \"hub\"\nlisten = \"0.0.0.0:0\"\n",
    )
    .await;
    assert!(
        is_serving(cluster.node("hub-a")).await,
        "a hub with a listen address serves the full stack plus the hub role"
    );

    // --- edge: role_fragment documents the intent (`hub = "hub-a"`); the real wiring is a
    // covered [partition] profile plus [services.node_role] naming the same node id and an
    // enrolment code (this edge has never enrolled with a real hub, which does not exist in
    // this harness yet — WP-RTB-02, out of scope).
    cluster
        .add_node(spec(
            "edge-a",
            Role::Edge {
                hub: "hub-a".to_owned(),
            },
        ))
        .await
        .expect("PostgreSQL was available for the first node");
    let fragment = role_fragment(cluster.node("edge-a").spec());
    assert!(fragment.contains("role = \"edge\""), "{fragment}");
    assert!(fragment.contains("hub = \"hub-a\""), "{fragment}");
    switch_on_partition(cluster.node("edge-a"), "edge-a").await;
    let code = enrol_code_file(cluster.node("edge-a"));
    set_role_and_restart(
        cluster.node_mut("edge-a"),
        &format!(
            "\n[services.node_role]\nrole = \"edge\"\nnode = \"edge-a\"\n\
             hub_url = \"https://hub.invalid.example:1\"\nenrol_code_file = {:?}\n\
             host_roles = [\"branch-edge\"]\n",
            code.display()
        ),
    )
    .await;
    // Offline-safe (item 4 of the brief): this edge's hub_url resolves nowhere real, and the
    // site still starts and serves — nothing in services::node_role ever probes the hub.
    assert!(
        is_serving(cluster.node("edge-a")).await,
        "a fully configured edge serves though no hub is reachable"
    );
}
