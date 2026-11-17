//! Business rules over the graph for one site (`[services.rule_graph]`, WP-GRA-04.T2), hosted over
//! `spot-rule-graph`.
//!
//! - **Rule sets** are app data: `<app>/rules/<name>.rules` of every installed app (the set's name
//!   is the file stem; a later app wins a name). They are compiled against the metadata served now
//!   when the site starts and recompiled on every metadata swap, by content hash. A set that does
//!   not compile keeps the previous compiled set serving (none at start) and is reported with its
//!   name and stable code only (`spot::rule_graph`, never rule text).
//! - **Evaluation as the principal** ([`RuleGraph::derive`]): the facts are the edges of the edge
//!   types the program reads, kept only when **both ends are records the principal may read**, so a
//!   rule never derives from a record the principal cannot see. A program that reads a field
//!   predicate is refused (`rule_graph.unsupported_field_fact`) until field facts are hosted: it
//!   fails closed, never derives from unfiltered values.
//! - **Limits** are settings (`max_facts`, `fuel`, `max_rows`, `budget_ms`); a breach is a stable
//!   error, never a partial answer.
//! - **Approvers**: [`ApproverResolver`] over a rule `"<set>/<predicate>"`; the first column of each
//!   row is a user name. It runs as `form.requested_by`; with no such principal, an unknown rule, a
//!   failure or no row it answers `None`, so the task creation fails closed.
//!
//! Settings: `enabled` (true), `max_facts` (100,000), `fuel` (1,000,000), `max_rows` (10,000),
//! `budget_ms` (5,000). Level: optional. Metrics are counters in [`Stats`]; no principal is ever a
//! label. Audit target `spot::audit::rule_graph`: who, set, predicate, rows or refusal code.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value as Json;
use spot_bpm_tasks::resolve::ApproverResolver;
use spot_edge_pg::convert::{EdgeRow, row_to_edge};
use spot_http::api::service::BoxFuture;
use spot_permissions::{Action, DocFacts, PermissionEngine, Principal};
use spot_postgres::PgBackend;
use spot_rule_graph::{Datum, Engine, FactSource, PredKind, Query, Registry, Rows};
use spot_storage_contract::{
    Catalog, DocumentStore as _, Isolation, Name, NodeRef, RowLock, StorageBackend, Transaction, TxOptions, Value,
};
use tokio::sync::watch;

use crate::live::SiteLive;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the graph (1200) and lineage (1210).
pub const ORDER: u32 = 1220;

const AUDIT_TARGET: &str = "spot::audit::rule_graph";
const TARGET: &str = "spot::rule_graph";
/// The directory of an app that holds its rule sets.
pub const RULES_DIR: &str = "rules";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
    max_facts: usize,
    fuel: u64,
    max_rows: usize,
    budget_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_facts: 100_000,
            fuel: 1_000_000,
            max_rows: 10_000,
            budget_ms: 5_000,
        }
    }
}

/// Why a derivation did not answer. Each has a stable code and never carries rule text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleError {
    /// No rule set (or predicate) of that name.
    Unknown,
    /// The program reads a field predicate.
    UnsupportedFieldFact,
    /// A limit (facts, fuel, rows or time) was breached.
    Limit,
    /// The facts could not be read, or the evaluation failed.
    Failed,
}

impl RuleError {
    /// The stable code.
    pub fn code(self) -> &'static str {
        match self {
            RuleError::Unknown => "rule_graph.unknown",
            RuleError::UnsupportedFieldFact => "rule_graph.unsupported_field_fact",
            RuleError::Limit => "rule_graph.limit",
            RuleError::Failed => "rule_graph.failed",
        }
    }
}

/// What other modules use to derive a relationship as a principal.
pub trait RuleGraph: Send + Sync {
    /// The tuples of `predicate` in rule set `set`, derived from the facts `principal` may read.
    fn derive<'a>(
        &'a self,
        set: &'a str,
        predicate: &'a str,
        principal: &'a Principal,
    ) -> BoxFuture<'a, std::result::Result<Rows, RuleError>>;

    /// The rule sets that failed their last (re)compile, as `(name, code)`.
    fn failures(&self) -> Vec<(String, String)>;
}

/// Site-level counters (no label holds a principal, a record or a rule text).
#[derive(Debug, Default)]
#[allow(dead_code)] // read by the operator listing (no metrics hook in the services registry yet)
struct Stats {
    compiled: AtomicU64,
    compile_failures: AtomicU64,
    evaluations: AtomicU64,
    limit_breaches: AtomicU64,
}

/// The facts one derivation reads: already filtered to what the principal may read.
struct Facts(BTreeMap<String, Vec<(NodeRef, NodeRef)>>);

impl FactSource for Facts {
    fn edges(&self, edge_type: &str) -> Vec<(NodeRef, NodeRef)> {
        self.0.get(edge_type).cloned().unwrap_or_default()
    }

    fn fields(&self, _doctype: &str, _field: &str) -> Vec<(NodeRef, Value)> {
        Vec::new()
    }
}

/// The sites' running services, by name: a reload builds the site again without building this
/// again, and re-reads the rule files into the same state.
fn built() -> std::sync::MutexGuard<'static, BTreeMap<String, Arc<Shared>>> {
    static BUILT: std::sync::OnceLock<Mutex<BTreeMap<String, Arc<Shared>>>> =
        std::sync::OnceLock::new();
    BUILT
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

struct Shared {
    site: String,
    settings: Settings,
    live: Arc<SiteLive>,
    backend: PgBackend,
    registry: Registry,
    /// The rule sets the apps ship, compiled by `check` once metadata is served.
    sets: Mutex<Vec<(String, String)>>,
    failures: Mutex<BTreeMap<String, String>>,
    stats: Stats,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Shared {
    /// Compiles `sets` (name, text) against the served metadata; a failure keeps what served.
    fn load(&self, sets: &[(String, String)]) {
        let Some(served) = self.live.served() else {
            return;
        };
        for (name, text) in sets {
            match self.registry.register(name.clone(), text, &served.meta) {
                Ok(_) => {
                    self.stats.compiled.fetch_add(1, Ordering::Relaxed);
                    lock(&self.failures).remove(name);
                }
                Err(error) => self.failed(name, error.code()),
            }
        }
    }

    fn failed(&self, name: &str, code: &str) {
        self.stats.compile_failures.fetch_add(1, Ordering::Relaxed);
        lock(&self.failures).insert(name.to_owned(), code.to_owned());
        tracing::warn!(target: TARGET, site = %self.site, set = name, code, "a rule set did not compile; the previous one keeps serving");
    }

    /// The metadata was swapped: recompile every set, by content hash.
    fn swapped(&self) {
        let Some(served) = self.live.served() else {
            return;
        };
        let report = self.registry.reload(&served.meta);
        for failure in &report.failed {
            self.failed(&failure.name, failure.error.code());
        }
    }

    /// The edges of `edge_types` whose both ends the principal may read.
    async fn facts(
        &self,
        edge_types: &BTreeSet<String>,
        principal: &Principal,
        engine: &PermissionEngine,
        catalog: Arc<Catalog>,
    ) -> std::result::Result<Facts, RuleError> {
        let label = Name::new("rule_graph_facts").map_err(|_| RuleError::Failed)?;
        let mut tx = self
            .backend
            .begin(catalog, TxOptions::new(label, Isolation::RepeatableRead))
            .await
            .map_err(|_| RuleError::Failed)?;
        let rows = tx
            .raw_query("SELECT edge_type, owner_entity, owner_id, from_entity, from_id, to_entity, to_id, properties FROM spot_graph_edge", &[], None)
            .await;
        let outcome = self
            .filter(rows, edge_types, principal, engine, &mut tx)
            .await;
        let _ = tx.rollback().await;
        outcome
    }

    async fn filter(
        &self,
        rows: spot_storage_contract::StorageResult<Vec<tokio_postgres::Row>>,
        edge_types: &BTreeSet<String>,
        principal: &Principal,
        engine: &PermissionEngine,
        tx: &mut spot_postgres::PgTransaction,
    ) -> std::result::Result<Facts, RuleError> {
        let model = spot_graph_model::GraphModel::derive(
            &self.live.served().ok_or(RuleError::Failed)?.meta,
        );
        let names: BTreeMap<String, String> = model
            .edges
            .values()
            .map(|e| (e.edge_id.as_str().to_owned(), e.name.clone()))
            .collect();
        let mut seen: BTreeMap<NodeRef, bool> = BTreeMap::new();
        let mut out: BTreeMap<String, Vec<(NodeRef, NodeRef)>> = BTreeMap::new();
        let mut total = 0usize;
        for row in rows.map_err(|_| RuleError::Failed)? {
            let get = |_: tokio_postgres::Error| RuleError::Failed;
            let edge = row_to_edge(EdgeRow {
                edge_type: row.try_get(0).map_err(get)?,
                owner_entity: row.try_get(1).map_err(get)?,
                owner_id: row.try_get(2).map_err(get)?,
                from_entity: row.try_get(3).map_err(get)?,
                from_id: row.try_get(4).map_err(get)?,
                to_entity: row.try_get(5).map_err(get)?,
                to_id: row.try_get(6).map_err(get)?,
                properties: row.try_get(7).map_err(get)?,
            })
            .map_err(|_| RuleError::Failed)?;
            let Some(name) = names
                .get(edge.edge_type.as_str())
                .filter(|n| edge_types.contains(*n))
            else {
                continue;
            };
            total += 1;
            if total > self.settings.max_facts {
                return Err(RuleError::Limit);
            }
            let (from_ok, to_ok) = (
                readable(&mut seen, tx, engine, principal, &edge.from).await?,
                readable(&mut seen, tx, engine, principal, &edge.to).await?,
            );
            if from_ok && to_ok {
                out.entry(name.clone())
                    .or_default()
                    .push((edge.from, edge.to));
            }
        }
        Ok(Facts(out))
    }

    async fn derive(
        &self,
        set: &str,
        predicate: &str,
        principal: &Principal,
    ) -> std::result::Result<Rows, RuleError> {
        let compiled = self.registry.get(set).ok_or(RuleError::Unknown)?;
        compiled.predicate(predicate).ok_or(RuleError::Unknown)?;
        let mut edge_types = BTreeSet::new();
        for p in compiled.predicates() {
            match &p.kind {
                PredKind::Edge => {
                    edge_types.insert(p.name.clone());
                }
                PredKind::Field { .. } => return Err(RuleError::UnsupportedFieldFact),
                PredKind::Derived => {}
            }
        }
        let served = self.live.served().ok_or(RuleError::Failed)?;
        let catalog =
            Arc::new(crate::schema::catalog(&served.meta).map_err(|_| RuleError::Failed)?);
        let budget = Duration::from_millis(self.settings.budget_ms);
        let engine = Arc::clone(&served.decision);
        let facts =
            tokio::time::timeout(budget, self.facts(&edge_types, principal, &engine, catalog))
                .await
                .map_err(|_| RuleError::Limit)??;
        let (fuel, predicate_name) = (self.settings.fuel, predicate.to_owned());
        let evaluated = tokio::task::spawn_blocking(move || {
            Engine::with_fuel(fuel).evaluate(&compiled, &facts, &Query::new(predicate_name))
        });
        let rows = tokio::time::timeout(budget, evaluated)
            .await
            .map_err(|_| RuleError::Limit)?
            .map_err(|_| RuleError::Failed)?
            .map_err(|e| match e {
                spot_rule_graph::RuleGraphError::FuelExhausted { .. } => RuleError::Limit,
                _ => RuleError::Failed,
            })?;
        if rows.len() > self.settings.max_rows {
            return Err(RuleError::Limit);
        }
        Ok(rows)
    }
}

/// Whether the principal may read `node`; judged once per node.
async fn readable(
    seen: &mut BTreeMap<NodeRef, bool>,
    tx: &mut spot_postgres::PgTransaction,
    engine: &PermissionEngine,
    principal: &Principal,
    node: &NodeRef,
) -> std::result::Result<bool, RuleError> {
    if let Some(known) = seen.get(node) {
        return Ok(*known);
    }
    let document = tx
        .load(&node.entity, node.id, RowLock::None)
        .await
        .map_err(|_| RuleError::Failed)?;
    let allowed = document.is_some_and(|d| {
        let facts = DocFacts {
            doctype: node.entity.as_str().to_owned(),
            id: Some(node.id.to_string()),
            created_by: None,
            state: None,
            values: d
                .data
                .fields
                .iter()
                .map(|(k, v)| (k.as_str().to_owned(), v.clone()))
                .collect(),
        };
        engine.can(principal, Action::Read, &facts).allowed
    });
    seen.insert(node.clone(), allowed);
    Ok(allowed)
}

struct Port(Arc<Shared>);

impl RuleGraph for Port {
    fn derive<'a>(
        &'a self,
        set: &'a str,
        predicate: &'a str,
        principal: &'a Principal,
    ) -> BoxFuture<'a, std::result::Result<Rows, RuleError>> {
        Box::pin(async move {
            self.0.stats.evaluations.fetch_add(1, Ordering::Relaxed);
            let outcome = self.0.derive(set, predicate, principal).await;
            match &outcome {
                Ok(rows) => {
                    tracing::info!(target: AUDIT_TARGET, site = %self.0.site, who = %principal.user, set, predicate, rows = rows.len(), "rule derived")
                }
                Err(error) => {
                    if *error == RuleError::Limit {
                        self.0.stats.limit_breaches.fetch_add(1, Ordering::Relaxed);
                    }
                    tracing::info!(target: AUDIT_TARGET, site = %self.0.site, who = %principal.user, set, predicate, code = error.code(), "rule refused");
                }
            }
            outcome
        })
    }

    fn failures(&self) -> Vec<(String, String)> {
        lock(&self.0.failures)
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// The approvers of a rule `"<set>/<predicate>"`, derived as `form.requested_by`.
impl ApproverResolver for Port {
    fn resolve(&self, rule: &str, _case: &str, _node: &str, form: &Json) -> Option<Vec<String>> {
        let (set, predicate) = rule.split_once('/')?;
        let user = form.get("requested_by").and_then(Json::as_str)?.to_owned();
        let principal = Principal {
            user,
            ..Principal::default()
        };
        let shared = Arc::clone(&self.0);
        let handle = tokio::runtime::Handle::current();
        let rows = tokio::task::block_in_place(|| {
            handle.block_on(shared.derive(set, predicate, &principal))
        })
        .ok()?;
        let users: Vec<String> = rows
            .rows()
            .iter()
            .filter_map(|row| match row.first()? {
                Datum::Text(name) => Some(name.clone()),
                _ => None,
            })
            .collect();
        (!users.is_empty()).then_some(users)
    }
}

struct RuleService {
    shared: Arc<Shared>,
}

impl Service for RuleService {
    fn name(&self) -> &'static str {
        "rule_graph"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn degraded(&self) -> Option<String> {
        let failed: Vec<String> = lock(&self.shared.failures).keys().cloned().collect();
        (!failed.is_empty()).then(|| format!("rule sets that did not compile: {}", failed.join(", ")))
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let sets = lock(&self.shared.sets).clone();
            self.shared.load(&sets);
            let failed = lock(&self.shared.failures)
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            if failed.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "rule sets that did not compile: {}",
                    failed.join(", ")
                ))
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut swaps = self.shared.live.swaps();
            loop {
                tokio::select! {
                    changed = stop.changed() => if changed.is_err() || *stop.borrow() { break },
                    changed = swaps.changed() => {
                        if changed.is_err() { break }
                        self.shared.swapped();
                    }
                }
            }
            Ok(())
        })
    }
}

/// The rule sets of every installed app: `<app>/rules/<name>.rules`, in install order.
fn read_sets(ctx: &SiteCtx<'_>) -> Result<Vec<(String, String)>> {
    let env = *ctx.env()?;
    let mut sets: BTreeMap<String, String> = BTreeMap::new();
    for app in super::controllers::installed(env.root, env.loaded, env.site)? {
        let Ok(entries) = std::fs::read_dir(app.dir.join(RULES_DIR)) else {
            continue;
        };
        let mut files: Vec<_> = entries.filter_map(std::result::Result::ok).collect();
        files.sort_by_key(std::fs::DirEntry::file_name);
        for file in files {
            let path = file.path();
            let (Some("rules"), Some(stem)) = (
                path.extension().and_then(|e| e.to_str()),
                path.file_stem().and_then(|s| s.to_str()),
            ) else {
                continue;
            };
            if let Ok(text) = std::fs::read_to_string(&path) {
                sets.insert(stem.to_owned(), text);
            }
        }
    }
    Ok(sets.into_iter().collect())
}

/// Registers the rule graph: the service, the [`RuleGraph`] port and the approver resolver.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("rule_graph")?;
    if !settings.enabled {
        return Ok(());
    }
    if !ctx.assembling() {
        // A reload keeps the running service and re-reads the apps' rule files: a set that no
        // longer compiles is named (the previous one keeps serving), a fixed one clears.
        let site = ctx.env()?.site.site.clone();
        let kept = built().get(&site).cloned();
        if let Some(shared) = kept {
            let sets = read_sets(ctx)?;
            *lock(&shared.sets) = sets.clone();
            shared.load(&sets);
        }
        return Ok(());
    }
    if settings.max_facts == 0
        || settings.fuel == 0
        || settings.max_rows == 0
        || settings.budget_ms == 0
    {
        return Err(Error(
            "services.rule_graph: a limit must be above zero".to_owned(),
        ));
    }
    let env = *ctx.env()?;
    let shared = Arc::new(Shared {
        site: env.site.site.clone(),
        settings,
        live: Arc::clone(env.live),
        backend: env.backend.clone(),
        registry: Registry::new(),
        sets: Mutex::new(read_sets(ctx)?),
        failures: Mutex::new(BTreeMap::new()),
        stats: Stats::default(),
    });
    ctx.add(Arc::new(RuleService {
        shared: Arc::clone(&shared),
    }));
    built().insert(shared.site.clone(), Arc::clone(&shared));
    let port = Arc::new(Port(shared));
    ctx.provide::<dyn RuleGraph>(Arc::clone(&port) as Arc<dyn RuleGraph>)?;
    ctx.provide::<dyn ApproverResolver>(port as Arc<dyn ApproverResolver>)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_meta::{Definition, FieldSpec, FieldType};
    use spot_storage_contract::RecordId;

    fn meta() -> Arc<spot_meta::EffectiveMeta> {
        let project = Definition::new("project");
        let mut task = Definition::new("task");
        let mut link = FieldSpec::new("project", FieldType::Link);
        link.options = Some("project".to_owned());
        task.fields.push(link);
        spot_meta::Registry::new(vec![project, task])
            .map(|registry| registry.effective())
            .unwrap_or_else(|errors| panic!("the metadata builds: {errors:?}"))
    }

    fn node(doctype: &str) -> NodeRef {
        NodeRef {
            entity: Name::new(doctype).unwrap(),
            id: RecordId::new_v7(),
        }
    }

    const OPEN: &str = "open(T, P) :- task.project(T, P).";

    #[test]
    fn defaults_are_the_documented_limits_and_unknown_keys_are_refused() {
        let s = Settings::default();
        assert!(s.enabled);
        assert_eq!(
            (s.max_facts, s.fuel, s.max_rows, s.budget_ms),
            (100_000, 1_000_000, 10_000, 5_000)
        );
        assert!(toml::from_str::<Settings>("max_rows = 5\nwall = 1\n").is_err());
        let parsed: Settings = toml::from_str("max_rows = 5").unwrap();
        assert_eq!(parsed.max_rows, 5);
        assert_eq!(parsed.max_facts, 100_000);
    }

    #[test]
    fn every_refusal_has_its_own_stable_code() {
        let codes = [
            RuleError::Unknown,
            RuleError::UnsupportedFieldFact,
            RuleError::Limit,
            RuleError::Failed,
        ]
        .map(RuleError::code);
        assert_eq!(
            codes,
            [
                "rule_graph.unknown",
                "rule_graph.unsupported_field_fact",
                "rule_graph.limit",
                "rule_graph.failed"
            ]
        );
    }

    #[test]
    fn a_fact_the_principal_cannot_read_changes_what_is_derived() {
        let registry = Registry::new();
        registry.register("open".to_owned(), OPEN, &meta()).unwrap();
        let compiled = registry.get("open").unwrap();
        let (project, visible, hidden) = (node("project"), node("task"), node("task"));
        let edges = |pairs: Vec<(NodeRef, NodeRef)>| {
            Facts(BTreeMap::from([("task.project".to_owned(), pairs)]))
        };
        let all = edges(vec![
            (visible.clone(), project.clone()),
            (hidden, project.clone()),
        ]);
        // The host drops an edge when either end is unreadable; what is left derives less.
        let filtered = edges(vec![(visible, project)]);
        let engine = Engine::with_fuel(1_000_000);
        let admin = engine
            .evaluate(&compiled, &all, &Query::new("open".to_owned()))
            .unwrap();
        let reader = engine
            .evaluate(&compiled, &filtered, &Query::new("open".to_owned()))
            .unwrap();
        assert_eq!((admin.len(), reader.len()), (2, 1));
        // No facts at all (a principal who reads nothing) derives nothing, never everything.
        let none = engine
            .evaluate(&compiled, &edges(vec![]), &Query::new("open".to_owned()))
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn fuel_exhaustion_is_an_error_and_never_a_partial_answer() {
        let registry = Registry::new();
        registry.register("open".to_owned(), OPEN, &meta()).unwrap();
        let compiled = registry.get("open").unwrap();
        let project = node("project");
        let pairs = (0..50).map(|_| (node("task"), project.clone())).collect();
        let facts = Facts(BTreeMap::from([("task.project".to_owned(), pairs)]));
        let starved =
            Engine::with_fuel(1).evaluate(&compiled, &facts, &Query::new("open".to_owned()));
        assert!(matches!(
            starved,
            Err(spot_rule_graph::RuleGraphError::FuelExhausted { .. })
        ));
    }

    #[test]
    fn a_set_that_does_not_compile_keeps_the_previous_one_serving() {
        let registry = Registry::new();
        let meta = meta();
        registry.register("open".to_owned(), OPEN, &meta).unwrap();
        // Unbound head variable: not range-restricted.
        let bad = "open(T, Z) :- task.project(T, P).";
        let refused = registry.register("open".to_owned(), bad, &meta);
        assert!(refused.is_err());
        let still = registry.get("open").expect("the previous set serves");
        assert!(still.predicate("open").is_some());
        // The failure's code is stable text, not the rule.
        let code = refused.err().unwrap().code();
        assert!(!code.contains("task.project"), "{code}");
    }
}
