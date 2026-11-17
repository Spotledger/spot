//! WP-IAM-11.T25: `spot serve` starts one listener per audience and applies the configured rate
//! limits. Real PostgreSQL and a real host (every test skips, with no output, when
//! `SPOT_TEST_PG_URL` is unset).
//!
//! What is proved: a config with a second staff listener serves the staff routes on both
//! addresses; a consumer listener starts only when some route is marked for consumers (today
//! none is, so it is not started and the staff listener is unaffected), and when one is, the
//! staff-only paths answer the unserved envelope on it; overlapping addresses refuse to start;
//! an invalid rate-limit setting refuses to start naming the key; a single-listener config is
//! unchanged. The limiter's own behaviour (429 after N requests) is proved in `spot-http`
//! (`tests/rate_limiter.rs`, `tests/public_routes.rs`): the only public group today is the
//! consumer renewal, whose port no host provides yet, so no route of this host is rate limited
//! to drive over HTTP.
//!
//! These tests set `[server] listeners`, `auth_rate_limit` and `auth_rate_window_seconds`, which
//! `spot-cli/src/config.rs` must list among the accepted `server` keys (see the report).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use std::net::SocketAddr;

use common::{Env, Started};
use spot_http::ListenerAudience;

/// A free loopback port (bound and released: the window for a race is small and the tests that
/// use it only need an address that is not taken now).
fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// Rewrites `common.toml` with `server` as the body of its `[server]` table.
fn write_common(env: &Env, server: &str) {
    std::fs::write(
        env.root.0.join("common.toml"),
        format!(
            "apps = [{:?}, 'sample']\n[server]\nshutdown_grace = 2\n{server}\n[postgres]\nadmin_url = {:?}\n[services.secrets]\nkek_file = {:?}\n[trust]\nrequire_signed = false\n",
            common::repo().join("apps/platform").display().to_string(),
            env.url,
            format!("{}/keys/{{site}}.json", env.root.0.display()),
        ),
    )
    .unwrap();
}

async fn rig() -> Option<Env> {
    let env = Env::new("").await?;
    let _first = env.site();
    Some(env)
}

/// Starts the host, or the error text when it refuses to.
async fn try_start(env: &Env) -> Result<Started, String> {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    match spot_cli::start_with(&env.root.0, false, common::fast_options(), async {
        let _ = stopped.await;
    })
    .await
    {
        Ok(running) => {
            let addr = running.local_addrs()[0];
            Ok(Started {
                running,
                addr,
                stop,
            })
        }
        Err(error) => Err(error.to_string()),
    }
}

fn consumer_routes_exist() -> bool {
    spot_http::api::routes::route_table()
        .iter()
        .any(|entry| entry.audiences.contains(&spot_core::Audience::Consumer))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_staff_listener_serves_the_staff_routes_and_a_consumer_listener_follows_the_table()
{
    let Some(env) = rig().await else { return };
    write_common(
        &env,
        &format!(
            "bind = ['127.0.0.1:0']\nlisteners = [{{ bind = '{}', audience = 'staff' }}, {{ bind = '{}', audience = 'consumer' }}]",
            free_addr(),
            free_addr()
        ),
    );
    let started = try_start(&env).await.expect("the host starts");
    let staff = started
        .running
        .handle
        .local_addrs_for(ListenerAudience::Staff);
    let consumer = started
        .running
        .handle
        .local_addrs_for(ListenerAudience::Consumer);
    assert_eq!(staff.len(), 2, "bind and the staff listener");
    // A staff route is reached on every staff address: no credential is the auth.required
    // answer of the route, not the unserved envelope.
    for addr in &staff {
        let reply = common::http(
            *addr,
            &env.site,
            None,
            "GET",
            "/api/v1/auth/session",
            &[],
            None,
        )
        .await;
        common::expect(&reply, 401);
        assert_eq!(reply.body["error"]["code"], "auth.required");
    }
    if consumer_routes_exist() {
        assert_eq!(consumer.len(), 1);
        // The staff-only path is absent on the consumer listener.
        let reply = common::http(
            consumer[0],
            &env.site,
            None,
            "GET",
            "/api/v1/auth/session",
            &[],
            None,
        )
        .await;
        assert_ne!(
            reply.body["error"]["code"],
            "auth.required",
            "{}",
            String::from_utf8_lossy(&reply.bytes)
        );
    } else {
        // No route is marked for consumers: the listener is not started, said once at info.
        assert!(consumer.is_empty());
    }
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overlapping_addresses_refuse_to_start() {
    let Some(env) = rig().await else { return };
    let addr = free_addr();
    write_common(
        &env,
        &format!("bind = ['{addr}']\nlisteners = [{{ bind = '{addr}', audience = 'consumer' }}]"),
    );
    let error = try_start(&env).await.err().expect("overlapping addresses");
    assert!(error.contains(&addr.to_string()), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_rate_limit_refuses_to_start_naming_the_key() {
    let Some(env) = rig().await else { return };
    write_common(&env, "bind = ['127.0.0.1:0']\nauth_rate_limit = 0");
    let error = try_start(&env).await.err().expect("limit 0");
    assert!(error.contains("auth_rate_limit"), "{error}");
    write_common(
        &env,
        "bind = ['127.0.0.1:0']\nauth_rate_window_seconds = 99999",
    );
    let error = try_start(&env).await.err().expect("window too long");
    assert!(error.contains("auth_rate_window_seconds"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_rate_limit_starts_and_a_single_listener_config_is_unchanged() {
    let Some(env) = rig().await else { return };
    write_common(
        &env,
        "bind = ['127.0.0.1:0']\nauth_rate_limit = 3\nauth_rate_window_seconds = 3600",
    );
    let started = try_start(&env).await.expect("a valid limit starts");
    assert_eq!(
        started
            .running
            .handle
            .local_addrs_for(ListenerAudience::Staff)
            .len(),
        1
    );
    assert!(
        started
            .running
            .handle
            .local_addrs_for(ListenerAudience::Consumer)
            .is_empty()
    );
    let reply = common::http(
        started.addr,
        &env.site,
        None,
        "GET",
        "/api/v1/auth/session",
        &[],
        None,
    )
    .await;
    common::expect(&reply, 401);
    started.finish().await;
}
