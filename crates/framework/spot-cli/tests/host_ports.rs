//! The host forwards the ports a service provides into the API `Ports` (WP-CTL-11.T5), so a route
//! group of the registry is mounted when its service provides the port and named in readiness
//! when it is not.
//!
//! The service of the group is a test module here (`fake_link`): it provides a
//! `dyn LinkInboundPort` and declares the group's open path, exactly as `services/link_external.rs`
//! does with `ctx.provide`. A real host (PostgreSQL through `SPOT_TEST_PG_URL`, skipped with no
//! output when unset) answers the inbound path with the module and does not serve it without.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::Method;
use common::*;
use spot_cli::services::{OpenPath, RateLimit, RegisterFuture, Registration, SiteCtx};
use spot_core::SiteId;
use spot_http::api::ports::link_inbound::{
    DEFAULT_MAX_BODY_BYTES, InboundCall, LinkInboundPort, LinkInboundRefusal,
};
use spot_http::api::router::routes;
use spot_http::api::service::BoxFuture;

const INBOUND: &str = "/api/v1/link/inbound/acme";
const REFUSED_BODY: &[u8] = b"refuse-me";

/// The audit of `host.rs` (`GROUPS`), repeated: every registry group and how its port reaches the
/// site. A group that is in the registry and not here fails `every_registry_group_is_classified`,
/// so the next group cannot be added without deciding it in both places.
const GROUPS: &[(&str, &str)] = &[
    ("ai_timeline", "service"),
    ("apps", "service"),
    ("connectors", "service"),
    ("consumer_renew", "no_provider"),
    ("events", "service"),
    ("graph", "service"),
    ("identity", "service"),
    ("import", "service"),
    ("invitation_accept", "service"),
    ("journal_access", "service"),
    ("link_inbound", "forwarded"),
    ("notices", "no_provider"),
    ("occupancy", "no_provider"),
    ("process", "service"),
    ("process_version", "service"),
    ("report", "service"),
    ("scim", "service"),
    ("session", "service"),
    ("sso", "service"),
    ("sso_admin", "service"),
    ("step_up", "service"),
    ("task", "no_provider"),
    ("webhook", "service"),
    ("webhook_secret", "service"),
];

#[test]
fn every_registry_group_is_classified() {
    let classified: BTreeSet<&str> = GROUPS.iter().map(|(name, _)| *name).collect();
    for registration in routes::REGISTRY {
        assert!(
            classified.contains(registration.name),
            "the route group `{}` is not in the audit table (host.rs GROUPS and this test): decide \
             whether the host forwards its port, the service mounts it, or nobody provides it",
            registration.name
        );
    }
    // The classified names are real groups (a removed group leaves no stale row).
    let registered: BTreeSet<&str> = routes::REGISTRY.iter().map(|r| r.name).collect();
    for (name, _) in GROUPS {
        assert!(registered.contains(name), "`{name}` is not a route group");
    }
}

#[test]
fn the_forwarded_group_declares_the_port_the_host_forwards() {
    let registration = routes::REGISTRY
        .iter()
        .find(|r| r.name == "link_inbound")
        .expect("the link_inbound group is registered");
    assert_eq!((registration.required_ports)().len(), 1);
    assert!(registration.public.is_some(), "the group is public");
}

/// The inbound port: accepts every body except one, which it refuses the way every refusal is.
struct Fake;

impl LinkInboundPort for Fake {
    fn admit(&self, _call: &InboundCall) -> Result<usize, LinkInboundRefusal> {
        Ok(DEFAULT_MAX_BODY_BYTES)
    }

    fn receive<'a>(
        &'a self,
        call: InboundCall,
        body: Bytes,
    ) -> BoxFuture<'a, Result<(), LinkInboundRefusal>> {
        Box::pin(async move {
            assert_eq!(call.peer, "acme");
            let _: &SiteId = &call.site;
            if body.as_ref() == REFUSED_BODY {
                Err(LinkInboundRefusal::Rejected)
            } else {
                Ok(())
            }
        })
    }
}

fn register_fake_link<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        ctx.provide::<dyn LinkInboundPort>(Arc::new(Fake))?;
        // The path is open to the host (no session) and the port decides who may call it.
        ctx.open_paths(&[OpenPath {
            method: Method::POST,
            pattern: "/api/v1/link/inbound/{peer}",
            limit: RateLimit { per_minute: 600 },
        }])?;
        Ok(())
    })
}

static WITH_LINK: [Registration; 1] = [Registration {
    name: "fake_link",
    order: 1300,
    register: register_fake_link,
}];
static WITHOUT_LINK: [Registration; 0] = [];

async fn start_modules(env: &Env, modules: &'static [Registration]) -> Started {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running =
        spot_cli::start_with_modules(&env.root.0, false, fast_options(), None, modules, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    let addr = running.local_addrs()[0];
    Started {
        running,
        addr,
        stop,
    }
}

async fn post_inbound(started: &Started, host: &str, token: Option<&str>, body: &[u8]) -> Reply {
    http_raw(
        started.addr,
        host,
        token,
        "POST",
        INBOUND,
        &[("X-Peer-Signature", "not-checked-by-the-fake")],
        ("application/octet-stream", body),
    )
    .await
}

fn text(reply: &Reply) -> String {
    String::from_utf8_lossy(&reply.bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_whose_service_provides_the_inbound_port_answers_the_path() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    let started = start_modules(&env, &WITH_LINK).await;
    let host = env.site.clone();

    // No session: the group is public and the port decides.
    let accepted = post_inbound(&started, &host, None, b"{\"hello\":\"peer\"}").await;
    expect(&accepted, 202);
    assert_eq!(accepted.body["accepted"], true, "{}", text(&accepted));
    // A refusal is the port's one uniform 401, not the host's own authentication answer.
    let refused = post_inbound(&started, &host, None, REFUSED_BODY).await;
    expect(&refused, 401);
    assert!(
        text(&refused).contains("link_inbound.unauthorized"),
        "the answer is the route's, not the host's: {}",
        text(&refused)
    );

    // Readiness does not name the group: it is served.
    let ready = http(
        started.addr,
        &host,
        None,
        "GET",
        "/api/health/ready",
        &[],
        None,
    )
    .await;
    assert_eq!(ready.status, 200, "{}", text(&ready));
    assert!(
        !text(&ready).contains("link_inbound"),
        "a served group is not named: {}",
        text(&ready)
    );
    let _ = first;
    started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_without_the_service_does_not_serve_the_path_and_readiness_names_the_group() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let first = env.site();
    let started = start_modules(&env, &WITHOUT_LINK).await;
    let host = env.site.clone();
    let admin = admin(started.addr, &host, &first).await;

    // The path is absent: even a signed-in administrator finds nothing there.
    let absent = post_inbound(&started, &host, Some(&admin), b"{}").await;
    expect(&absent, 404);

    // The skipped group is visible in readiness (still 200: the site is in rotation).
    let ready = http(
        started.addr,
        &host,
        None,
        "GET",
        "/api/health/ready",
        &[],
        None,
    )
    .await;
    assert_eq!(ready.status, 200, "{}", text(&ready));
    let body = text(&ready);
    assert!(
        body.contains("link_inbound") && body.contains("is not served"),
        "readiness names the skipped group: {body}"
    );
    // The groups their own services mount are not reported as skipped.
    for service_group in [
        "events", "export", "process", "report", "scim", "session", "webhook",
    ] {
        assert!(
            !body.contains(&format!("api group `{service_group}`")),
            "{service_group} is mounted by its own service and is not reported: {body}"
        );
    }
    started.finish().await;
}
