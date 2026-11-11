//! The node role model hosted (WP-RTB-03.T1): `[services.node_role]` read over a real
//! `spot serve` host.
//!
//! The host never refuses to **start the process** on a bad site configuration (that is
//! `host.rs`'s existing design, unowned by this task: a bad site is marked `Unavailable` and
//! answers `503` with a generic body, while the host keeps serving every other site). A valid
//! role configuration therefore starts the site and it answers `/api/health/ready` with `200`;
//! an invalid one leaves the site `503` and starts nothing for it (no listener route opens, no
//! job loop, no write) — and `host.rs`'s own `unavailable()` deliberately never echoes the
//! configuration error to an unauthenticated caller ("the reason a site is unavailable ... never
//! reaches an unauthenticated client"), so this file proves the refusal and the stable code from
//! two angles: the HTTP-visible `503` here, and the exact code string asserted by
//! `services::node_role`'s own `decide()` unit tests (in the module itself, no database needed).
//!
//! Real PostgreSQL (skipped without `SPOT_TEST_PG_URL`, the convention every other
//! `*_pg.rs`/`*_host.rs` file here follows).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, dead_code)]

mod common;

use common::{Env, Started};

/// This file's node id (equal to `[partition] node` in every edge test).
const NODE: &str = "edge-a";

/// Rewrites `common.toml`'s `[services.node_role]` table.
fn set_role(env: &Env, toml: &str) {
    env.write_common(&["'sample'"], toml);
}

/// Adds a covered `[partition]` profile to the site's file (the `label`/`name` pair
/// `participants_pg.rs` uses, so the owner check — which runs before this module — never
/// refuses first and masks what this file means to prove) and creates its tables.
fn switch_on_partition(env: &Env, node: &str) {
    let path = env.site_dir().join("site.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(&format!(
        "\n[partition]\nnode = {node:?}\ndoctypes = [\"label\"]\n\n[[partition.scope]]\n\
         doctype = \"label\"\nkey = \"branch\"\nfield = \"name\"\n"
    ));
    std::fs::write(&path, text).unwrap();
    common::ok(&env.root.0, &["migrate", "--site", &env.site, "--apply"]);
}

/// A readable, non-empty enrolment code file inside the site's own folder.
fn enrol_code_file(env: &Env) -> std::path::PathBuf {
    let path = env.site_dir().join("enrol.code");
    std::fs::write(&path, b"one-time-code").unwrap();
    path
}

/// Starts the host (it always starts; a bad site becomes `Unavailable`, not a process error)
/// and asks this site something that goes through the per-site dispatcher (`POST
/// /api/v1/auth/login`, open without a session): `503 site_unavailable` for a refused role,
/// the ordinary sign-in answer (`401`, a wrong password) for a site whose role started. The
/// global `/api/health/ready` cannot distinguish the two — `host.rs`'s own readiness list
/// simply omits a site whose build failed, so the host-wide answer stays `200` either way.
async fn site_status(env: &Env) -> (u16, Started) {
    let started = common::start(env).await;
    let reply = common::login(started.addr, &env.site, "administrator", "wrong").await;
    (reply.status, started)
}

// --- standalone ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn standalone_is_the_default_and_starts_clean() {
    let Some(env) = Env::new("").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    let (status, started) = site_status(&env).await;
    assert_ne!(status, 503, "the site must have started");
    started.finish().await;
}

// --- hub -----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_without_listen_is_refused_and_starts_nothing() {
    let Some(env) = Env::new("[services.node_role]\nrole = \"hub\"\n").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_with_listen_starts_and_is_ready() {
    let Some(env) =
        Env::new("[services.node_role]\nrole = \"hub\"\nlisten = \"0.0.0.0:17443\"\n").await
    else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    let (status, started) = site_status(&env).await;
    assert_ne!(status, 503, "the site must have started");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_cannot_declare_an_edge_setting() {
    let Some(env) = Env::new(
        "[services.node_role]\nrole = \"hub\"\nlisten = \"0.0.0.0:17444\"\n\
         hub_url = \"https://hub.example\"\n",
    )
    .await
    else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

// --- edge -----------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_without_partition_is_refused() {
    let Some(env) = Env::new(&format!(
        "[services.node_role]\nrole = \"edge\"\nhost_roles = [\"branch-edge\"]\nnode = {NODE:?}\n\
         hub_url = \"https://hub.example\"\n"
    ))
    .await
    else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_without_a_hub_url_is_refused() {
    let Some(env) = Env::new(&format!(
        "[services.node_role]\nrole = \"edge\"\nhost_roles = [\"branch-edge\"]\nnode = {NODE:?}\n"
    ))
    .await
    else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    switch_on_partition(&env, NODE);
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unenrolled_edge_with_no_code_file_is_refused() {
    let Some(env) = Env::new(&format!(
        "[services.node_role]\nrole = \"edge\"\nhost_roles = [\"branch-edge\"]\nnode = {NODE:?}\n\
         hub_url = \"https://hub.example\"\n"
    ))
    .await
    else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    switch_on_partition(&env, NODE);
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_id_that_disagrees_with_the_partition_is_refused() {
    let Some(env) = Env::new(
        "[services.node_role]\nrole = \"edge\"\nhost_roles = [\"branch-edge\"]\nnode = \"some-other-node\"\n\
         hub_url = \"https://hub.example\"\n",
    )
    .await
    else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    switch_on_partition(&env, NODE);
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fully_configured_edge_starts_and_is_ready_though_no_hub_is_reachable() {
    let Some(env) = Env::new("").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    switch_on_partition(&env, NODE);
    let code = enrol_code_file(&env);
    set_role(
        &env,
        &format!(
            "[services.node_role]\nrole = \"edge\"\nhost_roles = [\"branch-edge\"]\nnode = {NODE:?}\n\
             hub_url = \"https://hub.invalid.example:1\"\nenrol_code_file = {:?}\n",
            code.display()
        ),
    );
    // Offline-safe readiness (item 4 of the brief): this module never probes the hub, so an
    // edge with an unreachable hub_url is ready for exactly the reasons a standalone or hub
    // site is — the structural property this task owns; the real hub-reachability report
    // belongs to services.sync_node (WP-RTB-02, out of scope).
    let (status, started) = site_status(&env).await;
    assert_ne!(status, 503, "the site must have started");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_whose_host_role_set_is_unknown_is_refused() {
    let Some(env) = Env::new("").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    switch_on_partition(&env, NODE);
    let code = enrol_code_file(&env);
    // No `host_roles` here and (in this test process) no SPOT_HOST_ROLES: fail-closed.
    set_role(
        &env,
        &format!(
            "[services.node_role]\nrole = \"edge\"\nnode = {NODE:?}\n\
             hub_url = \"https://hub.example\"\nenrol_code_file = {:?}\n",
            code.display()
        ),
    );
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_cannot_declare_a_hub_listener() {
    let Some(env) = Env::new("").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    switch_on_partition(&env, NODE);
    let code = enrol_code_file(&env);
    set_role(
        &env,
        &format!(
            "[services.node_role]\nrole = \"edge\"\nhost_roles = [\"branch-edge\"]\nnode = {NODE:?}\n\
             hub_url = \"https://hub.example\"\nenrol_code_file = {:?}\n\
             listen = \"0.0.0.0:7443\"\n",
            code.display()
        ),
    );
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

// --- role change -------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_change_without_the_operation_is_refused_and_a_repeat_start_is_fine() {
    let Some(env) = Env::new("").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    // First start: standalone (the default), recorded in spot_node_role_state.
    let (status, started) = site_status(&env).await;
    assert_ne!(status, 503, "the site must have started");
    started.finish().await;
    // A second standalone start repeats the same recorded role and must stay fine.
    let (status, started) = site_status(&env).await;
    assert_ne!(status, 503, "the site must have started");
    started.finish().await;
    // Changing the role without the (out-of-scope) operation is refused.
    set_role(
        &env,
        "[services.node_role]\nrole = \"hub\"\nlisten = \"0.0.0.0:17445\"\n",
    );
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

// --- the table itself ----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_role_state_table_is_refused() {
    let Some(env) = Env::new("").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    env.site();
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == env.site)
        .unwrap()
        .clone();
    let backend = spot_cli::schema::backend(&loaded, &site).await.unwrap();
    spot_node_role_pg::drop_table_for_tests(&backend.pool())
        .await
        .unwrap();
    let (status, started) = site_status(&env).await;
    assert_eq!(status, 503);
    started.finish().await;
}

// --- the service sets (WP-RTB-03.T6) ---------------------------------------------------------

/// Signs the administrator in and asks `path`; returns `(status, code)`.
async fn hub_route(env: &Env, token: &str, started: &Started, path: &str) -> (u16, String) {
    let reply = common::http(started.addr, &env.site, Some(token), "GET", path, &[], None).await;
    let code = reply.body["error"]["code"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    (reply.status, code)
}

const HUB_ONLY_ROUTES: &[&str] = &[
    "/api/v1/sync/hub/status",
    "/api/v1/journal/segments",
    "/api/v1/enrolment/codes",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_edge_answers_501_on_every_hub_only_route_and_needs_no_hub_key() {
    let Some(env) = Env::new("").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    let secret = env.site();
    switch_on_partition(&env, NODE);
    let code = enrol_code_file(&env);
    // No `listen`, no key material of any kind: an edge starts without a hub listener.
    set_role(
        &env,
        &format!(
            "[services.node_role]\nrole = \"edge\"\nhost_roles = [\"branch-edge\"]\nnode = {NODE:?}\n\
             hub_url = \"https://hub.invalid.example:1\"\nenrol_code_file = {:?}\n",
            code.display()
        ),
    );
    let started = common::start(&env).await;
    let secret = common::admin(started.addr, &env.site, &secret).await;
    for path in HUB_ONLY_ROUTES {
        let (status, code) = hub_route(&env, &secret, &started, path).await;
        assert_eq!(
            (status, code.as_str()),
            (501, "service.not_configured"),
            "{path}"
        );
    }
    // A route that is not hub-only is untouched by the layer.
    let (status, code) = hub_route(&env, &secret, &started, "/api/v1/journals").await;
    assert_ne!(code, "service.not_configured", "status {status}");
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_standalone_answers_501_on_hub_only_routes_and_a_hub_does_not() {
    let Some(env) = Env::new("").await else {
        eprintln!("skipped: no SPOT_TEST_PG_URL");
        return;
    };
    let secret = env.site();
    let started = common::start(&env).await;
    let secret = common::admin(started.addr, &env.site, &secret).await;
    for path in HUB_ONLY_ROUTES {
        let (status, code) = hub_route(&env, &secret, &started, path).await;
        assert_eq!(
            (status, code.as_str()),
            (501, "service.not_configured"),
            "{path}"
        );
    }
    started.finish().await;

    // A hub leaves the routes to the hub services (WP-RTB-02): the layer is not installed, so
    // nothing here answers 501 on its behalf.
    let Some(env) =
        Env::new("[services.node_role]\nrole = \"hub\"\nlisten = \"0.0.0.0:17446\"\n").await
    else {
        return;
    };
    let secret = env.site();
    let started = common::start(&env).await;
    let secret = common::admin(started.addr, &env.site, &secret).await;
    for path in HUB_ONLY_ROUTES {
        let (_, code) = hub_route(&env, &secret, &started, path).await;
        assert_ne!(code, "service.not_configured", "{path}");
    }
    started.finish().await;
}
