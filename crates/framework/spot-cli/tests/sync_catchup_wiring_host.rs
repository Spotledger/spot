//! Catch-up wired to the hosts (WP-RTB-02.T15): the hub answers a returning node with chunks, the
//! node applies them and reaches the hub's head, a failed apply leaves the node at its previous
//! chunk and acknowledges nothing, a hub that cannot serve says so with the catalogued error, and
//! the module's order and registration. A hub and an edge on real databases are the end-to-end
//! scenarios (`harness/rust/l3-journal-sync`, WP-RTB-02.T9 and WP-RTB-03.T4).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use spot_cli::services::node_role::{ActiveRole, Role};
use spot_cli::services::sync_catchup_wiring::{
    AckReply, CatchupError, CatchupFeed, CatchupHub, CatchupNode, CatchupRoutes, ChunkSource,
    NodeView, ORDER, RouteReply, Settings, Step, register,
};
use spot_cli::services::{SiteCtx, sync_catchup, sync_catchup_serve};
use spot_core::ManualClock;
use spot_journal_snapshot::{ImageRecord, SnapshotInfo};
use spot_sync_bootstrap::{
    Bundle, ChunkRecord, HubHead, MemorySource, NodeState, ScopeView, SnapshotSource,
};
use spot_sync_catchup::{CatchUpPlan, ClassCounts, Event, GapSummary, Mode, Payload};
use spot_sync_catchup_host::{
    ApplyTarget, CatchupConfig, ChunkApplier, ChunkServer, Director, MemoryProgress, NodeSeams,
    ProgressStore, SeamsProvider, ServeError, SnapshotProvider,
};
use spot_sync_closure::{ClosureError, Limits, NoRedaction, RecordKey, References};
use spot_sync_http::AuthenticatedNode;
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;
use tower::ServiceExt;

const TENANT: &str = "acme";
const NODE: &str = "branch-1";
const POSITION: u64 = 100;

struct Everything;

impl ScopeView for Everything {
    fn covers(&self, _key: &RecordKey, _image: &Value) -> bool {
        true
    }
    fn fingerprint(&self) -> String {
        "everything".to_owned()
    }
}

struct NoLinks;

impl References for NoLinks {
    fn references(&self, _entity: &str, _image: &Value) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(Vec::new())
    }
}

struct Seams;

impl SeamsProvider for Seams {
    fn for_node(&self, node: &str) -> Option<NodeSeams> {
        (node == NODE).then(|| NodeSeams {
            scope: Box::new(Everything),
            references: Box::new(NoLinks),
            redactor: Box::new(NoRedaction),
        })
    }
}

struct Snapshot;

impl SnapshotProvider for Snapshot {
    fn open(&self) -> Result<Box<dyn SnapshotSource>, ServeError> {
        let info = SnapshotInfo {
            position: POSITION,
            generation: 1,
            verified: true,
            taken_at_millis: 0,
        };
        let records = (0..7)
            .map(|n| ImageRecord {
                entity: "item".to_owned(),
                id: format!("item-{n:04}"),
                version: 1,
                entity_schema: 1,
                image: json!({"name": format!("Item {n}")}),
            })
            .collect();
        Ok(Box::new(MemorySource::new(TENANT, info, records)))
    }
}

/// What the node's apply path was asked to apply; refuses the bundle holding the record
/// `fail_on_record` when set (a chunk may hold more than one bundle, so a count of bundles
/// does not name a chunk).
#[derive(Default)]
struct Target {
    applied: Mutex<Vec<String>>,
    events: Mutex<Vec<u64>>,
    bundles: AtomicU64,
    fail_on_record: Option<&'static str>,
}

impl ApplyTarget for Target {
    fn apply_bundle(&self, bundle: &Bundle) -> Result<(), ServeError> {
        self.bundles.fetch_add(1, Ordering::SeqCst);
        if let Some(id) = self.fail_on_record
            && bundle.records.iter().any(|record| record.key.id == id)
        {
            return Err(ServeError::ApplyFailed("apply.refused"));
        }
        let mut applied = self.applied.lock().unwrap();
        for ChunkRecord { key, .. } in &bundle.records {
            applied.push(key.id.clone());
        }
        Ok(())
    }
    fn apply_events(&self, events: &[Event]) -> Result<(), ServeError> {
        self.events
            .lock()
            .unwrap()
            .extend(events.iter().map(|e| e.position));
        Ok(())
    }
}

/// How far behind the node is.
#[derive(Clone, Copy)]
enum Gap {
    /// Nothing applied: a snapshot bootstrap.
    New,
    /// Behind the hot retention floor: a coalesced catch-up.
    Behind,
    /// A few events behind: the live stream.
    Close,
}

fn summary(gap: Gap) -> GapSummary {
    let (applied_prefix, hot_retention_floor, events) = match gap {
        Gap::New => (0, 0, 50),
        Gap::Behind => (10, 50, 50),
        Gap::Close => (115, 1, 5),
    };
    GapSummary {
        applied_prefix,
        stream_end: POSITION + 20,
        hot_retention_floor,
        in_scope_event_count: events,
        class_counts: ClassCounts {
            state: events,
            domain: 0,
            audit: 0,
        },
        estimated_coalesced_records: 10,
        estimated_coalesced_bytes: 1_000,
        estimated_snapshot_bytes: 1_000_000,
        node_is_new_or_resync: matches!(gap, Gap::New),
    }
}

/// A hub host's feed: one node, either brand new (a bootstrap) or behind (a coalesced catch-up).
struct Feed {
    gap: Gap,
    known: bool,
}

impl CatchupFeed for Feed {
    fn view(&self, node: &str) -> Option<NodeView> {
        (self.known && node == NODE).then(|| NodeView {
            state: NodeState {
                node_id: NODE.to_owned(),
                tenant: TENANT.to_owned(),
                generation: 0,
                applied_position: None,
            },
            head: HubHead {
                generation: 1,
                head_position: POSITION + 20,
            },
            summary: summary(self.gap),
        })
    }

    fn plan(&self, node: &str) -> Option<CatchUpPlan> {
        (node == NODE).then(|| {
            let mut mandatory = Event::new(41, "sys_sync_missing", Payload::new("{}"));
            mandatory.mandatory = true;
            CatchUpPlan {
                mode: Mode::Coalesced,
                reasons: Vec::new(),
                mandatory_lane: vec![mandatory],
                bulk: Vec::new(),
                audit: vec![Event::new(43, "sys_doc_audit", Payload::new("{}"))],
            }
        })
    }
}

struct Rig {
    hub: Arc<CatchupHub>,
    hub_progress: Arc<MemoryProgress>,
}

fn rig(gap: Gap, known: bool) -> Rig {
    let hub_progress = Arc::new(MemoryProgress::default());
    let director =
        Arc::new(Director::new(&CatchupConfig::default(), hub_progress.clone()).unwrap());
    let limits = Limits {
        max_records: 2,
        ..Limits::default()
    };
    let server = Arc::new(ChunkServer::new(
        Arc::clone(&director),
        Arc::new(Snapshot),
        Arc::new(Seams),
        limits,
    ));
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let hub = Arc::new(CatchupHub::new(
        director,
        server,
        Arc::new(Feed { gap, known }),
        clock,
    ));
    Rig { hub, hub_progress }
}

/// The node's connection to the hub, in process: the calls a route would make for this node.
struct Loopback {
    hub: Arc<CatchupHub>,
    acks: Mutex<Vec<u64>>,
}

impl ChunkSource for Loopback {
    fn route(&self) -> Result<RouteReply, CatchupError> {
        self.hub.route(NODE)
    }
    fn chunk(&self, index: u64) -> Result<spot_sync_catchup_host::WireChunk, CatchupError> {
        self.hub.chunk(NODE, index)
    }
    fn ack(&self, index: u64) -> Result<AckReply, CatchupError> {
        self.acks.lock().unwrap().push(index);
        self.hub.ack(NODE, index)
    }
}

fn node_for(
    rig: &Rig,
    target: Arc<Target>,
    checkpoint: Arc<dyn ProgressStore>,
) -> (CatchupNode, Arc<Loopback>, Arc<ChunkApplier>) {
    let source = Arc::new(Loopback {
        hub: Arc::clone(&rig.hub),
        acks: Mutex::default(),
    });
    let applier = Arc::new(ChunkApplier::new("hub", checkpoint, target));
    (
        CatchupNode::new(Arc::clone(&applier), source.clone()),
        source,
        applier,
    )
}

fn drive(node: &CatchupNode) -> Step {
    loop {
        match node.step().unwrap() {
            Step::Applied => {}
            other => return other,
        }
    }
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn the_order_is_after_the_director_and_the_chunk_server_and_before_the_hub() {
    assert!(sync_catchup::ORDER < ORDER);
    assert!(sync_catchup_serve::ORDER < ORDER);
    assert!(ORDER < spot_cli::services::sync_hub::ORDER);
    assert!(spot_cli::services::registered_names().contains(&"sync_catchup_wiring"));
}

#[test]
fn the_settings_default_on_and_refuse_unknown_keys() {
    assert!(Settings::default().enabled);
    assert!(
        !toml::from_str::<Settings>("enabled = false")
            .unwrap()
            .enabled
    );
    assert!(toml::from_str::<Settings>("bogus = 1").is_err());
}

#[test]
fn a_new_node_is_bootstrapped_and_reaches_the_hubs_head() {
    let rig = rig(Gap::New, true);
    let target = Arc::new(Target::default());
    let (node, source, applier) =
        node_for(&rig, target.clone(), Arc::new(MemoryProgress::default()));
    assert_eq!(
        node.begin().unwrap(),
        RouteReply::Bootstrap {
            resume_from_chunk: 0
        }
    );
    // The hub is asked for chunk 0 and answers; the node applies and acknowledges each in turn.
    let done = drive(&node);
    assert_eq!(
        done,
        Step::Done {
            resume_from: Some(POSITION + 1)
        }
    );
    let mut got = target.applied.lock().unwrap().clone();
    got.sort();
    assert_eq!(
        got,
        (0..7).map(|n| format!("item-{n:04}")).collect::<Vec<_>>()
    );
    // Every chunk was acknowledged once, in order, and the hub's bookkeeping ended.
    let acks = source.acks.lock().unwrap().clone();
    assert_eq!(
        acks,
        (0..u64::try_from(acks.len()).unwrap()).collect::<Vec<_>>()
    );
    assert!(rig.hub_progress.load(NODE).unwrap().is_none());
    assert_eq!(
        applier.next_index().unwrap(),
        0,
        "the checkpoint is cleared"
    );
}

#[test]
fn a_node_behind_gets_the_coalesced_plan_in_order() {
    let rig = rig(Gap::Behind, true);
    let target = Arc::new(Target::default());
    let (node, _, _) = node_for(&rig, target.clone(), Arc::new(MemoryProgress::default()));
    assert!(matches!(
        node.begin().unwrap(),
        RouteReply::CatchUp {
            resume_from_chunk: 0
        }
    ));
    assert_eq!(drive(&node), Step::Done { resume_from: None });
    assert_eq!(*target.events.lock().unwrap(), vec![41, 43]);
}

#[test]
fn a_failed_apply_leaves_the_node_at_its_previous_chunk_and_acknowledges_nothing() {
    let rig = rig(Gap::New, true);
    let target = Arc::new(Target {
        // The third record opens the second chunk (two records to a chunk).
        fail_on_record: Some("item-0002"),
        ..Target::default()
    });
    let checkpoint: Arc<dyn ProgressStore> = Arc::new(MemoryProgress::default());
    let (node, source, applier) = node_for(&rig, target.clone(), Arc::clone(&checkpoint));
    node.begin().unwrap();
    // Chunk 0 applies and is acknowledged; chunk 1 fails in the apply path.
    assert_eq!(node.step().unwrap(), Step::Applied);
    assert_eq!(
        node.step().unwrap_err(),
        CatchupError::Serve(ServeError::ApplyFailed("apply.refused"))
    );
    assert_eq!(
        applier.next_index().unwrap(),
        1,
        "still at the previous head"
    );
    assert_eq!(
        *source.acks.lock().unwrap(),
        vec![0],
        "the failed chunk was not acknowledged"
    );
    // The hub's director is on the same chunk: a restarted node resumes there, not before it.
    assert_eq!(
        rig.hub.route(NODE).unwrap(),
        RouteReply::Bootstrap {
            resume_from_chunk: 1
        }
    );
}

#[test]
fn a_tampered_chunk_is_refused_before_anything_is_applied() {
    let rig = rig(Gap::New, true);
    let target = Arc::new(Target::default());
    let applier = ChunkApplier::new("hub", Arc::new(MemoryProgress::default()), target.clone());
    rig.hub.route(NODE).unwrap();
    let mut wire = rig.hub.chunk(NODE, 0).unwrap();
    wire.body = wire.body.replace("Item 0", "Item X");
    assert_eq!(applier.apply(&wire), Err(ServeError::Tampered));
    assert!(target.applied.lock().unwrap().is_empty());
}

#[test]
fn the_hub_never_serves_past_the_next_unacknowledged_chunk_or_to_a_live_or_unknown_node() {
    let rig = rig(Gap::New, true);
    rig.hub.route(NODE).unwrap();
    assert_eq!(
        rig.hub.chunk(NODE, 3).unwrap_err(),
        CatchupError::Serve(ServeError::OutOfOrder)
    );
    assert!(rig.hub.ack(NODE, 3).is_err());
    // A chunk may be asked for again (a cut repeats at most the chunk in flight).
    assert!(rig.hub.chunk(NODE, 0).is_ok());
    assert!(rig.hub.chunk(NODE, 0).is_ok());

    let stranger = rig.hub.chunk("branch-9", 0).unwrap_err();
    assert_eq!(stranger, CatchupError::Serve(ServeError::ScopeUnknown));
    let unknown = self::rig(Gap::New, false);
    assert_eq!(
        unknown.hub.route(NODE).unwrap_err(),
        CatchupError::Serve(ServeError::ScopeUnknown)
    );

    // A node that is close behind stays on the live stream and is given no chunk.
    let live = self::rig(Gap::Close, true);
    assert_eq!(live.hub.route(NODE).unwrap(), RouteReply::Live);
    assert_eq!(
        live.hub.chunk(NODE, 0).unwrap_err(),
        CatchupError::Serve(ServeError::OutOfRange)
    );
}

// ---- the routes ----------------------------------------------------------------------------

fn identified(request: Request<Body>) -> Request<Body> {
    let mut request = request;
    request.extensions_mut().insert(AuthenticatedNode::new(
        NodeId::new(NODE).unwrap(),
        std::iter::empty::<String>(),
    ));
    request
}

async fn body_json(response: axum::response::Response) -> (StatusCode, Value) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn the_routes_answer_for_the_authenticated_node_only() {
    let rig = rig(Gap::New, true);
    let routes = CatchupRoutes::serving(Arc::clone(&rig.hub));

    let reply = routes
        .router()
        .oneshot(identified(
            Request::post("/sync/v1/catchup/route")
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    let (status, body) = body_json(reply).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["route"], "bootstrap");

    let reply = routes
        .router()
        .oneshot(identified(
            Request::get("/sync/v1/catchup/chunk?index=0")
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    let (status, body) = body_json(reply).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["index"], 0);
    assert!(body["hash"].as_str().is_some_and(|h| h.len() == 64));

    // A chunk past the next one is a conflict with the catalogued code and no payload.
    let reply = routes
        .router()
        .oneshot(identified(
            Request::get("/sync/v1/catchup/chunk?index=9")
                .body(Body::empty())
                .unwrap(),
        ))
        .await
        .unwrap();
    let (status, body) = body_json(reply).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["code"], "sync_catchup_serve.chunk_out_of_order");
}

#[tokio::test]
async fn a_hub_without_the_service_refuses_with_the_catalogued_error() {
    for (method, path) in [
        ("POST", "/sync/v1/catchup/route"),
        ("GET", "/sync/v1/catchup/chunk?index=0"),
        ("POST", "/sync/v1/catchup/ack?index=0"),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        let reply = CatchupRoutes::not_ready()
            .router()
            .oneshot(request)
            .await
            .unwrap();
        let (status, body) = body_json(reply).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(body["code"], "sync_catchup_serve.not_ready");
    }
}

// ---- registration --------------------------------------------------------------------------

fn ctx(role: Option<Role>) -> SiteCtx<'static> {
    let mut ctx = SiteCtx::new(
        "wiring-site",
        Duration::from_secs(1),
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
        BTreeMap::new(),
        true,
    );
    ctx.as_module("fake_services", 1000);
    if let Some(role) = role {
        ctx.provide::<ActiveRole>(Arc::new(ActiveRole::new(role)))
            .unwrap();
    }
    ctx
}

#[test]
fn a_standalone_site_or_one_without_a_role_wires_nothing() {
    let mut none = ctx(None);
    register(&mut none).unwrap();
    assert!(none.port::<CatchupRoutes>().is_none());
    let mut standalone = ctx(Some(Role::Standalone));
    register(&mut standalone).unwrap();
    assert!(standalone.port::<CatchupRoutes>().is_none());
    assert!(standalone.port::<CatchupHub>().is_none());
}

#[test]
fn a_hub_missing_its_ports_still_provides_the_not_ready_routes() {
    let mut hub = ctx(Some(Role::Hub));
    register(&mut hub).unwrap();
    assert!(hub.port::<CatchupRoutes>().is_some());
    assert!(
        hub.port::<CatchupHub>().is_none(),
        "nothing serves on a guess"
    );
}

#[test]
fn a_hub_with_its_ports_wires_the_catch_up_hub() {
    let rig = rig(Gap::New, true);
    let mut hub = ctx(Some(Role::Hub));
    let director = Arc::new(
        Director::new(
            &CatchupConfig::default(),
            Arc::new(MemoryProgress::default()),
        )
        .unwrap(),
    );
    hub.provide::<Director>(Arc::clone(&director)).unwrap();
    hub.provide::<ChunkServer>(Arc::new(ChunkServer::new(
        director,
        Arc::new(Snapshot),
        Arc::new(Seams),
        Limits::default(),
    )))
    .unwrap();
    hub.provide::<dyn CatchupFeed>(Arc::new(Feed {
        gap: Gap::New,
        known: true,
    }))
    .unwrap();
    register(&mut hub).unwrap();
    assert!(hub.port::<CatchupHub>().is_some());
    assert!(hub.port::<CatchupRoutes>().is_some());
    let _ = rig;
}

#[test]
fn an_edge_wires_the_node_only_with_an_applier_and_a_connection() {
    let mut bare = ctx(Some(Role::Edge));
    register(&mut bare).unwrap();
    assert!(bare.port::<CatchupNode>().is_none());

    let rig = rig(Gap::New, true);
    let mut edge = ctx(Some(Role::Edge));
    edge.provide::<ChunkApplier>(Arc::new(ChunkApplier::new(
        "hub",
        Arc::new(MemoryProgress::default()),
        Arc::new(Target::default()),
    )))
    .unwrap();
    edge.provide::<dyn ChunkSource>(Arc::new(Loopback {
        hub: Arc::clone(&rig.hub),
        acks: Mutex::default(),
    }))
    .unwrap();
    register(&mut edge).unwrap();
    assert!(edge.port::<CatchupNode>().is_some());
}

#[test]
fn a_disabled_module_wires_nothing() {
    let mut settings = BTreeMap::new();
    settings.insert(
        "sync_catchup_wiring".to_owned(),
        toml::from_str("enabled = false").unwrap(),
    );
    let mut ctx = SiteCtx::new(
        "off-site",
        Duration::from_secs(1),
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
        settings,
        true,
    );
    ctx.as_module("fake_services", 1000);
    ctx.provide::<ActiveRole>(Arc::new(ActiveRole::new(Role::Hub)))
        .unwrap();
    register(&mut ctx).unwrap();
    assert!(ctx.port::<CatchupRoutes>().is_none());
}
