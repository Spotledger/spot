//! The event stream (WP-INT-02.T4): named consumers, pull, ack, restart and the SSE stream over
//! a real `spot serve` on real PostgreSQL (skipped, with no output, when `SPOT_TEST_PG_URL` is
//! unset). Delivery is at-least-once: nothing here expects exactly-once.
//!
//! `enabled` defaults to `false` (WP-INT-02.T6, opt-in): a site with no `[services.events]`
//! table, or a table that omits `enabled`, serves nothing. Every stack here turns the service
//! on with `enabled = true` (`events/fixture.rs`'s `Stack::start`). The tests at the end of
//! this file prove that a wholly absent table, and a table without `enabled`, serve nothing and
//! create no tables.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
// The tests live in `events/`, one file per theme; they are included here, not declared as
// modules, so they share this file's imports and the fixtures without any visibility changes.
include!("events/fixture.rs");
include!("events/lifecycle.rs");
include!("events/stream.rs");
include!("events/rights.rs");

// `enabled` defaults to `false` (WP-INT-02.T6): a site with no `[services.events]` table at
// all must create no consumer tables, mount no route and never appear in `Health`. This test
// is owned by this file alone: it starts its own bare site directly with `Env`/`serve`/
// `administrator` (defined in `events/fixture.rs`, included above, and used unchanged — the
// site this test starts never has a `[services.events]` table written into its `common.toml`
// at all, which is exactly what `Env::new()` already gives: no call into `Stack` here).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_default_site_creates_no_consumer_tables_and_serves_no_event_route() {
    let Some(env) = Env::new().await else {
        return;
    };
    let secret = env
        .ok(&["new-site", &env.site])
        .trim()
        .rsplit(' ')
        .next()
        .unwrap()
        .to_owned();
    let (server, addr, stop) = serve(&env).await;
    let host = env.site.clone();
    let admin = administrator(addr, &host, &secret).await;
    let call = |path: &'static str| {
        let admin = admin.clone();
        let host = host.clone();
        async move { http(addr, &host, Some(&admin), "GET", path, &[], None).await }
    };

    // The route group was never mounted, so `/api/v1/events/*` must answer exactly what this
    // host answers a path that was never a route at all (verified against one): a bare `404`
    // with no body, not a dedicated "service disabled" response (the contract lists no `501`
    // for any `events` operation, so inventing one would be wrong).
    let never = call("/api/v1/zzzznonexistent").await;
    assert_eq!(never.status, 404, "{:?}", never.body);
    for path in [
        "/api/v1/events/consumers",
        "/api/v1/events/stream?consumer=x&partition=0",
    ] {
        let reply = call(path).await;
        assert_eq!(
            (reply.status, reply.body.clone()),
            (never.status, never.body.clone()),
            "{path} answered differently from a path that was never a route: {:?}",
            reply.body
        );
    }

    // No store `prepare()` ran: neither consumer table exists in the site's own database.
    let live = env.live().await;
    let leaked: Vec<&String> = live
        .tables
        .keys()
        .filter(|name| name.starts_with("spot_event_"))
        .collect();
    assert!(
        leaked.is_empty(),
        "a default (disabled) site created event-stream tables: {leaked:?}"
    );

    // `Health` never names a service that was never registered.
    let ready = call("/api/health/ready").await;
    assert_eq!(ready.status, 200, "{:?}", ready.body);
    assert!(
        !ready.body.to_string().contains("events"),
        "a disabled site's readiness report mentions `events`: {}",
        ready.body
    );

    let _ = stop.send(());
    server.join().await.unwrap();
}
