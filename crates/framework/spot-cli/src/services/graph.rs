//! The business graph of one site: the per-site graph index, its outbox feed, and the production
//! `GraphPort` behind `POST /api/v1/graph/query` (`graph_query`, WP-GRA-02.T2).
//!
//! - **Index** (`spot-graph-index`): one [`IndexStore`] per site under `index_memory_mb` (`0`: no
//!   index, every query runs over the database). Built from `spot_graph_edge` off to the side and
//!   swapped in (queries answer from the database while it builds), kept current by a consumer of
//!   the outbox topic `sys_graph_edge` (the projection crate's decoder), applied in order. A decode
//!   error, gap or apply error marks the graph stale (queries use the database) and a rebuild
//!   follows. A scheduled drift check (`check_equal`) rebuilds on a mismatch. Every node keeps its
//!   own index in its own memory: there is no lease.
//! - **Executors.** A query runs as the requesting user: over `IndexExecutor` (database source
//!   reading node facts through the permission engine) when the index is hot and the query follows
//!   no relation edge, over `StorageExecutor` otherwise. The index holds structure only, never a
//!   permission, so both give identical answers.
//! - **Port** ([`GraphPort`]): the hard limits are checked again, the request becomes the crate's
//!   typed query, runs under a cooperative cost budget (nodes, depth, wall time), and answers with
//!   `truncated` (`depth`, `nodes`) and `withheld` as the crate reports. A start record the caller
//!   may not read and one that does not exist are one `graph.not_found`. Edge properties appear
//!   only when asked, per field of the relation DocType the caller may read ([`EdgeAccess`]).
//! - **Host port** ([`GraphHost`]): lets later modules run a typed query as a user.
//!
//! Settings `[services.graph]`: `enabled`, `index_memory_mb` (256; `0` or 16..8192), `graph_name`,
//! `feed_interval_ms`, `drift_interval_secs`, `max_nodes`, `query_budget_ms`. Level: optional.
//! Audit target `spot::audit::graph`: who, counts, refusal codes; never a value or record id.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value as Json;
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_edge_pg::convert::{EdgeRow, row_to_edge};
use spot_graph_index::{
    Edge as IndexEdge, EdgeKey, GraphIndex, GraphKey, GraphSource, IndexExecutor, IndexStore,
    MemoryBudget, check_equal,
};
use spot_graph_model::GraphModel;
use spot_graph_projection::decode_record;
use spot_graph_query::{
    Direction, EdgeFieldAccess, EdgeSelector, FactFields, GraphExecutor, GraphQuery,
    GraphQueryError, GraphResult, Include, Invocation, NodeData, NodeSource, Order, Step,
    StorageExecutor,
};
use spot_http::api::Ports;
use spot_http::api::codec::parse_record_id;
use spot_http::api::ports::graph::{
    CompareOpIn, DirectionIn, EdgeOut, EdgePropertyOut, FilterIn, GraphAnswerOut, GraphPort,
    GraphQueryIn, GraphResult as PortResult, GraphTruncationOut, LineagePort, LiteralIn,
    NodeRef as WireNode, Unconfigured,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_permissions::{FieldFacts, FieldLevel, PermissionEngine, Principal};
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, ChangeFeed as _, ErrorKind, Expr, FeedPosition, FieldRef, Isolation, Name, NodeRef, RowLock,
    StorageBackend, Transaction, Traversal, TxOptions, Value,
};
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::{Served, SiteLive};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services and the report module (1020).
pub const ORDER: u32 = 1200;

/// A result whose error is, by default, the contract's [`ApiError`].
type Res<T, E = ApiError> = std::result::Result<T, E>;

const AUDIT_TARGET: &str = "spot::audit::graph";

/// The edge table every site's database holds (the crate `spot-edge-pg` owns it).
const SCAN_EDGES: &str = "SELECT edge_type, owner_entity, owner_id, from_entity, from_id, \
    to_entity, to_id, properties FROM spot_graph_edge ORDER BY edge_type, from_entity, from_id, to_entity, to_id";

/// `[services.graph]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
    index_memory_mb: u32,
    graph_name: String,
    feed_interval_ms: u64,
    drift_interval_secs: u64,
    max_nodes: u32,
    query_budget_ms: u64,
}

#[rustfmt::skip]
impl Default for Settings {
    fn default() -> Self {
        let graph_name = "business".to_owned();
        Self { enabled: true, index_memory_mb: 256, graph_name, feed_interval_ms: 500, drift_interval_secs: 300, max_nodes: 10_000, query_budget_ms: 5_000 }
    }
}

#[rustfmt::skip]
impl Settings {
    fn check(&self) -> Result<()> {
        let ok = (self.index_memory_mb == 0 || (16..=8192).contains(&self.index_memory_mb))
            && (1_000..=100_000).contains(&self.max_nodes)
            && self.feed_interval_ms > 0 && self.drift_interval_secs > 0 && self.query_budget_ms > 0
            && (1..=63).contains(&self.graph_name.len());
        if ok { Ok(()) } else { Err(Error("services.graph: a setting is out of range".to_owned())) }
    }
}

/// How long the drift scan rests between two pages of documents.
const VERIFY_PAGE_PAUSE: Duration = Duration::from_millis(20);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

/// Counters and the latency histogram of one site's graph (site-level only).
#[derive(Debug, Default)]
#[allow(dead_code)] // read by the operator listing (open question: no metrics hook yet)
struct Stats {
    hot: AtomicU64,
    fallback: AtomicU64,
    rebuilds: AtomicU64,
    gaps: AtomicU64,
    drifts: AtomicU64,
    latency: [AtomicU64; 6],
}

impl Stats {
    fn observe(&self, ms: u64) {
        let i = [5, 25, 100, 500, 5_000]
            .iter()
            .position(|l| ms <= *l)
            .unwrap_or(5);
        self.latency[i].fetch_add(1, Ordering::Relaxed);
    }
}

/// The caller of a query: who, and the principal the permission engine answers for.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The user's id (for the audit line, never a metric label).
    pub user: String,
    /// The principal the engine decides for.
    pub principal: Principal,
}

/// What later modules use to run a typed query over the hosted graph, as a user.
pub trait GraphHost: Send + Sync {
    /// The caller the credential names; anything else fails closed.
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, PortResult<Caller>>;

    /// Runs `query` as `caller` over the hot index when it can, over the database otherwise.
    fn execute<'a>(
        &'a self,
        caller: &'a Caller,
        query: &'a GraphQuery,
    ) -> BoxFuture<'a, Res<GraphResult, GraphQueryError>>;

    /// The graph model of the metadata served now (`None` before any is served).
    fn model(&self) -> Option<Arc<GraphModel>>;

    /// The permission engine of the metadata served now.
    fn engine(&self) -> Option<Arc<PermissionEngine>>;
}

/// A [`NodeSource`] over one read transaction: node values from storage, link steps from the
/// storage traversal. Used under the index executor, where it answers values and every step the
/// index does not.
struct PgSource<T> {
    tx: T,
    facts: FactFields,
}

fn text_of(values: &BTreeMap<Name, Value>, field: &Option<Name>) -> Option<String> {
    match field.as_ref().and_then(|f| values.get(f)) {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

impl<T: Transaction> NodeSource for PgSource<T> {
    async fn load(&mut self, node: &NodeRef) -> Res<Option<NodeData>, GraphQueryError> {
        let Some(document) = self.tx.load(&node.entity, node.id, RowLock::None).await? else {
            return Ok(None);
        };
        let values = document.data.fields;
        Ok(Some(NodeData {
            state: text_of(&values, &self.facts.state),
            created_by: text_of(&values, &self.facts.created_by),
            values,
        }))
    }

    #[rustfmt::skip]
    async fn step(&mut self, node: &NodeRef, step: &Step, max_nodes: u32) -> Res<Vec<NodeRef>, GraphQueryError> {
        let traversal = Traversal { start: vec![node.clone()], edges: vec![step.spec.clone()], direction: step.direction, max_depth: 1, max_nodes };
        let result = self.tx.traverse(&traversal).await?;
        Ok(result.nodes.into_iter().filter(|r| r.depth == 1).map(|r| r.node).collect())
    }
}

/// The caller's field-level access to edge properties: a property is readable only when the
/// caller may read the field of the relation DocType that holds it, unmasked. An edge type with
/// no relation DocType, and a name the model does not know, expose nothing.
pub struct EdgeAccess {
    engine: Arc<PermissionEngine>,
    principal: Principal,
    model: Arc<GraphModel>,
}

impl EdgeFieldAccess for EdgeAccess {
    #[rustfmt::skip]
    fn edge_field_readable(&self, edge_type: &str, property: &Name) -> bool {
        let Some(relation) = self
            .model
            .edge(edge_type)
            .and_then(|edge| edge.relation.as_ref())
        else {
            return false;
        };
        let snapshot = self.engine.snapshot();
        let facts = snapshot
            .facts(&relation.doctype)
            .as_deref()
            .and_then(|f| f.field(property.as_str()))
            .cloned()
            .unwrap_or_else(|| FieldFacts::plain(property.as_str()));
        let doc = spot_permissions::DocFacts { doctype: relation.doctype.clone(), id: None, created_by: None, state: None, values: Default::default() };
        let decision = self
            .engine
            .field_access(&self.principal, &relation.doctype, &facts, &doc);
        matches!(decision.level, FieldLevel::Read | FieldLevel::ReadReveal)
    }
}

/// The link, table and dynamic-link edges read from the edge table in one snapshot, as a
/// [`GraphSource`]. Relation edges are not indexed: they carry properties the index cannot cut per
/// caller, so a query that follows one runs over the database.
struct Snapshot {
    edges: Vec<IndexEdge>,
    watermark: u64,
}

impl GraphSource for Snapshot {
    fn stream_edges(
        &self,
        _key: &GraphKey,
    ) -> Res<Vec<IndexEdge>, spot_graph_index::GraphIndexError> {
        Ok(self.edges.clone())
    }

    fn watermark(&self, _key: &GraphKey) -> Res<u64, spot_graph_index::GraphIndexError> {
        Ok(self.watermark)
    }
}

/// What is derived from one served snapshot: rebuilt when the snapshot is swapped.
struct Prepared {
    served: Arc<Served>,
    model: Arc<GraphModel>,
    catalog: Arc<Catalog>,
    /// The canonical edge-type name by id, to read the edge table.
    names: BTreeMap<String, String>,
}

/// The feed consumer's position, held across awaits (one consumer, one rebuild at a time).
struct Feed {
    position: FeedPosition,
    /// Set by an error on the feed: the next turn rebuilds before applying anything.
    needs_rebuild: bool,
}

struct Shared {
    site: String,
    settings: Settings,
    live: Arc<SiteLive>,
    backend: PgBackend,
    key: GraphKey,
    store: Mutex<IndexStore>,
    tick: AtomicU64,
    feed: tokio::sync::Mutex<Feed>,
    prepared: Mutex<Option<Arc<Prepared>>>,
    building: AtomicBool,
    ready: AtomicBool,
    /// Until this instant a failed build is not retried (a budget the graph does not fit).
    retry_after: Mutex<Option<tokio::time::Instant>>,
    stats: Stats,
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::GraphUnavailable)
}

fn not_found() -> ApiError {
    ApiError::from_code(Code::GraphNotFound)
}

fn too_large(path: &str) -> ApiError {
    ApiError::from_code(Code::GraphTooLarge).detail("path", Json::from(path))
}

fn invalid(path: &str) -> ApiError {
    ApiError::from_code(Code::GraphQueryInvalid).detail("path", Json::from(path))
}

impl Shared {
    fn prepared(&self) -> Res<Arc<Prepared>, ApiError> {
        let served = self.live.served().ok_or_else(unavailable)?;
        let mut cached = lock(&self.prepared);
        if let Some(p) = cached.as_ref()
            && Arc::ptr_eq(&p.served, &served)
        {
            return Ok(Arc::clone(p));
        }
        let catalog = Arc::new(crate::schema::catalog(&served.meta).map_err(|_| unavailable())?);
        let model = Arc::new(GraphModel::derive(&served.meta));
        let names = model
            .edges
            .values()
            .filter(|edge| edge.relation.is_none())
            .map(|edge| (edge.edge_id.as_str().to_owned(), edge.name.clone()))
            .collect();
        let prepared = Arc::new(Prepared {
            served,
            model,
            catalog,
            names,
        });
        *cached = Some(Arc::clone(&prepared));
        Ok(prepared)
    }

    fn now(&self) -> u64 {
        self.tick.fetch_add(1, Ordering::Relaxed)
    }

    /// The hot graph, when there is a fresh one. When there is none, a build starts off to the
    /// side (once at a time, not while a failed build is backing off).
    fn hot(self: &Arc<Self>) -> Option<Arc<GraphIndex>> {
        if self.settings.index_memory_mb == 0 || !self.ready.load(Ordering::Acquire) {
            return None;
        }
        let found = lock(&self.store).get(&self.key, self.now());
        match found {
            Some(index) if !index.is_stale() => Some(index),
            _ => {
                self.kick_rebuild();
                None
            }
        }
    }

    fn kick_rebuild(self: &Arc<Self>) {
        let backing_off =
            lock(&self.retry_after).is_some_and(|until| tokio::time::Instant::now() < until);
        if backing_off || self.building.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this.rebuild().await;
            if let Err(why) = &result {
                tracing::warn!(site = %this.site, %why, "the graph index could not be built; queries use the database");
                *lock(&this.retry_after) =
                    Some(tokio::time::Instant::now() + Duration::from_secs(60));
            }
            this.building.store(false, Ordering::Release);
        });
    }

    /// Reads every edge of the edge table in one repeatable-read snapshot.
    async fn read_edges(&self, prepared: &Prepared) -> Res<Vec<IndexEdge>, String> {
        let label = Name::new("graph_snapshot").map_err(|e| e.to_string())?;
        let mut tx = self
            .backend
            .begin(
                Arc::clone(&prepared.catalog),
                TxOptions::new(label, Isolation::RepeatableRead),
            )
            .await
            .map_err(|e| e.to_string())?;
        let rows = tx.raw_query(SCAN_EDGES, &[], None).await;
        let _ = tx.rollback().await;
        let mut edges = Vec::new();
        for row in rows.map_err(|e| e.to_string())? {
            let get = |e: tokio_postgres::Error| e.to_string();
            let decoded = row_to_edge(EdgeRow {
                edge_type: row.try_get(0).map_err(get)?,
                owner_entity: row.try_get(1).map_err(get)?,
                owner_id: row.try_get(2).map_err(get)?,
                from_entity: row.try_get(3).map_err(get)?,
                from_id: row.try_get(4).map_err(get)?,
                to_entity: row.try_get(5).map_err(get)?,
                to_id: row.try_get(6).map_err(get)?,
                properties: row.try_get(7).map_err(get)?,
            })
            .map_err(|e| e.to_string())?;
            let Some(name) = prepared.names.get(decoded.edge_type.as_str()) else {
                continue;
            };
            edges.push(IndexEdge::bare(EdgeKey::new(
                name.clone(),
                decoded.from,
                decoded.to,
            )));
        }
        Ok(edges)
    }

    /// The outbox position after the last entry now: the feed resumes there. Read before the
    /// snapshot, so an event committed between the two is applied twice (idempotent), never lost.
    async fn outbox_tail(&self, mut position: FeedPosition) -> Res<FeedPosition, String> {
        loop {
            let records = self
                .backend
                .read_outbox(position, 1000)
                .await
                .map_err(|e| e.to_string())?;
            match records.last() {
                Some(last) => position = last.position,
                None => return Ok(position),
            }
        }
    }

    /// Builds the graph off to the side and swaps it in.
    async fn rebuild(&self) -> Res<(), String> {
        let mut feed = self.feed.lock().await;
        let prepared = self
            .prepared()
            .map_err(|_| "no metadata is served".to_owned())?;
        let tail = self.outbox_tail(feed.position).await?;
        let edges = self.read_edges(&prepared).await?;
        let snapshot = Snapshot {
            edges,
            watermark: 0,
        };
        lock(&self.store)
            .rebuild(&self.key, &snapshot, self.now())
            .map_err(|e| e.to_string())?;
        feed.position = tail;
        feed.needs_rebuild = false;
        self.stats.rebuilds.fetch_add(1, Ordering::Relaxed);
        tracing::info!(site = %self.site, edges = snapshot.edges.len(), "graph index built");
        Ok(())
    }

    /// Reads the outbox from the stored position and applies the edge events in order.
    async fn consume(&self) -> Res<(), String> {
        let mut feed = self.feed.lock().await;
        if feed.needs_rebuild {
            drop(feed);
            return self.rebuild().await;
        }
        let records = self
            .backend
            .read_outbox(feed.position, 500)
            .await
            .map_err(|e| e.to_string())?;
        let Some(last) = records.last().map(|r| r.position) else {
            return Ok(());
        };
        let prepared = self
            .prepared()
            .map_err(|_| "no metadata is served".to_owned())?;
        let linkish: std::collections::BTreeSet<&String> = prepared.names.values().collect();
        for record in &records {
            let decoded = decode_record(record);
            let mut event = match decoded {
                Ok(Some(event)) => event,
                Ok(None) => continue,
                Err(_) => {
                    self.fail_feed(&mut feed);
                    break;
                }
            };
            if event.graph != self.key || !linkish.contains(&&event.edge.edge_type) {
                continue;
            }
            let mut store = lock(&self.store);
            let Some(index) = store.peek(&self.key).cloned() else {
                continue;
            };
            event.sequence = index.watermark() + 1;
            if store.apply(&event, self.now()).is_err() {
                drop(store);
                self.fail_feed(&mut feed);
                break;
            }
        }
        feed.position = last;
        Ok(())
    }

    fn fail_feed(&self, feed: &mut Feed) {
        self.stats.gaps.fetch_add(1, Ordering::Relaxed);
        feed.needs_rebuild = true;
        tracing::warn!(site = %self.site, "graph feed error: the index is rebuilt");
    }

    /// The edge table must equal the documents' link set (the documents are authoritative): scan
    /// the documents, and when the table has lost or kept an edge no event explains, repair it
    /// from the drift report. The index then follows the table through the comparison below.
    async fn verify_and_repair_edges(&self) -> Res<(), String> {
        let prepared = self
            .prepared()
            .map_err(|_| "no metadata is served".to_owned())?;
        let schema = spot_edge_sync::EdgeSchema::derive(&prepared.served.meta, &[]);
        let now = time::OffsetDateTime::now_utc();
        let key = format!("graph-drift-{}", spot_storage_contract::RecordId::new_v7());
        let job = spot_edge_pg::verify::EdgeVerifyJob::new(
            self.backend.clone(),
            Arc::clone(&prepared.catalog),
            schema,
        )
        // One connection at a time, one short transaction per page, a pause between pages: the
        // scan of a large tenant must never take the pool from the requests being served.
        .with_pause(VERIFY_PAGE_PAUSE);
        let report = job.run(now, &key).await.map_err(|e| e.to_string())?;
        if report.is_clean() {
            let _ = job.discard_report(&key).await;
            return Ok(());
        }
        tracing::warn!(site = %self.site, drift = report.entries.len(), "graph edge table drifted from the documents: repairing");
        let repaired = spot_edge_pg::repair::EdgeRepair::new(self.backend.clone(), Arc::clone(&prepared.catalog))
            .with_pause(VERIFY_PAGE_PAUSE)
            .repair(now, &key)
            .await
            .map_err(|e| e.to_string());
        // The report only drove this repair; a tenant whose edge table is far behind would
        // otherwise keep one row per missing edge for every check.
        let _ = job.discard_report(&key).await;
        repaired?;
        Ok(())
    }

    /// Compares the index with the edge table; a mismatch rebuilds.
    async fn drift_check(&self) -> Res<(), String> {
        self.consume().await?;
        self.verify_and_repair_edges().await?;
        let feed = self.feed.lock().await;
        let index = lock(&self.store).peek(&self.key).cloned();
        let Some(index) = index else { return Ok(()) };
        let prepared = self
            .prepared()
            .map_err(|_| "no metadata is served".to_owned())?;
        let edges = self.read_edges(&prepared).await?;
        drop(feed);
        let snapshot = Snapshot {
            edges,
            watermark: index.watermark(),
        };
        let drift = check_equal(&index, &snapshot).map_err(|e| e.to_string())?;
        if drift.is_empty() {
            return Ok(());
        }
        self.stats.drifts.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(site = %self.site, drift = drift.len(), "graph index drift: rebuilding");
        self.rebuild().await
    }

    async fn caller(&self, ctx: &RequestCtx) -> PortResult<Caller> {
        if !self.ready.load(Ordering::Acquire) {
            return Err(unavailable());
        }
        if ctx.site.as_str() != self.site {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        let principals = self.live.principals().ok_or_else(unavailable)?;
        let principal = DocPrincipals::resolve(&*principals, &who.user_id).await;
        Ok(Caller {
            user: who.user_id.to_string(),
            principal,
        })
    }

    async fn run_query(
        self: &Arc<Self>,
        caller: &Caller,
        query: &GraphQuery,
    ) -> Res<GraphResult, GraphQueryError> {
        let unavailable = |why: String| {
            GraphQueryError::Storage(spot_storage_contract::StorageError::new(
                ErrorKind::StatementTimeout,
                why,
            ))
        };
        let prepared = self
            .prepared()
            .map_err(|_| unavailable("no metadata is served".to_owned()))?;
        let served = &prepared.served;
        let who = Invocation {
            principal: &caller.principal,
            engine: &served.decision,
        };
        let access: Arc<dyn EdgeFieldAccess> = Arc::new(EdgeAccess {
            engine: Arc::clone(&served.decision),
            principal: caller.principal.clone(),
            model: Arc::clone(&prepared.model),
        });
        let label = Name::new("graph_query").map_err(|e| unavailable(e.to_string()))?;
        let tx = self
            .backend
            .begin(
                Arc::clone(&prepared.catalog),
                TxOptions::new(label, Isolation::RepeatableRead),
            )
            .await?;
        let facts = FactFields {
            state: None,
            created_by: Name::new("owner").ok(),
        };
        let relation = query.follow.iter().any(|selector| {
            prepared
                .model
                .edge(&selector.edge)
                .is_some_and(|edge| edge.relation.is_some())
        });
        let hot = if relation { None } else { self.hot() };
        if hot.is_some() {
            self.stats.hot.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.fallback.fetch_add(1, Ordering::Relaxed);
        }
        match hot {
            Some(index) => {
                let mut executor = IndexExecutor::hot(
                    PgSource { tx, facts },
                    Arc::clone(&prepared.model),
                    index,
                    access,
                );
                let result = executor.execute(query, &who).await;
                let _ = executor.into_source().tx.rollback().await;
                result
            }
            None => {
                let mut executor =
                    StorageExecutor::new(tx, Arc::clone(&prepared.model), facts, access);
                let result = executor.execute(query, &who).await;
                let _ = executor.into_transaction().rollback().await;
                result
            }
        }
    }
}

fn name_of(text: &str, path: &str) -> Res<Name, ApiError> {
    Name::new(text).map_err(|_| invalid(path))
}

fn node_of(node: &WireNode, path: &str) -> Res<NodeRef, ApiError> {
    Ok(NodeRef {
        entity: name_of(&node.doctype, path)?,
        id: parse_record_id(&node.id).ok_or_else(|| invalid(path))?,
    })
}

#[rustfmt::skip]
fn literal(value: &LiteralIn) -> Value {
    match value {
        LiteralIn::Text(text) => Value::Text(text.clone()),
        LiteralIn::Bool(flag) => Value::Bool(*flag),
        LiteralIn::Int(n) => Value::Int(*n),
    }
}

#[rustfmt::skip]
fn expr_of(filter: &FilterIn, alias: &Name, path: &str) -> Res<Expr> {
    use spot_storage_contract::CmpOp;
    let field = |f: &str| -> Res<Box<Expr>> { Ok(Box::new(Expr::Field(FieldRef { alias: alias.clone(), field: name_of(f, path)? }))) };
    let terms = |ts: &[FilterIn]| -> Res<Vec<Expr>> { ts.iter().map(|t| expr_of(t, alias, path)).collect() };
    Ok(match filter {
        FilterIn::Compare { field: f, op, value } => {
            let op = match op {
                CompareOpIn::Eq => CmpOp::Eq, CompareOpIn::Ne => CmpOp::Ne, CompareOpIn::Lt => CmpOp::Lt,
                CompareOpIn::Le => CmpOp::Le, CompareOpIn::Gt => CmpOp::Gt, CompareOpIn::Ge => CmpOp::Ge,
            };
            Expr::Cmp(field(f)?, op, Box::new(Expr::Value(literal(value))))
        }
        FilterIn::In { field: f, values } => Expr::In(field(f)?, values.iter().map(literal).collect()),
        FilterIn::IsNull { field: f, is_null } => {
            let test = Expr::IsNull(field(f)?);
            if *is_null { test } else { Expr::Not(Box::new(test)) }
        }
        FilterIn::And(ts) => Expr::And(terms(ts)?),
        FilterIn::Or(ts) => Expr::Or(terms(ts)?),
        FilterIn::Not(t) => Expr::Not(Box::new(expr_of(t, alias, path)?)),
    })
}

/// The contract's edge-type identifier for a canonical edge-type name: the dot becomes an
/// underscore (`task.project` is `task_project`), so the name matches the contract's pattern.
fn wire_edge(canonical: &str) -> String {
    canonical.replace('.', "_")
}

/// The typed query for `request`. The walk goes one level past the depth asked for, so a node
/// beyond it says the answer was cut by depth; it is not returned.
#[rustfmt::skip]
fn typed_query(request: &GraphQueryIn, model: &GraphModel, walk_nodes: u32) -> Res<GraphQuery> {
    let follow: Vec<EdgeSelector> = if request.edge_types.is_empty() {
        // "Every type the caller may traverse": a multi-select edge has no storage mapping yet
        // (the compiler refuses it), so the default walk leaves it out instead of refusing the
        // whole query; naming one explicitly is still refused.
        model.edges.iter()
            .filter(|(_, edge)| edge.source != spot_graph_model::EdgeSource::TableMultiselect)
            .map(|(name, _)| EdgeSelector::new(name.clone()))
            .collect()
    } else {
        let mut chosen = Vec::new();
        for (i, wire) in request.edge_types.iter().enumerate() {
            let found = model.edges.keys().find(|canonical| wire_edge(canonical) == *wire);
            chosen.push(EdgeSelector::new(found.cloned().ok_or_else(|| invalid(&format!("body.edge_types[{i}]")))?));
        }
        chosen
    };
    if follow.is_empty() {
        return Err(invalid("body.edge_types"));
    }
    let mut builder = GraphQuery::builder()
        .from(vec![node_of(&request.start, "body.start")?])
        .follow(follow)
        .direction(match request.direction {
            DirectionIn::Out => Direction::Out,
            DirectionIn::In => Direction::In,
            DirectionIn::Both => Direction::Both,
        })
        .max_depth(u16::from(request.depth) + 1)
        .max_nodes(walk_nodes)
        .include(Include { fields: Vec::new(), edge_properties: request.return_edge_properties })
        .order(Order::Breadth);
    if let Some(filter) = &request.node_filter {
        builder = builder.node_filter(expr_of(filter, &name_of("n", "body")?, "body.node_filter")?);
    }
    if let Some(filter) = &request.edge_filter {
        let alias = name_of("e", "body")?;
        builder = builder.edge_var(alias.clone()).edge_filter(expr_of(filter, &alias, "body.edge_filter")?);
    }
    builder.build().map_err(|_| invalid("body"))
}

fn property_out(value: &Value) -> Option<EdgePropertyOut> {
    Some(match value {
        Value::Null => EdgePropertyOut::Null,
        Value::Bool(v) => EdgePropertyOut::Bool(*v),
        Value::Int(v) => EdgePropertyOut::Int(*v),
        Value::Decimal(v) => EdgePropertyOut::Text(v.to_string()),
        Value::Text(v) => EdgePropertyOut::Text(v.clone()),
        Value::Date(v) => EdgePropertyOut::Text(v.to_string()),
        Value::Id(v) => EdgePropertyOut::Text(v.to_string()),
        _ => return None,
    })
}

fn wire_node(node: &NodeRef) -> WireNode {
    WireNode::new(node.entity.as_str(), node.id.to_string())
}

/// The contract's answer for a result: nodes at the asked depth and within the limit, the edges
/// between them, `truncated` with its reason, and `withheld` as the crate reports it.
#[rustfmt::skip]
fn answer_of(result: &GraphResult, request: &GraphQueryIn, model: &GraphModel) -> GraphAnswerOut {
    let (depth, limit) = (u16::from(request.depth), usize::try_from(request.limit).unwrap_or(usize::MAX));
    let within: Vec<_> = result.nodes.iter().filter(|n| n.depth <= depth).collect();
    let kept: Vec<_> = within.iter().take(limit).copied().collect();
    let reason = if within.len() > limit {
        Some(GraphTruncationOut::Nodes)
    } else if result.nodes.iter().any(|n| n.depth > depth) {
        Some(GraphTruncationOut::Depth)
    } else {
        None
    };
    let present = |node: &NodeRef| kept.iter().any(|n| n.node == *node);
    let props_of = |e: &spot_graph_query::GraphEdge| -> Option<BTreeMap<String, EdgePropertyOut>> {
        let node = kept.iter().find(|n| n.node == e.to)?;
        let hop = node.path.iter().find(|h| h.node == e.to && h.edge_type == e.edge)?;
        let props: BTreeMap<_, _> = hop.properties.iter().filter_map(|(k, v)| Some((k.as_str().to_owned(), property_out(v)?))).collect();
        (!props.is_empty()).then_some(props)
    };
    let edges = result.edges.iter().filter(|e| present(&e.from) && present(&e.to)).map(|e| {
        let (from, to) = if followed_backwards(e, model, request.direction) { (&e.to, &e.from) } else { (&e.from, &e.to) };
        EdgeOut {
            from: wire_node(from),
            to: wire_node(to),
            kind: wire_edge(&e.edge),
            properties: if request.return_edge_properties { props_of(e) } else { None },
        }
    });
    GraphAnswerOut {
        nodes: kept.iter().map(|n| wire_node(&n.node)).collect(),
        edges: edges.collect(),
        truncated: reason.is_some(),
        truncation_reason: reason,
        withheld: result.truncated_by_permission,
    }
}

/// Whether the traversal crossed `edge` against the edge type's own direction. An edge of the
/// answer is a fact about two records and reads the same whichever way it was walked: `from` is
/// the record that holds the link (the edge type's `from` DocType), `to` the record it names.
/// Only an edge type that joins a DocType to itself needs the walk's direction to tell.
fn followed_backwards(edge: &spot_graph_query::GraphEdge, model: &GraphModel, direction: DirectionIn) -> bool {
    let Some(kind) = model.edge(&edge.edge) else {
        return matches!(direction, DirectionIn::In);
    };
    let forward = edge.from.entity.as_str() == kind.from && kind.to_one().is_none_or(|to| to == edge.to.entity.as_str());
    let backward = edge.to.entity.as_str() == kind.from && kind.to_one().is_none_or(|to| to == edge.from.entity.as_str());
    match (forward, backward) {
        (true, false) => false,
        (false, true) => true,
        _ => matches!(direction, DirectionIn::In),
    }
}

/// A walk that overflowed its budget answers the part found: the deepest level that fits.
fn too_big(error: &GraphQueryError) -> bool {
    matches!(error, GraphQueryError::Storage(e) if e.kind == ErrorKind::TraversalTooLarge)
}

impl Shared {
    async fn graph_query(
        self: &Arc<Self>,
        ctx: &RequestCtx,
        request: &GraphQueryIn,
    ) -> PortResult<GraphAnswerOut> {
        let started = tokio::time::Instant::now();
        let caller = self.caller(ctx).await?;
        let outcome = self.answer(&caller, request).await;
        self.stats
            .observe(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX));
        match &outcome {
            Ok(answer) => tracing::info!(
                target: AUDIT_TARGET, site = %self.site, who = %caller.user, action = "query",
                nodes = answer.nodes.len(), truncated = answer.truncated, withheld = answer.withheld,
                "graph query"
            ),
            Err(error) => tracing::info!(
                target: AUDIT_TARGET, site = %self.site, who = %caller.user, action = "refused",
                code = %error.code.as_str(), "graph query refused"
            ),
        }
        outcome
    }

    async fn answer(
        self: &Arc<Self>,
        caller: &Caller,
        request: &GraphQueryIn,
    ) -> PortResult<GraphAnswerOut> {
        if !(1..=8).contains(&request.depth) {
            return Err(too_large("body.depth"));
        }
        if !(1..=1000).contains(&request.limit) {
            return Err(too_large("body.limit"));
        }
        if request.edge_types.len() > 16 {
            return Err(too_large("body.edge_types"));
        }
        let prepared = self.prepared()?;
        let budget = Duration::from_millis(self.settings.query_budget_ms);
        let query = typed_query(request, &prepared.model, self.settings.max_nodes)?;
        let run = async {
            self.run_query(caller, &query).await.map_err(|e| {
                if too_big(&e) {
                    too_large("body.limit")
                } else {
                    map_error(e)
                }
            })
        };
        let result = tokio::time::timeout(budget, run)
            .await
            .map_err(|_| too_large("body.limit"))??;
        let start = node_of(&request.start, "body.start")?;
        if !result.nodes.iter().any(|n| n.depth == 0 && n.node == start) {
            return Err(not_found());
        }
        Ok(answer_of(&result, request, &prepared.model))
    }
}

/// A query error as the contract's refusal: a bad query is `graph.query_invalid` (the crate's
/// code only, never a value); anything else is `graph.unavailable`.
fn map_error(error: GraphQueryError) -> ApiError {
    match error {
        GraphQueryError::Invalid(_)
        | GraphQueryError::UnknownEdgeType { .. }
        | GraphQueryError::UnsupportedEdge { .. }
        | GraphQueryError::UnknownEdgeProperty { .. } => {
            ApiError::from_code(Code::GraphQueryInvalid).detail("code", Json::from(error.code()))
        }
        other => {
            tracing::warn!(code = other.code(), "a graph query failed");
            unavailable()
        }
    }
}

struct Graph(Arc<Shared>);

impl GraphPort for Graph {
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a GraphQueryIn,
    ) -> BoxFuture<'a, PortResult<GraphAnswerOut>> {
        Box::pin(async move { self.0.graph_query(ctx, query).await })
    }
}

impl GraphHost for Graph {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, PortResult<Caller>> {
        Box::pin(async move { self.0.caller(ctx).await })
    }

    fn execute<'a>(
        &'a self,
        caller: &'a Caller,
        query: &'a GraphQuery,
    ) -> BoxFuture<'a, Res<GraphResult, GraphQueryError>> {
        Box::pin(async move { self.0.run_query(caller, query).await })
    }

    fn model(&self) -> Option<Arc<GraphModel>> {
        self.0.prepared().ok().map(|p| Arc::clone(&p.model))
    }

    fn engine(&self) -> Option<Arc<PermissionEngine>> {
        self.0
            .prepared()
            .ok()
            .map(|p| Arc::clone(&p.served.decision))
    }
}

struct GraphService {
    shared: Arc<Shared>,
}

impl Service for GraphService {
    fn name(&self) -> &'static str {
        "graph"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.shared
                .prepared()
                .map_err(|_| "no metadata is served".to_owned())?;
            self.shared.ready.store(true, Ordering::Release);
            Ok(())
        })
    }

    #[rustfmt::skip]
    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let (shared, mut stop) = (&self.shared, stop);
            let indexed = shared.settings.index_memory_mb != 0;
            let mut feed = tokio::time::interval(Duration::from_millis(shared.settings.feed_interval_ms));
            let mut drift = tokio::time::interval(Duration::from_secs(shared.settings.drift_interval_secs));
            feed.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            drift.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            drift.tick().await;
            if indexed {
                shared.kick_rebuild();
            }
            loop {
                tokio::select! {
                    changed = stop.changed() => if changed.is_err() || *stop.borrow() { break },
                    _ = feed.tick(), if indexed => if let Err(why) = shared.consume().await {
                        tracing::warn!(site = %shared.site, %why, "the graph feed could not be read");
                    },
                    _ = drift.tick(), if indexed => if let Err(why) = shared.drift_check().await {
                        tracing::warn!(site = %shared.site, %why, "the graph drift check failed");
                    },
                }
            }
            shared.ready.store(false, Ordering::Release);
            Ok(())
        })
    }
}

/// The ports of the sites built so far, by site name: a reload builds the site again without
/// building the service again, and its routes must reach the same port.
fn built() -> &'static Mutex<HashMap<String, Arc<Graph>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<Graph>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// Registers the graph: the service, the host port and the `graph_query` / `lineage_trace`
/// routes. The routes are mounted once every module registered: the lineage module (when it
/// runs) provides the `LineagePort` the group needs, and a site without it gets the stub that
/// answers `501 service.not_configured`.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("graph")?;
    settings.check()?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let port = if ctx.assembling() {
        let budget = MemoryBudget::new(u64::from(settings.index_memory_mb) * 1024 * 1024);
        let feed = Feed {
            position: FeedPosition::START,
            needs_rebuild: false,
        };
        #[rustfmt::skip]
        let shared = Arc::new(Shared {
            key: GraphKey::new(site.clone(), settings.graph_name.clone()), site: site.clone(), settings,
            live: Arc::clone(env.live), backend: env.backend.clone(), store: Mutex::new(IndexStore::new(budget)),
            tick: AtomicU64::new(1), feed: tokio::sync::Mutex::new(feed), prepared: Mutex::new(None),
            building: AtomicBool::new(false), ready: AtomicBool::new(false), retry_after: Mutex::new(None),
            stats: Stats::default(),
        });
        ctx.add(Arc::new(GraphService {
            shared: Arc::clone(&shared),
        }));
        let port = Arc::new(Graph(shared));
        lock(built()).insert(site, Arc::clone(&port));
        port
    } else {
        match lock(built()).get(&site).cloned() {
            Some(port) => port,
            None => return Ok(()),
        }
    };
    ctx.provide::<dyn GraphHost>(Arc::clone(&port) as Arc<dyn GraphHost>)?;
    let graph_port = Arc::clone(&port) as Arc<dyn GraphPort>;
    ctx.finally(move |ctx| {
        let lineage = ctx
            .port::<dyn LineagePort>()
            .unwrap_or_else(|| Arc::new(Unconfigured) as Arc<dyn LineagePort>);
        let mut ports = Ports::new();
        ports.provide::<dyn GraphPort>(graph_port);
        ports.provide::<dyn LineagePort>(lineage);
        let mounted = routes::mount(&[routes::graph::registration()], &ports, &[]);
        if let Some((module, reason)) = mounted.report.skipped.first() {
            tracing::error!(
                module = *module,
                ?reason,
                "the graph routes were not mounted"
            );
        }
        let router = mounted.authenticated;
        ctx.router_layer(move |base| base.merge(router));
        Ok(())
    });
    Ok(())
}
