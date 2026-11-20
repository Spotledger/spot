//! A consumer bearer reaches the occupancy port (WP-STO-14.T4): the resolver over a consumer
//! session service, the module's provision rule and the operation list. No database needed; the
//! holder and own-hold rules of the adapter itself are in `occupancy_service.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use spot_cli::services::SiteCtx;
use spot_cli::services::consumer_session::{
    CONSUMER_OPERATIONS, ConsumerSession, ConsumerSessions, SessionConsumers, register,
};
use spot_cli::services::occupancy::ConsumerIdentity;
use spot_core::{ManualClock, SiteId};
use spot_http::api::service::{BoxFuture, Credential, RequestCtx};
use spot_http::request::RequestId;

/// Sessions by token.
struct Table(BTreeMap<String, ConsumerSession>);

impl ConsumerSessions for Table {
    fn session<'a>(&'a self, credential: &'a Credential) -> BoxFuture<'a, Option<ConsumerSession>> {
        let found = self.0.get(&credential.token).cloned();
        Box::pin(async move { found })
    }
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

fn table() -> Arc<Table> {
    let mut map = BTreeMap::new();
    for (token, on, id) in [
        ("tok-a", "shop", "consumer-a"),
        ("tok-other-site", "elsewhere", "consumer-a"),
        ("tok-bad-id", "shop", "not:valid"),
    ] {
        map.insert(
            token.to_owned(),
            ConsumerSession {
                site: site(on),
                consumer_id: id.to_owned(),
            },
        );
    }
    Arc::new(Table(map))
}

fn ctx(token: Option<&str>) -> RequestCtx {
    RequestCtx {
        site: site("shop"),
        credential: token.map(Credential::new),
        idempotency_key: None,
        language: None,
        request_id: RequestId::new("req-1").unwrap(),
        peer: None,
    }
}

#[tokio::test]
async fn a_live_consumer_session_resolves_to_its_consumer() {
    let resolver = SessionConsumers::new(table());
    assert_eq!(
        resolver.consumer(&ctx(Some("tok-a"))).await.as_deref(),
        Some("consumer-a")
    );
}

#[tokio::test]
async fn anything_else_is_refused() {
    let resolver = SessionConsumers::new(table());
    // No credential, an unknown token, a session of another site, an id the holder grammar refuses.
    for token in [
        None,
        Some("nope"),
        Some("tok-other-site"),
        Some("tok-bad-id"),
    ] {
        assert_eq!(resolver.consumer(&ctx(token)).await, None, "{token:?}");
    }
}

fn site_ctx() -> SiteCtx<'static> {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    SiteCtx::new("shop", Duration::from_secs(1), clock, BTreeMap::new(), true)
}

#[test]
fn without_a_consumer_session_service_nothing_is_provided() {
    let mut ctx = site_ctx();
    register(&mut ctx).unwrap();
    assert!(ctx.port::<dyn ConsumerIdentity>().is_none());
}

#[test]
fn with_a_consumer_session_service_the_resolver_is_provided() {
    let mut ctx = site_ctx();
    let sessions: Arc<dyn ConsumerSessions> = table();
    ctx.provide::<dyn ConsumerSessions>(sessions).unwrap();
    register(&mut ctx).unwrap();
    assert!(ctx.port::<dyn ConsumerIdentity>().is_some());
}

#[test]
fn a_consumer_is_offered_only_claim_confirm_and_release() {
    assert_eq!(CONSUMER_OPERATIONS, ["claim", "confirm", "release"]);
    assert!(!CONSUMER_OPERATIONS.contains(&"availability_all"));
}
