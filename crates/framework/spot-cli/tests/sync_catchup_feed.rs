//! The hub's catch-up feed (WP-AI-07.T16): a hub that has a feed provides `CatchupHub`; `view` of a
//! lagging node reports the gap and the head; `plan` yields a plan the chunk server can cut, in
//! dependency order, scoped and redacted for the node; a node below the outbox, one that never
//! acknowledged anything, one past the head and a gap larger than the bound are bootstraps and
//! plan nothing; an unknown node, an inactive node and a node of another tenant answer `None`; a
//! role other than hub provides no feed. The feed's logic runs over an in-memory source (the
//! PostgreSQL source is exercised by the hub's end-to-end harness), so these tests need no
//! database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use spot_cli::services::SiteCtx;
use spot_cli::services::node_role::{ActiveRole, Role};
use spot_cli::services::sync_catchup_feed::feed::{
    FeedFault, FeedSettings, FeedSource, HubFeed, Links, NodeFacts, NodeScope, RawEvent,
    TOPIC_DOC_STATE, Window,
};
use spot_cli::services::sync_catchup_feed::{self, LazyFeed, Settings};
use spot_cli::services::sync_catchup_wiring::{
    CatchupFeed, CatchupHub, CatchupRoutes, RouteReply, register as register_wiring,
};
use spot_core::ManualClock;
use spot_journal_snapshot::{ImageRecord, SnapshotInfo};
use spot_sync_bootstrap::{HubHead, MemorySource, ScopeView, SnapshotSource};
use spot_sync_catchup::{BundleKind, CatchUpPlan, Mode, RecordId};
use spot_sync_catchup_host::{
    CatchUpChunk, CatchupConfig, ChunkBody, ChunkServer, Director, MemoryProgress, NodeSeams,
    SeamsProvider, ServeError, SnapshotProvider,
};
use spot_sync_closure::{ClosureError, Limits, NoRedaction, RecordKey, References};
use time::OffsetDateTime;

const TENANT: &str = "acme";
const NODE: &str = "edge-khi#branch_01";
const GENERATION: u64 = 2;
const HEAD: u64 = 100;

// ---- the in-memory source ---------------------------------------------------------------------

/// Branch `north` is in the node's scope; `secret` is excluded going down.
struct North;

impl NodeScope for North {
    fn covers(&self, event: &RawEvent) -> bool {
        event.body["image"]["branch"] == "north"
    }

    fn redact(&self, _entity: &str, image: &mut Value) {
        if let Some(object) = image.as_object_mut() {
            object.remove("secret");
        }
    }
}

struct Mem {
    head: Mutex<HubHead>,
    oldest: Mutex<Option<u64>>,
    nodes: Mutex<BTreeMap<String, NodeFacts>>,
    events: Mutex<Vec<RawEvent>>,
    has_scope: AtomicBool,
    failing: AtomicBool,
    reads: AtomicUsize,
}

impl Mem {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            head: Mutex::new(HubHead {
                generation: GENERATION,
                head_position: HEAD,
            }),
            oldest: Mutex::new(Some(1)),
            nodes: Mutex::new(BTreeMap::new()),
            events: Mutex::new(Vec::new()),
            has_scope: AtomicBool::new(true),
            failing: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
        })
    }

    fn set_node(&self, name: &str, applied_prefix: u64, ever_acknowledged: bool) {
        self.nodes.lock().unwrap().insert(
            name.to_owned(),
            NodeFacts {
                applied_prefix,
                ever_acknowledged,
            },
        );
    }

    fn check(&self) -> Result<(), FeedFault> {
        if self.failing.load(Ordering::SeqCst) {
            Err(FeedFault)
        } else {
            Ok(())
        }
    }
}

impl FeedSource for Mem {
    fn head(&self) -> Result<HubHead, FeedFault> {
        self.check()?;
        Ok(*self.head.lock().unwrap())
    }

    fn oldest_position(&self) -> Result<Option<u64>, FeedFault> {
        self.check()?;
        Ok(*self.oldest.lock().unwrap())
    }

    fn node(&self, node: &str) -> Result<Option<NodeFacts>, FeedFault> {
        self.check()?;
        Ok(self.nodes.lock().unwrap().get(node).copied())
    }

    fn events_after(&self, after: u64, max_rows: usize) -> Result<Window, FeedFault> {
        self.check()?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        let rows: Vec<RawEvent> = self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.position > after)
            .cloned()
            .collect();
        if rows.len() > max_rows {
            return Ok(Window {
                events: Vec::new(),
                truncated: true,
            });
        }
        Ok(Window {
            events: rows,
            truncated: false,
        })
    }

    fn scope(
        &self,
        _node: &str,
        _events: &[RawEvent],
    ) -> Result<Option<Box<dyn NodeScope>>, FeedFault> {
        self.check()?;
        Ok(self
            .has_scope
            .load(Ordering::SeqCst)
            .then(|| Box::new(North) as Box<dyn NodeScope>))
    }
}

/// A record whose image names its parent in `parent`.
struct ParentLinks;

impl Links for ParentLinks {
    fn references(&self, _entity: &str, image: &Value) -> Option<Vec<(String, String)>> {
        Some(
            image
                .get("parent")
                .and_then(Value::as_str)
                .map(|id| vec![("note".to_owned(), id.to_owned())])
                .unwrap_or_default(),
        )
    }
}

struct BrokenLinks;

impl Links for BrokenLinks {
    fn references(&self, _entity: &str, _image: &Value) -> Option<Vec<(String, String)>> {
        None
    }
}

fn event(position: u64, id: &str, branch: &str, parent: Option<&str>) -> RawEvent {
    let mut image = json!({"branch": branch, "secret": "s3cr3t", "title": format!("t{position}")});
    if let Some(parent) = parent {
        image["parent"] = json!(parent);
    }
    RawEvent {
        position,
        txn: format!("tx{position}"),
        ordinal: 0,
        topic: TOPIC_DOC_STATE.to_owned(),
        entity: "note".to_owned(),
        id: id.to_owned(),
        version: Some(1),
        schema_version: 1,
        body: json!({"op": "upsert", "entity_schema": 1, "image": image}),
    }
}

/// Positions 41..=100. Every fifth is branch `south` (out of the node's scope, 12 rows); 41 and 91
/// are two versions of record `dup`; the record at 43 names the record at 44 as its parent, which
/// appears later in the stream. In scope: 48 events, 47 distinct records.
fn gap_events() -> Vec<RawEvent> {
    (41..=HEAD)
        .map(|position| {
            let branch = if position % 5 == 0 { "south" } else { "north" };
            let id = if position == 41 || position == 91 {
                "dup".to_owned()
            } else {
                format!("n{position}")
            };
            let parent = (position == 43).then_some("n44");
            event(position, &id, branch, parent)
        })
        .collect()
}

fn planner() -> spot_sync_catchup::Settings {
    CatchupConfig {
        catchup_threshold_events: 10,
        ..CatchupConfig::default()
    }
    .planner()
    .unwrap()
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH))
}

fn feed_over(source: &Arc<Mem>, settings: FeedSettings, clock: &Arc<ManualClock>) -> HubFeed {
    HubFeed::new(
        TENANT,
        Arc::clone(source) as Arc<dyn FeedSource>,
        Arc::new(ParentLinks),
        planner(),
        settings,
        Arc::clone(clock) as Arc<dyn spot_core::Clock>,
    )
}

/// A source with the gap of a node that applied through 40.
fn lagging() -> Arc<Mem> {
    let source = Mem::new();
    source.set_node(NODE, 40, true);
    *source.events.lock().unwrap() = gap_events();
    source
}

// ---- view -------------------------------------------------------------------------------------

#[test]
fn view_of_a_lagging_node_reports_the_gap_and_the_head() {
    let source = lagging();
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    let view = feed.view(NODE).expect("a known node has a view");
    assert_eq!(view.state.node_id, NODE);
    assert_eq!(view.state.tenant, TENANT);
    assert_eq!(view.state.generation, GENERATION);
    assert_eq!(view.state.applied_position, Some(40));
    assert_eq!(
        view.head,
        HubHead {
            generation: GENERATION,
            head_position: HEAD
        }
    );
    let summary = view.summary;
    assert_eq!(summary.applied_prefix, 40);
    assert_eq!(summary.stream_end, HEAD);
    assert_eq!(summary.hot_retention_floor, 0, "the outbox starts at 1");
    assert_eq!(
        summary.in_scope_event_count, 48,
        "the south rows are out of scope"
    );
    assert_eq!(summary.class_counts.state, 48);
    assert_eq!(summary.class_counts.domain, 0);
    assert_eq!(summary.class_counts.audit, 0);
    assert_eq!(summary.estimated_coalesced_records, 47, "dup counts once");
    assert!(summary.estimated_coalesced_bytes > 0);
    assert!(!summary.node_is_new_or_resync);
    summary.validate().unwrap();
}

#[test]
fn a_node_that_is_caught_up_has_an_empty_gap() {
    let source = Mem::new();
    source.set_node(NODE, HEAD, true);
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    let view = feed.view(NODE).unwrap();
    assert_eq!(view.summary.in_scope_event_count, 0);
    assert!(!view.summary.node_is_new_or_resync);
    assert_eq!(source.reads.load(Ordering::SeqCst), 0, "nothing to read");
}

// ---- plan -------------------------------------------------------------------------------------

fn flat_records(plan: &CatchUpPlan) -> Vec<RecordId> {
    plan.bulk
        .iter()
        .flat_map(|bundle| bundle.records.iter().cloned())
        .collect()
}

#[test]
fn the_plan_is_coalesced_scoped_redacted_and_in_dependency_order() {
    let source = lagging();
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    let plan = feed.plan(NODE).expect("a gap the hub holds is planned");
    assert_eq!(plan.mode, Mode::Coalesced);
    assert!(plan.mandatory_lane.is_empty());
    assert!(plan.audit.is_empty());
    assert!(
        plan.bulk
            .iter()
            .all(|bundle| bundle.kind == BundleKind::CoalescedImages)
    );
    let records = flat_records(&plan);
    assert_eq!(
        records.len(),
        47,
        "the latest image of each in-scope record"
    );
    // The out-of-scope records are not in the plan.
    assert!(!records.contains(&RecordId::new("note", "n45")));
    assert!(!records.contains(&RecordId::new("note", "n100")));
    // `dup` is one record, at its latest version.
    assert_eq!(
        records
            .iter()
            .filter(|record| **record == RecordId::new("note", "dup"))
            .count(),
        1
    );
    let latest_dup = plan
        .bulk
        .iter()
        .flat_map(|bundle| bundle.events.iter())
        .find(|event| event.subject == Some(RecordId::new("note", "dup")))
        .unwrap();
    assert_eq!(latest_dup.position, 91);
    // The parent comes before the record that references it, though it is later in the stream.
    let at = |id: &str| {
        records
            .iter()
            .position(|r| *r == RecordId::new("note", id))
            .unwrap()
    };
    assert!(at("n44") < at("n43"), "targets before referrers");
    // Redaction: the excluded field is in no payload; the rest of the image is.
    for event in plan.bulk.iter().flat_map(|bundle| bundle.events.iter()) {
        let text = event.payload.as_str();
        assert!(!text.contains("s3cr3t"), "the excluded field leaked");
        assert!(text.contains("branch"));
        assert_eq!(event.topic, TOPIC_DOC_STATE);
    }
}

#[test]
fn a_plan_the_chunk_server_can_cut() {
    let source = lagging();
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    let plan = feed.plan(NODE).unwrap();
    let (_hub, server, _director) = serving(Arc::new(feed_over(
        &source,
        FeedSettings::default(),
        &clock(),
    )));
    let chunk = server.serve_catch_up(NODE, &plan, 0).unwrap();
    assert!(!chunk.last || plan.bulk.len() == 1);
    match chunk.open().unwrap() {
        ChunkBody::CatchUp(CatchUpChunk::Bulk(bundle)) => {
            assert!(!bundle.records.is_empty());
            assert!(!bundle.events.is_empty());
        }
        other => panic!("a bulk chunk was expected, got {other:?}"),
    }
}

#[test]
fn links_the_metadata_cannot_resolve_make_a_resync_not_a_stuck_catch_up() {
    let source = lagging();
    let feed = HubFeed::new(
        TENANT,
        Arc::clone(&source) as Arc<dyn FeedSource>,
        Arc::new(BrokenLinks),
        planner(),
        FeedSettings::default(),
        clock() as Arc<dyn spot_core::Clock>,
    );
    assert_resync(&feed);
}

// ---- a gap that cannot be planned is a bootstrap --------------------------------------------

fn assert_resync(feed: &HubFeed) {
    let view = feed.view(NODE).expect("the node is known");
    assert!(view.summary.node_is_new_or_resync);
    assert_eq!(view.summary.in_scope_event_count, 0);
    view.summary.validate().unwrap();
    assert!(
        feed.plan(NODE).is_none(),
        "never a plan from a partial read"
    );
}

#[test]
fn a_node_below_the_outbox_never_acknowledged_or_past_the_head_is_a_resync() {
    // Below the outbox: it holds positions from 60, the node applied 40.
    let source = lagging();
    *source.oldest.lock().unwrap() = Some(60);
    assert_resync(&feed_over(&source, FeedSettings::default(), &clock()));
    // Exactly at the edge: applied 59 with the outbox from 60 is still served.
    source.set_node(NODE, 59, true);
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    assert!(!feed.view(NODE).unwrap().summary.node_is_new_or_resync);

    // Never acknowledged anything.
    let fresh = lagging();
    fresh.set_node(NODE, 0, false);
    let feed = feed_over(&fresh, FeedSettings::default(), &clock());
    assert_resync(&feed);
    assert_eq!(feed.view(NODE).unwrap().state.applied_position, None);

    // A prefix past the hub's head (a restored hub, or a node from another history).
    let ahead = lagging();
    ahead.set_node(NODE, HEAD + 50, true);
    let feed = feed_over(&ahead, FeedSettings::default(), &clock());
    assert_resync(&feed);
    assert_eq!(
        feed.view(NODE).unwrap().summary.applied_prefix,
        HEAD,
        "the summary never exceeds the head"
    );
}

#[test]
fn a_gap_larger_than_the_bound_or_with_an_undecodable_event_is_a_resync() {
    let source = lagging();
    let small = FeedSettings {
        max_events: 10,
        ..FeedSettings::default()
    };
    assert_resync(&feed_over(&source, small, &clock()));

    let broken = lagging();
    broken.events.lock().unwrap()[3].body = Value::Null;
    assert_resync(&feed_over(&broken, FeedSettings::default(), &clock()));
}

#[test]
fn a_resync_routes_to_a_bootstrap_through_the_director() {
    let source = lagging();
    *source.oldest.lock().unwrap() = Some(60);
    let feed = Arc::new(feed_over(&source, FeedSettings::default(), &clock()));
    let (hub, _server, _director) = serving(feed);
    assert_eq!(
        hub.route(NODE).unwrap(),
        RouteReply::Bootstrap {
            resume_from_chunk: 0
        }
    );
}

// ---- who is answered ------------------------------------------------------------------------

#[test]
fn an_unknown_node_an_inactive_node_and_a_node_of_another_tenant_answer_none() {
    let source = lagging();
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    // The source answers `None` for a node it does not hold as active in this tenant: unknown,
    // suspended, revoked, without a subscription, or of another tenant (the registry is
    // tenant-scoped).
    for node in ["nobody#branch_09", "other-tenant-node", ""] {
        assert!(feed.view(node).is_none(), "{node:?}");
        assert!(feed.plan(node).is_none(), "{node:?}");
    }
}

#[test]
fn a_node_with_no_scope_is_not_planned() {
    let source = lagging();
    source.has_scope.store(false, Ordering::SeqCst);
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    assert!(feed.view(NODE).is_none());
    assert!(feed.plan(NODE).is_none());
}

#[test]
fn a_store_that_cannot_answer_is_none_and_recovers() {
    let source = lagging();
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    source.failing.store(true, Ordering::SeqCst);
    assert!(feed.view(NODE).is_none());
    assert!(feed.plan(NODE).is_none());
    source.failing.store(false, Ordering::SeqCst);
    assert!(feed.view(NODE).is_some());
    assert!(feed.plan(NODE).is_some());
}

// ---- the cache --------------------------------------------------------------------------------

#[test]
fn one_catch_up_reads_the_gap_once_and_gets_the_same_plan_every_time() {
    let source = lagging();
    let feed = feed_over(&source, FeedSettings::default(), &clock());
    let first = feed.plan(NODE).unwrap();
    let _ = feed.view(NODE).unwrap();
    // The stream moves on while the node is catching up: the plan does not change under it.
    source
        .events
        .lock()
        .unwrap()
        .push(event(101, "late", "north", None));
    source.head.lock().unwrap().head_position = 101;
    let again = feed.plan(NODE).unwrap();
    assert_eq!(first, again);
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
}

#[test]
fn a_moved_prefix_a_new_generation_or_an_idle_node_is_planned_again() {
    let source = lagging();
    let clock = clock();
    let feed = feed_over(&source, FeedSettings::default(), &clock);
    feed.plan(NODE).unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    // The node's applied prefix moved.
    source.set_node(NODE, 50, true);
    feed.plan(NODE).unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 2);
    // A new stream generation.
    source.head.lock().unwrap().generation = GENERATION + 1;
    feed.plan(NODE).unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 3);
    // Idle for longer than `cache_secs`.
    clock.advance(Duration::from_secs(
        FeedSettings::default().cache_secs as u64 + 1,
    ));
    feed.plan(NODE).unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 4);
    // Used within `cache_secs`: kept, and the use renews it.
    clock.advance(Duration::from_secs(60));
    feed.plan(NODE).unwrap();
    clock.advance(Duration::from_secs(60));
    feed.plan(NODE).unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 4);
}

#[test]
fn the_cache_keeps_at_most_max_nodes() {
    let source = lagging();
    source.set_node("a-node#one", 40, true);
    source.set_node("a-node#two", 40, true);
    let settings = FeedSettings {
        max_nodes: 1,
        ..FeedSettings::default()
    };
    let feed = feed_over(&source, settings, &clock());
    feed.plan("a-node#one").unwrap();
    feed.plan("a-node#two").unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 2);
    // `one` was evicted by `two`: asking again reads again.
    feed.plan("a-node#one").unwrap();
    assert_eq!(source.reads.load(Ordering::SeqCst), 3);
}

// ---- the lazy port, the wiring and the role ---------------------------------------------------

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
    fn for_node(&self, _node: &str) -> Option<NodeSeams> {
        Some(NodeSeams {
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
            position: HEAD,
            generation: GENERATION,
            verified: true,
            taken_at_millis: 0,
        };
        let records = (0..3)
            .map(|n| ImageRecord {
                entity: "note".to_owned(),
                id: format!("n{n}"),
                version: 1,
                entity_schema: 1,
                image: json!({"title": n}),
            })
            .collect();
        Ok(Box::new(MemorySource::new(TENANT, info, records)))
    }
}

/// The hub's real director and chunk server, over `feed`.
fn serving(feed: Arc<dyn CatchupFeed>) -> (Arc<CatchupHub>, Arc<ChunkServer>, Arc<Director>) {
    let config = CatchupConfig {
        catchup_threshold_events: 10,
        ..CatchupConfig::default()
    };
    let director = Arc::new(Director::new(&config, Arc::new(MemoryProgress::default())).unwrap());
    let server = Arc::new(ChunkServer::new(
        Arc::clone(&director),
        Arc::new(Snapshot),
        Arc::new(Seams),
        Limits::default(),
    ));
    let hub = Arc::new(CatchupHub::new(
        Arc::clone(&director),
        Arc::clone(&server),
        feed,
        clock(),
    ));
    (hub, server, director)
}

#[test]
fn a_catch_up_runs_end_to_end_over_the_feed() {
    let source = lagging();
    let feed = Arc::new(feed_over(&source, FeedSettings::default(), &clock()));
    let (hub, _server, _director) = serving(feed);
    assert_eq!(
        hub.route(NODE).unwrap(),
        RouteReply::CatchUp {
            resume_from_chunk: 0
        }
    );
    let mut index = 0_u64;
    loop {
        let chunk = hub.chunk(NODE, index).unwrap();
        let last = chunk.last;
        hub.ack(NODE, index).unwrap();
        if last {
            break;
        }
        index += 1;
        assert!(index < 1_000, "the plan ends");
    }
}

fn ctx(
    label: &str,
    role: Option<Role>,
    settings: BTreeMap<String, toml::Value>,
) -> SiteCtx<'static> {
    let mut ctx = SiteCtx::new(label, Duration::from_secs(1), clock(), settings, true);
    ctx.as_module("fake_services", 1000);
    if let Some(role) = role {
        ctx.provide::<ActiveRole>(Arc::new(ActiveRole::new(role)))
            .unwrap();
    }
    ctx.as_module(sync_catchup_feed::NAME, sync_catchup_feed::ORDER);
    ctx
}

#[test]
fn only_a_hub_provides_a_feed() {
    for (label, role) in [
        ("feed-none", None),
        ("feed-standalone", Some(Role::Standalone)),
        ("feed-edge", Some(Role::Edge)),
    ] {
        let mut site = ctx(label, role, BTreeMap::new());
        sync_catchup_feed::register(&mut site).unwrap();
        assert!(site.port::<dyn CatchupFeed>().is_none(), "{label}");
    }
    let mut hub = ctx("feed-hub", Some(Role::Hub), BTreeMap::new());
    sync_catchup_feed::register(&mut hub).unwrap();
    assert!(hub.port::<dyn CatchupFeed>().is_some());
    assert!(sync_catchup_feed::hosted("feed-hub").is_some());
}

#[test]
fn a_disabled_module_provides_no_feed() {
    let mut settings = BTreeMap::new();
    let mut table = toml::map::Map::new();
    table.insert("enabled".to_owned(), toml::Value::Boolean(false));
    settings.insert(
        sync_catchup_feed::NAME.to_owned(),
        toml::Value::Table(table),
    );
    let mut hub = ctx("feed-off", Some(Role::Hub), settings);
    sync_catchup_feed::register(&mut hub).unwrap();
    assert!(hub.port::<dyn CatchupFeed>().is_none());
}

#[test]
fn a_reload_provides_the_same_feed_and_a_second_provider_is_refused() {
    let mut first = ctx("feed-reload", Some(Role::Hub), BTreeMap::new());
    sync_catchup_feed::register(&mut first).unwrap();
    let provided = first.port::<dyn CatchupFeed>().unwrap();
    let mut reload = SiteCtx::new(
        "feed-reload",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        false,
    );
    reload.as_module(sync_catchup_feed::NAME, sync_catchup_feed::ORDER);
    sync_catchup_feed::register(&mut reload).unwrap();
    let again = reload.port::<dyn CatchupFeed>().unwrap();
    assert!(Arc::ptr_eq(&provided, &again));
    // Another module providing the port first makes this one a startup error naming both.
    let mut clash = ctx("feed-clash", Some(Role::Hub), BTreeMap::new());
    clash.as_module("another_feed", 1100);
    clash
        .provide::<dyn CatchupFeed>(LazyFeed::new() as Arc<dyn CatchupFeed>)
        .unwrap();
    clash.as_module(sync_catchup_feed::NAME, sync_catchup_feed::ORDER);
    let refused = sync_catchup_feed::register(&mut clash)
        .unwrap_err()
        .to_string();
    assert!(refused.contains("another_feed"), "{refused}");
    assert!(refused.contains(sync_catchup_feed::NAME), "{refused}");
}

#[test]
fn a_hub_with_a_feed_provides_the_catch_up_hub() {
    let mut hub = ctx("feed-wiring", Some(Role::Hub), BTreeMap::new());
    let director = Arc::new(
        Director::new(
            &CatchupConfig::default(),
            Arc::new(MemoryProgress::default()),
        )
        .unwrap(),
    );
    hub.as_module("sync_catchup", 1150);
    hub.provide::<Director>(Arc::clone(&director)).unwrap();
    hub.as_module("sync_catchup_serve", 1155);
    hub.provide::<ChunkServer>(Arc::new(ChunkServer::new(
        director,
        Arc::new(Snapshot),
        Arc::new(Seams),
        Limits::default(),
    )))
    .unwrap();
    hub.as_module(sync_catchup_feed::NAME, sync_catchup_feed::ORDER);
    sync_catchup_feed::register(&mut hub).unwrap();
    hub.as_module("sync_catchup_wiring", 1160);
    register_wiring(&mut hub).unwrap();
    assert!(
        hub.port::<CatchupHub>().is_some(),
        "the feed completes the wiring"
    );
    assert!(hub.port::<CatchupRoutes>().is_some());
}

#[test]
fn an_unbound_feed_answers_none_until_the_real_one_is_bound() {
    let lazy = LazyFeed::new();
    assert!(!lazy.is_bound());
    assert!(lazy.view(NODE).is_none());
    assert!(lazy.plan(NODE).is_none());
    let source = lagging();
    lazy.bind(Arc::new(feed_over(
        &source,
        FeedSettings::default(),
        &clock(),
    )));
    assert!(lazy.is_bound());
    assert!(lazy.view(NODE).is_some());
    assert!(lazy.plan(NODE).is_some());
}

#[test]
fn the_settings_are_bounded() {
    assert!(Settings::default().resolve().is_ok());
    let bad = |edit: &dyn Fn(&mut Settings)| {
        let mut settings = Settings::default();
        edit(&mut settings);
        settings.resolve().unwrap_err().to_string()
    };
    assert!(bad(&|s| s.max_events = 99).contains("max_events"));
    assert!(bad(&|s| s.max_events = 50_001).contains("max_events"));
    assert!(bad(&|s| s.cache_secs = 4).contains("cache_secs"));
    assert!(bad(&|s| s.max_nodes = 0).contains("max_nodes"));
    assert!(bad(&|s| s.max_nodes = 1_025).contains("max_nodes"));
    let resolved = Settings {
        snapshot_bytes: Some(0),
        ..Settings::default()
    }
    .resolve()
    .unwrap();
    assert_eq!(resolved.snapshot_bytes, None, "0 is unknown");
}

#[test]
fn the_order_sits_between_the_chunk_server_and_the_wiring() {
    const { assert!(sync_catchup_feed::ORDER > spot_cli::services::sync_catchup_serve::ORDER) };
    const { assert!(sync_catchup_feed::ORDER < spot_cli::services::sync_catchup_wiring::ORDER) };
}
