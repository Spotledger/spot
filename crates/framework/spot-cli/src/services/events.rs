//! The event stream of one site: named consumers over the outbox, served as the pull API and as
//! server-sent events (`/api/v1/events/*`, ADR-078, delivery level **D1**: at-least-once and
//! resumable; there is no exactly-once claim anywhere).
//!
//! `spot-http` owns the routes ([`EventStreamPort`]); `spot-event-stream` owns the rules
//! (checkpoints that only an acknowledgement advances, partitions, resync, parking, the read
//! filter); `spot-event-stream-pg` keeps the consumers and their checkpoints in the site's
//! database. This module wires them together and supplies what they leave to the host:
//!
//! - **who is calling**: the site's sign-in service resolves the bearer credential; a session
//!   that has not chosen its password yet may do nothing here;
//! - **what may be managed** (default deny): consumer management, pull, ack, restart and the
//!   stream need the `Tenant Administrator` role (unscoped) or a role named in
//!   `services.events.consumer_roles`. A caller without it gets `403 permission.denied`, never
//!   an empty success. The reserved platform topic classes (`doc_`, `sys_`, `identity_`) need
//!   the administrator or a role in `services.events.stream_roles`, checked when the consumer
//!   is registered and again on every pull and stream cycle for the owner;
//! - **what a consumer sees**: only events its owner may read, decided with the permission
//!   engine of the snapshot served *now* and the document's committed facts, and every value
//!   passes the one field guard on the export channel. A decision that cannot be made is a
//!   refusal: the event is skipped (counted in `skipped`, never named);
//! - **one connection per consumer**: a fenced lease from the site's coordinator, renewed while
//!   the connection lives and given back when it ends;
//! - **bounded connections**: at most `max_streams` open streams per site and
//!   `max_streams_per_user` per caller (`429 api.rate_limited`); this budget is separate from
//!   the realtime hub's.
//!
//! # Level
//!
//! Optional. A failed start-up check (the consumer store or the outbox cannot be read) degrades
//! the site and the routes answer `503 server.unavailable`; the rest of the site serves. The
//! consumer tables are created at site start by the store's `prepare()` (safe to repeat, safe
//! from several nodes), as the job store's are; a database that refuses the DDL stops the site
//! start with a message naming the service.
//!
//! # What is never logged or audited
//!
//! Event content, positions' insides, tokens and leases. The audit trail (target
//! `spot::audit::event_stream`) records who registered, restarted, acknowledged or deleted
//! which consumer and where, nothing else.
//!
//! # Honest limits (see `docs/running-spot/events.md`)
//!
//! A pull that is repeated before an ack returns the same events: the host rewinds the
//! partition to its checkpoint, which the engine would otherwise skip past. The in-flight
//! window, the lease and the positions a connection was delivered live in this process: a
//! pull and its ack must reach the same node, and after a restart the consumer simply pulls
//! again from its checkpoint (nothing is lost; some events arrive twice).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{Name, Value};
use spot_coordinator::{Coordinator, Lease};
use spot_core::{Clock, SiteId};
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_event_stream::{
    AuditRecord, AuditSink, ConsumerLease, ConsumerRecord, ConsumerSpec, EngineConfig, FeedHead,
    PayloadMode, PermissionVisibility, Seams, Start, StreamEngine, StreamError, StreamRights,
    SubjectFacts, TopicSelector, Visibility, encode_position, parse_position, position_to_feed,
};
use spot_event_stream_pg::PgConsumerStore;
use spot_events_contract::envelope::{EventEnvelope, Position, Subject};
use spot_events_contract::names::SourceId;
use spot_field_guard::{
    AuditRecord as GuardAudit, FieldAccessRecord, FieldGuard, MetaProtection, Offer, TraceSink,
};
use spot_http::api::Ports;
use spot_http::api::codec::parse_record_id;
use spot_http::api::ports::events::{
    CheckpointOut, ConsumerOut, ConsumerSpecIn, EventFeed, EventStreamPort, EventsResult,
    FeedItem, PartitionOut, PayloadModeIn, PullIn, PullOut, StartIn, StreamIn, StreamLimits,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::api::storage_error_to_api;
use spot_permissions::{DocFacts, Principal, TENANT_ADMINISTRATOR};
use spot_postgres::PgBackend;
use spot_storage_contract::{ChangeFeed, ErrorKind, FeedPosition};
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::{Served, SiteLive};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "events/feed.rs"]
mod feed;
#[path = "events/ops.rs"]
mod ops;
#[path = "events/seams.rs"]
mod seams;

use feed::{StreamFeed, StreamSlot};
use seams::{EventAudit, HeadCache, LiveFacts, LiveVisibility, Principals, Rights};

/// Start order: after the framework's own services (they use 100 to 900).
pub const ORDER: u32 = 1000;

/// The audit target of this service.
const AUDIT_TARGET: &str = "spot::audit::event_stream";

/// How many outbox records one head scan reads at a time.
const HEAD_PAGE: u32 = 500;

/// `[services.events]`. Unknown keys are refused; a value is never echoed in an error.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    /// Whether the site serves the event stream at all. Off by default (opt-in,
    /// WP-INT-02.T6): a site with no `[services.events]` table creates no consumer tables,
    /// mounts no route and does not appear in `Health`, exactly as if the service did not
    /// exist. The routes then answer what every path no site route serves answers (a bare
    /// `404` with an empty body). A table that omits `enabled` is off too: write
    /// `enabled = true` to turn the service on.
    enabled: bool,
    /// How often an open stream or a waiting pull looks at the feed again (milliseconds).
    poll_ms: u64,
    /// A keepalive comment goes out after this many silent seconds (contract: 25).
    heartbeat_seconds: u64,
    /// A stream that delivered no event for this long ends (seconds).
    max_idle_seconds: u64,
    /// The consumer lease's time to live (seconds).
    lease_seconds: u64,
    /// How often an open stream asks again whether its credential and rights still hold.
    revalidate_seconds: u64,
    /// The most streams open at once on this site.
    max_streams: usize,
    /// The most streams one caller holds open at once.
    max_streams_per_user: usize,
    /// The in-flight window: delivered, unacknowledged events per partition.
    window: usize,
    /// Roles, besides `Tenant Administrator`, that may manage and read consumers.
    consumer_roles: Vec<String>,
    /// Roles, besides `Tenant Administrator`, that may stream the reserved topic classes.
    stream_roles: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_ms: 250,
            heartbeat_seconds: 25,
            max_idle_seconds: 900,
            lease_seconds: 60,
            revalidate_seconds: 10,
            max_streams: 64,
            max_streams_per_user: 4,
            window: 1000,
            consumer_roles: Vec::new(),
            stream_roles: Vec::new(),
        }
    }
}

impl Settings {
    fn validate(&self) -> Result<()> {
        let bad = |key: &str, why: &str| Err(Error(format!("services.events.{key}: {why}")));
        if !(20..=5_000).contains(&self.poll_ms) {
            return bad("poll_ms", "must be 20 to 5000");
        }
        if !(1..=120).contains(&self.heartbeat_seconds)
            || self.heartbeat_seconds * 1000 < self.poll_ms * 2
        {
            return bad(
                "heartbeat_seconds",
                "must be 1 to 120 and at least twice poll_ms",
            );
        }
        if !(self.heartbeat_seconds..=86_400).contains(&self.max_idle_seconds) {
            return bad("max_idle_seconds", "must be heartbeat_seconds to 86400");
        }
        if !(5..=3_600).contains(&self.lease_seconds) {
            return bad("lease_seconds", "must be 5 to 3600");
        }
        if !(1..=300).contains(&self.revalidate_seconds) {
            return bad("revalidate_seconds", "must be 1 to 300");
        }
        if !(1..=10_000).contains(&self.max_streams) {
            return bad("max_streams", "must be 1 to 10000");
        }
        if !(1..=self.max_streams).contains(&self.max_streams_per_user) {
            return bad("max_streams_per_user", "must be 1 to max_streams");
        }
        if !(1..=10_000).contains(&self.window) {
            return bad("window", "must be 1 to 10000");
        }
        for (key, roles) in [
            ("consumer_roles", &self.consumer_roles),
            ("stream_roles", &self.stream_roles),
        ] {
            if roles
                .iter()
                .any(|r| r.is_empty() || r.len() > 128 || r.chars().any(char::is_control))
            {
                return bad(key, "a role name is 1 to 128 printable characters");
            }
        }
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

// ---------------------------------------------------------------------------------------------
// Positions on the wire, and errors
// ---------------------------------------------------------------------------------------------

/// The contract's `EventPosition` for `position` of `consumer`: `v1.` and the engine's
/// consumer-bound token.
fn wire(consumer: &str, position: Position) -> String {
    format!("v1.{}", encode_position(consumer, position))
}

/// The position `text` names for `consumer`, or `422 eventstream.position_invalid` for one that
/// is malformed or was issued for another consumer.
fn unwire(consumer: &str, text: &str) -> EventsResult<Position> {
    text.strip_prefix("v1.")
        .and_then(|token| parse_position(consumer, token).ok())
        .ok_or_else(|| ApiError::from_code(Code::EventstreamPositionInvalid))
}

fn order(p: Position) -> FeedPosition {
    position_to_feed(p)
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable)
}

fn denied() -> ApiError {
    ApiError::from_code(Code::PermissionDenied)
}

fn spec_invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(Code::EventstreamSpecInvalid, message)
}

/// A stream failure as the API reports it. No message carries event content or a token.
fn api_error(error: StreamError, lease_seconds: u64) -> ApiError {
    match error {
        StreamError::ConsumerNotFound { .. } => {
            ApiError::from_code(Code::EventstreamConsumerNotFound)
        }
        StreamError::ConsumerBusy { .. } => ApiError::from_code(Code::EventstreamConsumerBusy)
            .detail("retry_after", Json::from(lease_seconds.max(1))),
        StreamError::ResyncRequired {
            name,
            retained_from,
            ..
        } => ApiError::from_code(Code::EventstreamResyncRequired)
            .detail("retained_from", Json::String(wire(&name, retained_from))),
        StreamError::WindowFull { .. } => ApiError::from_code(Code::EventstreamWindowFull),
        StreamError::PositionInvalid { .. } => {
            ApiError::from_code(Code::EventstreamPositionInvalid)
        }
        StreamError::AckRefused { reason } => {
            ApiError::new(Code::EventstreamAckRefused, reason)
        }
        StreamError::SpecInvalid { reason } => spec_invalid(reason),
        StreamError::Feed(storage) => storage_error_to_api(&storage, None),
        StreamError::Coordinator { code, .. } => {
            tracing::warn!(code, "the event stream coordinator refused");
            unavailable()
        }
        StreamError::Permission { code, .. } => {
            tracing::error!(code, "the event stream could not filter an event");
            ApiError::from_code(Code::HttpInternal)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The shared state of one site's event stream
// ---------------------------------------------------------------------------------------------

/// A lease this process handed out.
#[derive(Clone)]
struct Held {
    token: String,
    lease: Lease,
    streaming: bool,
}

/// Open streams, in total and per caller.
#[derive(Default)]
struct Streams {
    total: usize,
    per_user: HashMap<String, usize>,
}

/// The turn-takers of the partitions: one operation at a time on a (consumer, partition).
type Gates = Mutex<HashMap<(String, u32), Arc<tokio::sync::Mutex<()>>>>;

/// The caller of one request, resolved and checked.
struct Caller {
    user: String,
    principal: Principal,
    stream_reserved: bool,
}

struct Shared {
    site: String,
    live: Arc<SiteLive>,
    backend: PgBackend,
    engine: StreamEngine<PgBackend>,
    coordinator: Arc<dyn Coordinator>,
    clock: Arc<dyn Clock>,
    settings: Settings,
    head: Arc<HeadCache>,
    principals: Arc<Principals>,
    rights: Arc<Rights>,
    audit: Arc<EventAudit>,
    store: Arc<PgConsumerStore>,
    held: Mutex<HashMap<String, Held>>,
    outstanding: Mutex<HashSet<(String, u32)>>,
    sent: Mutex<HashMap<(String, u32), Position>>,
    locks: Gates,
    streams: Mutex<Streams>,
    stop: watch::Sender<bool>,
    ready: AtomicBool,
}

/// Whether `principal` holds the administrator role, or one of `extra`, without a scope.
fn holds(principal: &Principal, extra: &[String]) -> bool {
    principal.assignments.iter().any(|assignment| {
        assignment.scope.is_empty()
            && assignment
                .roles
                .iter()
                .any(|role| role.0 == TENANT_ADMINISTRATOR || extra.contains(&role.0))
    })
}

fn token_of(random: &str) -> String {
    format!("l1.{random}")
}

// ---------------------------------------------------------------------------------------------
// The port
// ---------------------------------------------------------------------------------------------

struct EventsPort {
    shared: Arc<Shared>,
}

impl EventStreamPort for EventsPort {
    fn list<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, EventsResult<Vec<ConsumerOut>>> {
        Box::pin(async move { self.shared.list(ctx).await })
    }

    fn put<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        spec: ConsumerSpecIn,
    ) -> BoxFuture<'a, EventsResult<(ConsumerOut, bool)>> {
        Box::pin(async move { self.shared.put(ctx, name, spec).await })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, EventsResult<ConsumerOut>> {
        Box::pin(async move { self.shared.get(ctx, name).await })
    }

    fn delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
    ) -> BoxFuture<'a, EventsResult<()>> {
        Box::pin(async move { self.shared.delete(ctx, name).await })
    }

    fn pull<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        request: PullIn,
    ) -> BoxFuture<'a, EventsResult<PullOut>> {
        Box::pin(async move { self.shared.pull(ctx, name, lease, request).await })
    }

    fn ack<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        partition: u32,
        position: &'a str,
    ) -> BoxFuture<'a, EventsResult<CheckpointOut>> {
        Box::pin(async move { self.shared.ack(ctx, name, lease, partition, position).await })
    }

    fn restart<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        name: &'a str,
        lease: Option<&'a str>,
        partition: u32,
        from: StartIn,
    ) -> BoxFuture<'a, EventsResult<CheckpointOut>> {
        Box::pin(async move { self.shared.restart(ctx, name, lease, partition, from).await })
    }

    fn open_stream<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: StreamIn,
    ) -> BoxFuture<'a, EventsResult<Box<dyn EventFeed>>> {
        Box::pin(async move { self.shared.open_stream(ctx, request).await })
    }

    fn limits(&self) -> StreamLimits {
        StreamLimits {
            heartbeat: Duration::from_secs(self.shared.settings.heartbeat_seconds),
            max_idle: Duration::from_secs(self.shared.settings.max_idle_seconds),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

struct EventsService {
    shared: Arc<Shared>,
}

impl Service for EventsService {
    fn name(&self) -> &'static str {
        "events"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let shared = &self.shared;
            shared
                .engine
                .list(&Principal::system())
                .await
                .map_err(|e| format!("the consumer store cannot be read ({})", e.code()))?;
            shared
                .backend
                .read_outbox(FeedPosition::START, 1)
                .await
                .map_err(|e| format!("the outbox cannot be read ({})", e.kind.code()))?;
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let shared = &self.shared;
            let mut stop = stop;
            shared.ready.store(true, Ordering::Release);
            // Prime the head so the first `tail` consumer does not scan the feed.
            if shared.head.refresh(&shared.backend).await.is_err() {
                tracing::warn!(site = %shared.site, "the event stream could not read the outbox head");
            }
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            break;
                        }
                    }
                    () = tokio::time::sleep(Duration::from_secs(60)) => {
                        match shared.engine.parked_report().await {
                            Ok(parked) if !parked.is_empty() => tracing::warn!(
                                site = %shared.site,
                                parked = parked.len(),
                                "event consumers are parked: an operator restarts the partition"
                            ),
                            _ => {}
                        }
                    }
                }
            }
            shared.ready.store(false, Ordering::Release);
            // Open streams and waiting pulls end; give them the chance to give their leases back.
            let _ = shared.stop.send(true);
            let drain = tokio::time::Instant::now() + Duration::from_secs(5);
            while lock(&shared.streams).total > 0 && tokio::time::Instant::now() < drain {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok(())
        })
    }
}

/// The ports of the sites built so far, by site name: a reload builds the site again without
/// building the service again, and its routes must reach the same port.
fn built() -> &'static Mutex<HashMap<String, Arc<EventsPort>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<EventsPort>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

async fn build(ctx: &mut SiteCtx<'_>, settings: Settings) -> Result<Arc<EventsPort>> {
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let site_id = SiteId::new(&site_name).map_err(|e| Error(e.to_string()))?;
    let backend = env.backend.clone();
    let clock = Arc::clone(ctx.clock());
    let source = SourceId::new(&site_name)
        .map_err(|_| Error(format!("the site name `{site_name}` is not a valid event source")))?;
    let store = Arc::new(PgConsumerStore::new(backend.pool(), site_id.clone()));
    store
        .prepare()
        .await
        .map_err(|e| Error(format!("services.events: the consumer tables: {}", e.code)))?;
    let node = format!(
        "events-{site_name}-{}",
        spot_storage_contract::RecordId::new_v7()
            .to_string()
            .rsplit('-')
            .next()
            .unwrap_or("0")
    );
    let coordinator: Arc<dyn Coordinator> = if env.loaded.common.services.profile == "cluster" {
        let coordinator = spot_coordinator_pg::PgCoordinator::new(
            backend.pool(),
            spot_coordinator_pg::PgCoordinatorConfig::default(),
        );
        coordinator
            .prepare()
            .await
            .map_err(|e| Error(format!("services.events: the coordinator: {}", e.code)))?;
        Arc::new(coordinator)
    } else {
        let node_id = spot_coordinator::NodeId::new(node.clone())
            .map_err(|e| Error(format!("services.events: the node name: {}", e.code)))?;
        spot_coordinator::InProcessCoordinator::start(
            Arc::new(spot_coordinator::InMemoryPresence::new()),
            node_id,
            Arc::clone(&clock),
        )
        .await
        .map_err(|e| Error(format!("services.events: the coordinator: {}", e.code)))?
    };
    let head = Arc::new(HeadCache::default());
    let principals = Arc::new(Principals::default());
    let rights = Arc::new(Rights::default());
    let audit = Arc::new(EventAudit {
        site: site_name.clone(),
        quiet: Mutex::default(),
    });
    let facts = Arc::new(LiveFacts {
        live: Arc::clone(env.live),
        clock: Arc::clone(&clock),
        site: site_id,
    });
    let visibility = Arc::new(LiveVisibility {
        live: Arc::clone(env.live),
        facts,
        principals: Arc::clone(&principals),
        clock: Arc::clone(&clock),
        cached: Mutex::new(None),
    });
    let mut config = EngineConfig::new(source);
    config.policy.window = settings.window;
    config.holder = node;
    config.lease_ttl = Duration::from_secs(settings.lease_seconds);
    let engine = StreamEngine::new(
        Seams {
            feed: Arc::new(backend.clone()),
            head: Some(Arc::clone(&head) as Arc<dyn FeedHead>),
            store: Arc::clone(&store) as Arc<dyn spot_event_stream::ConsumerStore>,
            coordinator: Arc::clone(&coordinator),
            visibility,
            audit: Arc::clone(&audit) as Arc<dyn AuditSink>,
            clock: Arc::clone(&clock),
            rights: Arc::clone(&rights) as Arc<dyn StreamRights>,
        },
        config,
    );
    let (stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        site: site_name,
        live: Arc::clone(env.live),
        backend,
        engine,
        coordinator,
        clock,
        settings,
        head,
        principals,
        rights,
        audit,
        store,
        held: Mutex::default(),
        outstanding: Mutex::default(),
        sent: Mutex::default(),
        locks: Mutex::default(),
        streams: Mutex::default(),
        stop,
        ready: AtomicBool::new(false),
    });
    let _ = &shared.store;
    ctx.add(Arc::new(EventsService {
        shared: Arc::clone(&shared),
    }));
    Ok(Arc::new(EventsPort { shared }))
}

/// Registers the event stream: its service, and its routes on the site's router.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("events")?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.env()?.site.site.clone();
    let port = if ctx.assembling() {
        let port = build(ctx, settings).await?;
        lock(built()).insert(site, Arc::clone(&port));
        port
    } else {
        match lock(built()).get(&site).cloned() {
            Some(port) => port,
            None => return Ok(()),
        }
    };
    let mut ports = Ports::new();
    ports.provide::<dyn EventStreamPort>(port);
    // The group is mounted here, not by the route registry (see `routes/events.rs`). The host
    // authenticates every request before the router sees it, and the port authenticates again.
    let mounted = routes::mount(&[routes::events::registration()], &ports, &[]);
    let router = mounted.authenticated;
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(module = *module, ?reason, "the event stream routes were not mounted");
    }
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}


#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod settings_tests {
    use super::Settings;

    #[test]
    fn a_table_that_omits_enabled_is_off() {
        let omitted: Settings = toml::from_str("poll_ms = 50").unwrap();
        assert!(!omitted.enabled);
        let on: Settings = toml::from_str("enabled = true").unwrap();
        assert!(on.enabled);
        assert!(!Settings::default().enabled);
    }
}
