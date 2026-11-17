//! Process versions of one site (WP-BPM-09.T4; ADR-49, ADR-071, ADR-080): the production
//! `VersionPort` behind native API 0.12.0 (version list, get, publish, retire, migration plan,
//! start and get), over the durable definition store and the Case store.
//!
//! # What the module does
//!
//! - **Publish** (`process.publish`): compile the text with the model, check every node kind has a
//!   handler (`handler_missing`), service-principal Document nodes (`process.publish_service_principal`
//!   and the site's `PublishGuard`), the effect checks, and that webhook triggers have a configured
//!   secret; then `DefinitionStore::publish` with the expected head, an audit row, the registry and
//!   trigger reload. A problem is a node id and a closed kind token, never document text.
//! - **Retire** (`process.publish`): the stored state changes and the live registry entry is replaced
//!   by its retired form; the trigger index only holds published versions, so a retired-only process
//!   loses its triggers. **Pinning** is the engine's: a Run keeps the version it started on.
//! - **Migration** (`process.migrate`): the plan is read-only (Run ids and reason codes only). A start
//!   writes one durable record and wakes the worker, which claims a record under a database lease,
//!   moves at most `batch` Runs per pass from the record's cursor (each in one transaction that also
//!   counts the move and audits it), and skips, never forces, a Run that is mid-node, waiting or
//!   whose mapping does not fit. A restart resumes from the cursor.
//! - **Every refusal is audited** (who, process, version, the refusing rule): target
//!   `spot::audit::process_versions` and the `spot_bpm_version_audit` table; never the text, note,
//!   key or mapping. A caller without the permission, an unknown process and an unknown version are
//!   one `NotFound`; a permission source that cannot answer denies.
//!
//! # Configuration
//!
//! `[services.process_versions]`: `enabled` (true), `batch` (Runs per pass, 100, 1 to 1000),
//! `tick_secs` (5, 1 to 60), `lease_secs` (60, 10 to 600). Unknown keys are refused.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Map, Value as Json, json};
use spot_bpm_definitions::{
    DefinitionError, DefinitionStore, MAX_PAGE, StoredVersion, VersionState as StoredState,
    check_name,
};
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_pg::{MigrationMove, MigrationOutcome, PgCaseStore};
use spot_bpm_documents::{ProcessAuthor, PublishGuard};
use spot_bpm_effect_checks::{ChecksError, PolicyMeta, PublishGate};
use spot_bpm_engine::{
    AttemptState, CaseId, CaseRecord, CaseStore, NodeKind as EngineKind, ProcessDefinition,
};
use spot_bpm_model::{NodeId, NodeKind, ProcessVersion, VersionStatus, from_text};
use spot_bpm_triggers::TriggerIndex;
use spot_bpm_versioning::{
    MIGRATION_AUDIT_TOPIC, MigrationAudit, MigrationMapping, MigrationPlan, VariableAction,
    plan_migration, PublishCheck, VersioningError, validate_guarded,
};
use spot_core::{Clock, SiteId};
use spot_dag_exec::NodeStatus;
use spot_http::api::Ports;
use spot_http::api::idempotency::{IdempotencyStore, MemoryIdempotencyStore};
use spot_http::api::ports::process_version::{
    MAX_DEFINITION_BYTES, MAX_NODE_COUNTS, MAX_NOTE_CHARS, MAX_PROBLEMS, MAX_UNMOVED,
    MigrationMapping as PortMapping, MigrationPlan as PortPlan, MigrationProgress,
    MigrationRequest, MigrationState, NodeCount, Problem, PublishRequest, Published, Unmoved,
    VersionDetail, VersionPage, VersionPort, VersionPortError, VersionResult, VersionState,
    VersionSummary,
};
use spot_http::api::router::routes::{self, Registration};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_permissions::Principal;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Notify, watch};

use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::process::ProcessHost;
use crate::services::process_triggers::{Fut, Settings as TriggerSettings, TriggerHost};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the process engine (1100), the node-kind modules (1110 to 1115), the
/// documents module that provides the `PublishGuard`, and the trigger intake (1130).
pub const ORDER: u32 = 1160;

const AUDIT: &str = "spot::audit::process_versions";
const PAGE: i64 = 200;
/// The most Runs one plan scans; the counts are of those scanned.
const MAX_SCAN: usize = 20_000;
const PUBLISH: &str = "process_publish";
const READ: &str = "process_read_versions";
const MIGRATE: &str = "process_migrate";
const PRINCIPAL: &str = "process_publish_service_principal";

/// `[services.process_versions]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// Runs one migration pass moves (1 to 1000).
    pub batch: u32,
    /// Seconds between worker passes (1 to 60).
    pub tick_secs: u64,
    /// Seconds a worker holds a migration (10 to 600).
    pub lease_secs: u64,
}

#[rustfmt::skip]
impl Default for Settings {
    fn default() -> Self { Self { enabled: true, batch: 100, tick_secs: 5, lease_secs: 60 } }
}

#[rustfmt::skip]
impl Settings {
    fn validate(&self) -> std::result::Result<(), String> {
        let bad = |key: &str| Err(format!("services.process_versions.{key}: out of range"));
        if !(1..=1000).contains(&self.batch) { return bad("batch"); }
        if !(1..=60).contains(&self.tick_secs) { return bad("tick_secs"); }
        if !(10..=600).contains(&self.lease_secs) { return bad("lease_secs"); }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Who is calling

/// An authenticated caller: the user id and the permission principal of that user.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The user's record id, as text.
    pub user: String,
    /// The user's assignments.
    pub principal: Principal,
}

/// Who is calling and what they hold. A source that cannot answer returns `None` or `false`.
pub trait Access: Send + Sync {
    /// The caller of `ctx`, or `None` (no credential, a refused one, an unreadable source).
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> Fut<'a, Option<Caller>>;
    /// Whether `caller` holds the platform permission `permission` (its engine name).
    fn holds(&self, caller: &Caller, permission: &str) -> bool;
}

/// [`Access`] over the site's sign-in service and the permission snapshot served now.
pub struct LiveAccess {
    /// The site's live hub.
    pub live: Arc<SiteLive>,
    /// The storage-backed principal source.
    pub principals: Arc<dyn spot_document::engine::PrincipalSource>,
}

impl Access for LiveAccess {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> Fut<'a, Option<Caller>> {
        Box::pin(async move {
            ctx.credential.as_ref()?;
            let auth = self.live.auth()?;
            let who = auth.authenticate_on(ctx, PathClass::Api).await.ok()?;
            if auth.is_restricted(&who) {
                return None;
            }
            let principal = self.principals.resolve(&who.user_id).await;
            Some(Caller {
                user: who.user_id.to_string(),
                principal,
            })
        })
    }

    fn holds(&self, caller: &Caller, permission: &str) -> bool {
        self.live.served().is_some_and(|s| {
            s.decision
                .app_permission(&caller.principal, permission)
                .allowed
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Tables

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS spot_bpm_version_audit (\
  id bigserial PRIMARY KEY, tenant text NOT NULL, at timestamptz NOT NULL, actor text NOT NULL, \
  operation text NOT NULL, process text NOT NULL, version integer, outcome text NOT NULL, \
  content_hash text, size integer);\
CREATE TABLE IF NOT EXISTS spot_bpm_migration (\
  tenant text NOT NULL, id text NOT NULL, process text NOT NULL, from_version integer NOT NULL, \
  to_version integer NOT NULL, mapping jsonb NOT NULL, state text NOT NULL, \
  cursor text NOT NULL DEFAULT '', moved bigint NOT NULL DEFAULT 0, \
  skipped bigint NOT NULL DEFAULT 0, failed bigint NOT NULL DEFAULT 0, started_by text NOT NULL, \
  idem_key text, started_at timestamptz NOT NULL, ended_at timestamptz, lease_holder text, \
  lease_until timestamptz, PRIMARY KEY (tenant, id));\
CREATE UNIQUE INDEX IF NOT EXISTS spot_bpm_migration_idem_idx \
  ON spot_bpm_migration (tenant, idem_key) WHERE idem_key IS NOT NULL;\
CREATE TABLE IF NOT EXISTS spot_bpm_migration_audit (\
  tenant text NOT NULL, migration_id text NOT NULL, case_id text NOT NULL, \
  plan_digest text NOT NULL, payload text NOT NULL, at timestamptz NOT NULL, \
  PRIMARY KEY (tenant, case_id, plan_digest));";

// ---------------------------------------------------------------------------------------------
// The host

/// What a [`VersionHost`] is built from; tests supply their own seams.
pub struct Parts {
    /// The site.
    pub site: SiteId,
    /// The site's pool.
    pub pool: deadpool_postgres::Pool,
    /// The durable definition store of this site.
    pub definitions: Arc<PgDefinitionStore>,
    /// The process engine host (registry, handlers, Case store).
    pub process: Arc<ProcessHost>,
    /// Who is calling and what they hold.
    pub access: Arc<dyn Access>,
    /// The service-principal publish guard, when the documents module is on.
    pub guard: Option<Arc<PublishGuard>>,
    /// The trigger intake, when it provides itself as a port.
    pub triggers: Option<Arc<TriggerHost>>,
    /// The trigger intake's settings (webhook secrets are checked against them).
    pub trigger_settings: TriggerSettings,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
    /// This module's settings.
    pub settings: Settings,
}

/// The production [`VersionPort`] of one site, and the migration worker.
pub struct VersionHost {
    p: Parts,
    holder: String,
    dirty: AtomicBool,
    wake: Notify,
}

type R<T> = VersionResult<T>;

#[rustfmt::skip]
fn code_of(error: &VersionPortError) -> &'static str {
    use VersionPortError as E;
    match error {
        E::NotFound => "process.not_found", E::PublishDenied => "process.publish_denied",
        E::VersionConflict => "process.version_conflict", E::DefinitionInvalid(_) => "process.definition_invalid",
        E::VersionNotPublished => "process.version_not_published", E::MigrationInvalid => "process.migration_invalid",
        E::BadCursor => "api.invalid_request", E::Unavailable => "server.unavailable", E::Internal => "http.internal",
    }
}

#[rustfmt::skip]
fn def_err(error: DefinitionError) -> VersionPortError {
    match error {
        DefinitionError::VersionConflict { .. } => VersionPortError::VersionConflict,
        DefinitionError::NotFound { .. } | DefinitionError::NameInvalid => VersionPortError::NotFound,
        DefinitionError::StoreFailed { .. } => VersionPortError::Unavailable,
        _ => VersionPortError::Internal,
    }
}

fn db(error: impl std::fmt::Display) -> VersionPortError {
    tracing::warn!(target: AUDIT, %error, "a process version query failed");
    VersionPortError::Unavailable
}

fn problem(node: Option<&NodeId>, kind: &str) -> Problem {
    Problem {
        node: node.map(|n| n.as_str().to_owned()),
        kind: kind.to_owned(),
    }
}

fn invalid(mut problems: Vec<Problem>) -> VersionPortError {
    problems.truncate(MAX_PROBLEMS);
    VersionPortError::DefinitionInvalid(problems)
}

fn rfc(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

#[rustfmt::skip]
fn summary(v: &StoredVersion, running: u32) -> VersionSummary {
    let state = match v.state { StoredState::Published => VersionState::Published, StoredState::Retired => VersionState::Retired };
    VersionSummary {
        version: v.version, state, published_at: Some(rfc(v.published_at)), published_by: Some(v.published_by.clone()),
        note: v.note.clone().unwrap_or_default(), running_runs: running,
    }
}

/// The compiled definition of a stored version, in the status the store says.
#[rustfmt::skip]
fn definition_of(stored: &StoredVersion) -> Option<ProcessDefinition> {
    let mut version = from_text(&stored.text).ok()?;
    version.version = stored.version;
    version.status = match stored.state { StoredState::Published => VersionStatus::Published, StoredState::Retired => VersionStatus::Retired };
    ProcessDefinition::compile(&version).ok()
}

/// Why a Run is not moved now, or `None` when it stands where a migration may touch it.
#[rustfmt::skip]
fn busy(record: &CaseRecord) -> Option<&'static str> {
    let attempts = || record.attempts.values().map(|a| &a.state);
    if record.run.nodes.values().any(|n| n.status == NodeStatus::Running)
        || attempts().any(|s| matches!(s, AttemptState::Started | AttemptState::Retrying)) {
        return Some("bpm_versioning.case_mid_node");
    }
    if !record.timers.is_empty() || !record.children.is_empty() || attempts().any(|s| matches!(s, AttemptState::Waiting)) {
        return Some("bpm_versioning.case_waiting");
    }
    None
}

#[rustfmt::skip]
fn evaluate(record: &CaseRecord, from: &ProcessDefinition, to: &ProcessDefinition, mapping: &MigrationMapping)
    -> std::result::Result<MigrationPlan, String> {
    if let Some(reason) = busy(record) { return Err(reason.to_owned()); }
    plan_migration(record, from, to, mapping).map_err(|e| e.code().to_owned())
}

/// The versioning mapping of a request's mapping, every node checked against the target.
#[rustfmt::skip]
fn mapping_of(mapping: Option<&PortMapping>, target: &ProcessDefinition) -> R<MigrationMapping> {
    let mut out = MigrationMapping::new();
    let Some(map) = mapping else { return Ok(out) };
    for (a, b) in &map.nodes {
        let a = spot_dag::NodeId::new(a).map_err(|_| VersionPortError::MigrationInvalid)?;
        let b = spot_dag::NodeId::new(b).map_err(|_| VersionPortError::MigrationInvalid)?;
        if target.node(&b).is_none() { return Err(VersionPortError::MigrationInvalid); }
        out.map_node(a, b);
    }
    for path in &map.required_variables { out = out.requiring(path.clone()); }
    for (path, value) in &map.variable_defaults { out = out.with_variable_default(path.clone(), value.clone()); }
    Ok(out)
}

#[rustfmt::skip]
fn mapping_json(mapping: Option<&PortMapping>) -> Json {
    let Some(m) = mapping else { return json!({}) };
    json!({"nodes": m.nodes, "required": m.required_variables, "defaults": m.variable_defaults})
}

#[rustfmt::skip]
fn mapping_back(value: &Json) -> PortMapping {
    let text = |v: &Json| v.as_str().map(str::to_owned);
    PortMapping {
        nodes: value["nodes"].as_object()
            .map(|o| o.iter().filter_map(|(k, v)| Some((k.clone(), text(v)?))).collect()).unwrap_or_default(),
        required_variables: value["required"].as_array().map(|a| a.iter().filter_map(text).collect()).unwrap_or_default(),
        variable_defaults: value["defaults"].as_object().cloned().unwrap_or_else(Map::new),
    }
}

// The two impl blocks below are hand-wrapped (`rustfmt::skip`): one module file may not exceed the
// size gate, and rustfmt's one-argument-per-line style would double their length.
#[rustfmt::skip]
impl VersionHost {
    /// A host over `parts`.
    #[must_use]
    pub fn new(parts: Parts) -> Arc<Self> {
        let holder = format!("versions-{}", spot_storage_contract::RecordId::new_v7());
        Arc::new(Self { holder, p: parts, dirty: AtomicBool::new(false), wake: Notify::new() })
    }

    /// Creates the module's tables.
    ///
    /// # Errors
    /// The database refuses them.
    pub async fn prepare(&self) -> Result<()> {
        let client = self.p.pool.get().await.map_err(|e| Error(e.to_string()))?;
        let made = client.batch_execute(SCHEMA).await;
        made.map_err(|e| Error(format!("the process version tables could not be prepared: {e}")))
    }

    fn now(&self) -> OffsetDateTime { self.p.clock.now_utc() }

    /// Authenticates the caller and checks that they hold one of `perms`. Anything else, including a
    /// source that cannot answer, is the one `NotFound`, audited when the caller is known.
    async fn gate(&self, ctx: &RequestCtx, op: &'static str, process: &str, perms: &[&str]) -> R<Caller> {
        if ctx.site.as_str() != self.p.site.as_str() { return Err(VersionPortError::NotFound); }
        let Some(caller) = self.p.access.caller(ctx).await else {
            tracing::info!(target: AUDIT, site = %self.p.site, op, outcome = "unauthenticated");
            return Err(VersionPortError::NotFound);
        };
        if perms.iter().any(|p| self.p.access.holds(&caller, p)) { return Ok(caller); }
        self.deny(&caller, op, process, None, VersionPortError::NotFound).await
    }

    /// Audits a refusal and returns it.
    async fn deny<T>(&self, who: &Caller, op: &str, process: &str, version: Option<u32>, e: VersionPortError) -> R<T> {
        self.audit(&who.user, op, process, version, code_of(&e), (None, None)).await;
        Err(e)
    }

    /// Writes one audit row (never a text, a note, a key or a mapping): `extra` is the content hash
    /// and the note length of a publish.
    async fn audit(&self, actor: &str, op: &str, process: &str, version: Option<u32>, outcome: &str, extra: (Option<&str>, Option<usize>)) {
        tracing::info!(target: AUDIT, site = %self.p.site, op, process = %process, ?version, outcome);
        let process = if check_name(process).is_ok() { process } else { "?" };
        let version = version.and_then(|v| i32::try_from(v).ok());
        let size = extra.1.and_then(|s| i32::try_from(s).ok());
        let now = self.now();
        let Ok(client) = self.p.pool.get().await else { return };
        let site = self.p.site.as_str();
        let written = client.execute(
            "INSERT INTO spot_bpm_version_audit (tenant, at, actor, operation, process, version, outcome, \
             content_hash, size) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
            &[&site, &now, &actor, &op, &process, &version, &outcome, &extra.0, &size],
        ).await;
        if let Err(error) = written { tracing::warn!(target: AUDIT, %error, "the version audit row was not written"); }
    }

    async fn all_versions(&self, process: &str) -> R<Vec<StoredVersion>> {
        let (mut out, mut after) = (Vec::new(), None);
        loop {
            let page = self.p.definitions.list(process, after, MAX_PAGE).await.map_err(def_err)?;
            let Some(last) = page.last().map(|v| v.version) else { return Ok(out) };
            out.extend(page);
            after = Some(last);
        }
    }

    /// A Case store for this site; every query it runs is tenant-scoped.
    fn cases(&self) -> PgCaseStore {
        PgCaseStore::new(self.p.pool.clone()).with_tenant(self.p.site.as_str())
    }

    async fn pin_counts(&self, process: &str) -> R<BTreeMap<u32, u32>> {
        let counts = self.cases().running_by_version(process).await.map_err(db)?;
        Ok(counts.into_iter().map(|(v, n)| (v, u32::try_from(n).unwrap_or(u32::MAX))).collect())
    }

    /// Rebuilds the registry and the trigger index. A failure leaves the `dirty` flag, so the worker
    /// retries; the stored state is the truth either way.
    pub async fn reload(&self) -> bool {
        self.dirty.store(true, Ordering::SeqCst);
        match self.p.process.reload_definitions().await {
            Ok(report) if !report.is_clean() => tracing::warn!(target: AUDIT, n = report.failures.len(), "versions do not load"),
            Ok(_) => {}
            Err(error) => { tracing::warn!(target: AUDIT, %error, "reloading the process registry failed"); return false; }
        }
        if let Some(triggers) = &self.p.triggers && let Err(error) = triggers.reload().await {
            tracing::warn!(target: AUDIT, %error, "reloading the trigger index failed");
            return false;
        }
        self.dirty.store(false, Ordering::SeqCst);
        true
    }

    // ---- versions

    async fn do_list(&self, ctx: &RequestCtx, process: &str, cursor: Option<&str>, limit: u32) -> R<VersionPage> {
        let op = "process_version_list";
        let caller = self.gate(ctx, op, process, &[PUBLISH, READ]).await?;
        let before = match cursor {
            None => None,
            Some(c) => Some(c.strip_prefix('c').and_then(|n| n.parse::<u32>().ok()).ok_or(VersionPortError::BadCursor)?),
        };
        let mut all = self.all_versions(process).await?;
        if all.is_empty() { return self.deny(&caller, op, process, None, VersionPortError::NotFound).await; }
        all.sort_by_key(|v| std::cmp::Reverse(v.version));
        let pins = self.pin_counts(process).await?;
        let limit = limit.clamp(1, 100) as usize;
        let mut items: Vec<VersionSummary> = all.iter()
            .filter(|v| before.is_none_or(|b| v.version < b)).take(limit + 1)
            .map(|v| summary(v, pins.get(&v.version).copied().unwrap_or(0))).collect();
        let more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if more { items.last().map(|v| format!("c{}", v.version)) } else { None };
        Ok(VersionPage { items, next_cursor })
    }

    async fn do_get(&self, ctx: &RequestCtx, process: &str, version: u32) -> R<VersionDetail> {
        let op = "process_version_get";
        let caller = self.gate(ctx, op, process, &[PUBLISH, READ]).await?;
        let Some(stored) = self.p.definitions.get(process, version).await.map_err(def_err)? else {
            return self.deny(&caller, op, process, Some(version), VersionPortError::NotFound).await;
        };
        let pins = self.pin_counts(process).await?;
        let running = pins.get(&version).copied().unwrap_or(0);
        Ok(VersionDetail { version: summary(&stored, running), hash: stored.content_hash.clone(), definition: stored.text })
    }

    /// Every check a definition passes before it is stored; the first failing class answers.
    fn check_definition(&self, caller: &Caller, process: &str, text: &str, head: u32) -> R<()> {
        let bad = |kind: &str| invalid(vec![problem(None, kind)]);
        let mut version: ProcessVersion = from_text(text).map_err(|_| bad("bpm_model.text_invalid"))?;
        if version.name != process { return Err(bad("bpm_model.process_mismatch")); }
        version.version = head.saturating_add(1);
        version.status = VersionStatus::Published;
        let _compiled = ProcessDefinition::compile(&version)
            .map_err(|ps| invalid(ps.iter().map(|p| problem(p.node.as_ref(), p.code)).collect()))?;
        let principal = |n: &&spot_bpm_model::Node| matches!(&n.kind, NodeKind::Document(c) if c.principal.is_some());
        let acts_as_principal = version.nodes.iter().any(|n| principal(&n));
        // Fails closed: a caller without the permission, or a site without the guard, is a denial.
        let guard = self.p.guard.as_ref().filter(|_| self.p.access.holds(caller, PRINCIPAL));
        if acts_as_principal && guard.is_none() { return Err(VersionPortError::PublishDenied); }
        let engine = self.p.process.engine().map_err(|_| VersionPortError::Unavailable)?;
        let missing: Vec<Problem> = version.nodes.iter()
            .filter(|n| { let k = EngineKind::from_model(&n.kind); k != EngineKind::SubProcess && !engine.handlers().contains(&k) })
            .map(|n| problem(Some(&n.id), "handler_missing")).collect();
        if !missing.is_empty() { return Err(invalid(missing)); }
        if let (true, Some(guard)) = (acts_as_principal, guard) {
            let author = ProcessAuthor::User(caller.user.clone());
            // The guard logic lives in one place; nothing is registered here (the version is stored
            // first, then the reload makes it live), so the returned definition is dropped.
            let check = PublishCheck { guard, tenant: &self.p.site, author: &author };
            match validate_guarded(&version, &check) {
                Ok(_) => {}
                Err(VersioningError::PublishRefused { violations }) => {
                    return Err(invalid(violations.into_iter()
                        .map(|v| Problem { node: Some(v.node), kind: v.rule }).collect()));
                }
                Err(_) => return Err(VersionPortError::Internal),
            }
        }
        let registry = crate::platform_registry().map_err(|_| VersionPortError::Internal)?;
        let meta = PolicyMeta::new().with_known_classes(spot_seams::PLATFORM_EFFECT_CLASSES.iter().copied());
        match PublishGate::check(&version, &registry, &meta) {
            Ok(_) => {}
            Err(ChecksError::PublishRefused { problems }) => {
                return Err(invalid(problems.iter().map(|p| problem(p.node.as_ref(), p.token())).collect()));
            }
            Err(_) => return Err(VersionPortError::Internal),
        }
        let index = TriggerIndex::from_versions(std::slice::from_ref(&version)).map_err(|_| bad("trigger_invalid"))?;
        let settings = &self.p.trigger_settings;
        let configured = |path: &str| settings.webhooks.get(path).is_some_and(|w| settings.secrets.contains_key(&w.secret));
        let unsigned: Vec<Problem> = index.entries().iter()
            .filter(|e| e.spec.as_webhook().is_some_and(|hook| !configured(&hook.path)))
            .map(|e| problem(Some(&e.node), "webhook_secret_missing")).collect();
        if unsigned.is_empty() { Ok(()) } else { Err(invalid(unsigned)) }
    }

    async fn do_publish(&self, ctx: &RequestCtx, process: &str, request: PublishRequest) -> R<Published> {
        let op = "process_version_publish";
        let caller = self.gate(ctx, op, process, &[PUBLISH]).await?;
        let oversize = request.definition.len() > MAX_DEFINITION_BYTES
            || request.note.chars().count() > MAX_NOTE_CHARS || request.note.contains('\0');
        if oversize {
            return self.deny(&caller, op, process, None, invalid(vec![problem(None, "request.invalid")])).await;
        }
        let head = self.all_versions(process).await?.iter().map(|v| v.version).max().unwrap_or(0);
        if request.expected_previous.is_some_and(|n| n != head) {
            return self.deny(&caller, op, process, None, VersionPortError::VersionConflict).await;
        }
        if let Err(error) = self.check_definition(&caller, process, &request.definition, head) {
            return self.deny(&caller, op, process, None, error).await;
        }
        let note = (!request.note.is_empty()).then_some(request.note.as_str());
        let expected = (head > 0).then_some(head);
        let stored = self.p.definitions.publish(process, &request.definition, expected, &caller.user, note, self.now()).await;
        let stored = match stored { Ok(s) => s, Err(e) => return self.deny(&caller, op, process, None, def_err(e)).await };
        let extra = (Some(stored.content_hash.as_str()), Some(request.note.len()));
        self.audit(&caller.user, op, process, Some(stored.version), "ok", extra).await;
        if !self.reload().await { tracing::warn!(target: AUDIT, "the version is stored; the reload retries"); }
        Ok(Published { hash: stored.content_hash.clone(), version: summary(&stored, 0) })
    }

    async fn do_retire(&self, ctx: &RequestCtx, process: &str, version: u32) -> R<VersionSummary> {
        let op = "process_version_retire";
        let caller = self.gate(ctx, op, process, &[PUBLISH]).await?;
        let Some(stored) = self.p.definitions.get(process, version).await.map_err(def_err)? else {
            return self.deny(&caller, op, process, Some(version), VersionPortError::NotFound).await;
        };
        if stored.state != StoredState::Published {
            return self.deny(&caller, op, process, Some(version), VersionPortError::VersionNotPublished).await;
        }
        let retired = self.p.definitions.retire(process, version, &caller.user, self.now()).await;
        let retired = match retired { Ok(r) => r, Err(e) => return self.deny(&caller, op, process, Some(version), def_err(e)).await };
        // The registry keeps a loaded published copy until it is replaced: replace it now, so no new
        // Run starts on a retired version even before the next reload.
        if let (Some(definition), Ok(engine)) = (definition_of(&retired), self.p.process.engine()) {
            let _ = engine.processes().publish(definition);
        }
        self.audit(&caller.user, op, process, Some(version), "ok", (Some(retired.content_hash.as_str()), None)).await;
        if !self.reload().await { tracing::warn!(target: AUDIT, "the retire is stored; the reload retries"); }
        let pins = self.pin_counts(process).await?;
        Ok(summary(&retired, pins.get(&version).copied().unwrap_or(0)))
    }

    // ---- migration

    /// The two definitions of a migration request, or the refusal.
    async fn targets(&self, who: &Caller, op: &'static str, process: &str, from: u32, req: &MigrationRequest)
        -> R<(ProcessDefinition, ProcessDefinition)> {
        let store = &self.p.definitions;
        let Some(stored_from) = store.get(process, from).await.map_err(def_err)? else {
            return self.deny(who, op, process, Some(from), VersionPortError::NotFound).await;
        };
        if req.to == from { return self.deny(who, op, process, Some(from), VersionPortError::MigrationInvalid).await; }
        let stored_to = store.get(process, req.to).await.map_err(def_err)?;
        let Some(stored_to) = stored_to.filter(|t| t.state == StoredState::Published) else {
            return self.deny(who, op, process, Some(from), VersionPortError::VersionNotPublished).await;
        };
        let (Some(a), Some(b)) = (definition_of(&stored_from), definition_of(&stored_to)) else {
            return Err(VersionPortError::Internal);
        };
        if let Err(e) = mapping_of(req.mapping.as_ref(), &b) { return self.deny(who, op, process, Some(from), e).await; }
        Ok((a, b))
    }

    async fn case_ids(&self, process: &str, version: u32, after: &str, limit: i64) -> R<Vec<String>> {
        let ids = self.cases().running_case_ids(process, version, after, limit).await.map_err(db)?;
        Ok(ids)
    }

    async fn load(&self, id: &str) -> R<Option<CaseRecord>> {
        let id = CaseId::new(id).map_err(|_| VersionPortError::Internal)?;
        let engine = self.p.process.engine().map_err(|_| VersionPortError::Unavailable)?;
        engine.store().load(&id).await.map_err(db)
    }

    async fn do_plan(&self, ctx: &RequestCtx, process: &str, from: u32, request: MigrationRequest) -> R<PortPlan> {
        let op = "process_migration_plan";
        let caller = self.gate(ctx, op, process, &[MIGRATE]).await?;
        let (a, b) = self.targets(&caller, op, process, from, &request).await?;
        let mapping = mapping_of(request.mapping.as_ref(), &b)?;
        let (mut movable, mut total, mut scanned) = (0_u32, 0_u32, 0_usize);
        let (mut counts, mut unmoved, mut after) = (BTreeMap::<(String, String), u32>::new(), Vec::new(), String::new());
        'scan: loop {
            let ids = self.case_ids(process, from, &after, PAGE).await?;
            if ids.is_empty() { break; }
            for id in ids {
                if scanned >= MAX_SCAN { break 'scan; }
                scanned += 1;
                after.clone_from(&id);
                let Some(record) = self.load(&id).await? else { continue };
                match evaluate(&record, &a, &b, &mapping) {
                    Ok(plan) => {
                        movable += 1;
                        for c in plan.node_changes() { *counts.entry((c.from.clone(), c.to.clone())).or_default() += 1; }
                    }
                    Err(reason) => {
                        total += 1;
                        if unmoved.len() < MAX_UNMOVED { unmoved.push(Unmoved { run: id, reason }); }
                    }
                }
            }
        }
        self.audit(&caller.user, op, process, Some(from), "ok", (None, None)).await;
        let node_counts = counts.into_iter().take(MAX_NODE_COUNTS)
            .map(|((from, to), runs)| NodeCount { from, to, runs }).collect();
        Ok(PortPlan { from, to: request.to, movable, unmoved_total: total, node_counts, unmoved })
    }

    async fn do_start(&self, ctx: &RequestCtx, process: &str, from: u32, request: MigrationRequest) -> R<String> {
        let op = "process_migration_start";
        let caller = self.gate(ctx, op, process, &[MIGRATE]).await?;
        self.targets(&caller, op, process, from, &request).await?;
        let id = format!("mig_{}", spot_storage_contract::RecordId::new_v7());
        let conv = |v: u32| i32::try_from(v).map_err(|_| VersionPortError::Internal);
        let (from_v, to_v) = (conv(from)?, conv(request.to)?);
        let mapping = mapping_json(request.mapping.as_ref());
        let (key, now, site) = (ctx.idempotency_key.as_deref(), self.now(), self.p.site.as_str());
        let client = self.p.pool.get().await.map_err(db)?;
        let created = client.query_opt(
            "INSERT INTO spot_bpm_migration (tenant, id, process, from_version, to_version, mapping, state, \
             started_by, idem_key, started_at) VALUES ($1, $2, $3, $4, $5, $6, 'running', $7, $8, $9) \
             ON CONFLICT (tenant, idem_key) WHERE idem_key IS NOT NULL DO NOTHING RETURNING id",
            &[&site, &id, &process, &from_v, &to_v, &mapping, &caller.user, &key, &now],
        ).await.map_err(db)?;
        let id = if created.is_some() { id } else {
            // The same key again: the first id, never a second batch.
            let row = client.query_opt(
                "SELECT id FROM spot_bpm_migration WHERE tenant = $1 AND idem_key = $2 AND process = $3 \
                 AND from_version = $4 AND to_version = $5",
                &[&site, &key, &process, &from_v, &to_v],
            ).await.map_err(db)?;
            match row {
                Some(row) => row.get::<_, String>(0),
                None => return self.deny(&caller, op, process, Some(from), VersionPortError::MigrationInvalid).await,
            }
        };
        self.audit(&caller.user, op, process, Some(from), "ok", (None, None)).await;
        self.wake.notify_one();
        Ok(id)
    }

    async fn do_progress(&self, ctx: &RequestCtx, process: &str, id: &str) -> R<MigrationProgress> {
        let op = "process_migration_get";
        let caller = self.gate(ctx, op, process, &[MIGRATE]).await?;
        let client = self.p.pool.get().await.map_err(db)?;
        let row = client.query_opt(
            "SELECT state, moved, skipped, failed, started_at, ended_at FROM spot_bpm_migration \
             WHERE tenant = $1 AND id = $2 AND process = $3",
            &[&self.p.site.as_str(), &id, &process],
        ).await.map_err(db)?;
        let Some(row) = row else { return self.deny(&caller, op, process, None, VersionPortError::NotFound).await };
        let count = |i: usize| u32::try_from(row.get::<_, i64>(i)).unwrap_or(u32::MAX);
        let state = match row.get::<_, String>(0).as_str() {
            "completed" => MigrationState::Completed,
            "failed" => MigrationState::Failed,
            _ => MigrationState::Running,
        };
        Ok(MigrationProgress {
            state, moved: count(1), skipped: count(2), failed: count(3),
            started_at: rfc(row.get::<_, OffsetDateTime>(4)),
            ended_at: row.get::<_, Option<OffsetDateTime>>(5).map(rfc),
        })
    }

    /// Ends a migration (`completed` or `failed`), counting `failed` more failures.
    async fn finish(&self, id: &str, state: &str, failed: i64) -> Result<()> {
        let client = self.p.pool.get().await.map_err(|e| Error(e.to_string()))?;
        let done = client.execute(
            "UPDATE spot_bpm_migration SET state = $3, failed = failed + $4, ended_at = $5, lease_until = NULL \
             WHERE tenant = $1 AND id = $2",
            &[&self.p.site.as_str(), &id, &state, &failed, &self.now()],
        ).await;
        done.map(|_| ()).map_err(|e| Error(e.to_string()))
    }

    /// One worker pass: claims one migration whose lease is free and moves up to `batch` Runs from
    /// its cursor. `true` means a migration was worked, so the caller may call again at once.
    ///
    /// # Errors
    /// The database failed; the lease lapses and the next pass resumes from the cursor.
    pub async fn migrate_pass(&self) -> Result<bool> {
        let (site, now) = (self.p.site.as_str(), self.now());
        let until = now + Duration::from_secs(self.p.settings.lease_secs);
        let e = |e: &dyn std::fmt::Display| Error(e.to_string());
        let client = self.p.pool.get().await.map_err(|x| e(&x))?;
        let claimed = client.query_opt(
            "UPDATE spot_bpm_migration SET lease_holder = $2, lease_until = $3 WHERE tenant = $1 AND id = \
             (SELECT id FROM spot_bpm_migration WHERE tenant = $1 AND state = 'running' \
               AND (lease_holder = $2 OR lease_until IS NULL OR lease_until < $4) ORDER BY started_at LIMIT 1 FOR UPDATE SKIP LOCKED) \
             RETURNING id, process, from_version, to_version, mapping, cursor, started_by, failed",
            &[&site, &self.holder, &until, &now],
        ).await.map_err(|x| e(&x))?;
        let Some(row) = claimed else { return Ok(false) };
        let (id, process, cursor): (String, String, String) = (row.get(0), row.get(1), row.get(5));
        let version = |i: usize| u32::try_from(row.get::<_, i32>(i)).unwrap_or(0);
        let (from, to, mapping, actor, failed): (u32, u32, Json, String, i64) = (version(2), version(3), row.get(4), row.get(6), row.get(7));
        let store = &self.p.definitions;
        let (sf, st) = (store.get(&process, from).await.ok().flatten(), store.get(&process, to).await.ok().flatten());
        let defs = sf.as_ref().zip(st.as_ref().filter(|t| t.state == StoredState::Published))
            .and_then(|(a, b)| Some((definition_of(a)?, definition_of(b)?)));
        let mapped = defs.as_ref().and_then(|(_, b)| mapping_of(Some(&mapping_back(&mapping)), b).ok());
        let (Some((a, b)), Some(map)) = (defs, mapped) else {
            // The target was retired or removed after the start: nothing moves, the batch fails.
            self.finish(&id, "failed", 1).await?;
            return Ok(true);
        };
        let ids = self.case_ids(&process, from, &cursor, i64::from(self.p.settings.batch)).await;
        let ids = ids.map_err(|_| Error("reading the Runs to migrate failed".into()))?;
        if ids.is_empty() {
            self.finish(&id, if failed > 0 { "failed" } else { "completed" }, 0).await?;
            return Ok(true);
        }
        let (mut skipped, mut last) = (0_i64, cursor);
        for case in ids {
            last.clone_from(&case);
            let record = self.load(&case).await.map_err(|_| Error("reading a Run failed".into()))?;
            let Some(record) = record else { skipped += 1; continue };
            let Ok(plan) = evaluate(&record, &a, &b, &map) else { skipped += 1; continue };
            match self.apply(&plan, &id, &actor, self.now()).await {
                Ok(true) => {}
                Ok(false) => skipped += 1,
                Err(error) => {
                    tracing::warn!(target: AUDIT, %error, "a Run could not be migrated");
                    self.finish(&id, "failed", 1).await?;
                    return Ok(true);
                }
            }
        }
        let until = self.now() + Duration::from_secs(self.p.settings.lease_secs);
        client.execute(
            "UPDATE spot_bpm_migration SET cursor = $3, skipped = skipped + $4, lease_until = $5 \
             WHERE tenant = $1 AND id = $2 AND lease_holder = $6",
            &[&site, &id, &last, &skipped, &until, &self.holder],
        ).await.map_err(|x| e(&x))?;
        Ok(true)
    }

    /// Moves one Run through `PgCaseStore::apply_migration`: the Case row, its variables, the
    /// migration's counter and the audit row commit together or not at all. `false` means the Run
    /// was skipped (busy, or changed after the plan was made): it is never forced.
    async fn apply(&self, plan: &MigrationPlan, migration: &str, actor: &str, now: OffsetDateTime) -> std::result::Result<bool, String> {
        let run = &plan.migrated().run;
        let nodes = serde_json::to_value(&run.nodes).map_err(|e| e.to_string())?;
        let taken = serde_json::to_value(&run.taken).map_err(|e| e.to_string())?;
        let defaulted = plan.variable_changes().iter().filter(|c| matches!(c.action, VariableAction::Defaulted { .. })).count();
        let audit = MigrationAudit {
            topic: MIGRATION_AUDIT_TOPIC.to_owned(), case: plan.case().clone(), from: plan.from().clone(),
            to: plan.to().clone(), plan_digest: plan.plan_digest().to_owned(), actor: Some(actor.to_owned()),
            store_version: plan.store_version(), nodes_changed: plan.node_changes().len(),
            variables_defaulted: defaulted, at: now,
        };
        let payload = audit.to_outbox_json();
        let mv = MigrationMove {
            case: plan.case(), from_name: &plan.from().name, from_version: plan.from().version,
            expected_revision: plan.store_version(), to_version: plan.to().version, to_hash: &plan.to().hash,
            nodes: &nodes, taken: &taken, has_ready: !run.ready_nodes().is_empty(),
            variables: &plan.migrated().variables, migration_id: migration, plan_digest: plan.plan_digest(),
            audit_payload: &payload, at: now,
        };
        match self.cases().apply_migration(&mv).await.map_err(|e| e.to_string())? {
            MigrationOutcome::Moved => Ok(true),
            MigrationOutcome::Skipped(_) => Ok(false),
        }
    }
}

type Op<'a, T> = BoxFuture<'a, R<T>>;

#[rustfmt::skip]
impl VersionPort for VersionHost {
    fn list_versions<'a>(&'a self, ctx: &'a RequestCtx, process: &'a str, cursor: Option<&'a str>, limit: u32) -> Op<'a, VersionPage> {
        Box::pin(self.do_list(ctx, process, cursor, limit))
    }
    fn publish<'a>(&'a self, ctx: &'a RequestCtx, process: &'a str, request: PublishRequest) -> Op<'a, Published> {
        Box::pin(self.do_publish(ctx, process, request))
    }
    fn get_version<'a>(&'a self, ctx: &'a RequestCtx, process: &'a str, version: u32) -> Op<'a, VersionDetail> {
        Box::pin(self.do_get(ctx, process, version))
    }
    fn retire<'a>(&'a self, ctx: &'a RequestCtx, process: &'a str, version: u32) -> Op<'a, VersionSummary> {
        Box::pin(self.do_retire(ctx, process, version))
    }
    fn plan_migration<'a>(&'a self, ctx: &'a RequestCtx, process: &'a str, from: u32, request: MigrationRequest) -> Op<'a, PortPlan> {
        Box::pin(self.do_plan(ctx, process, from, request))
    }
    fn start_migration<'a>(&'a self, ctx: &'a RequestCtx, process: &'a str, from: u32, request: MigrationRequest) -> Op<'a, String> {
        Box::pin(self.do_start(ctx, process, from, request))
    }
    fn get_migration<'a>(&'a self, ctx: &'a RequestCtx, process: &'a str, id: &'a str) -> Op<'a, MigrationProgress> {
        Box::pin(self.do_progress(ctx, process, id))
    }
}

// ---------------------------------------------------------------------------------------------
// The service and the registration

struct VersionService {
    host: Arc<VersionHost>,
}

#[rustfmt::skip]
impl Service for VersionService {
    fn name(&self) -> &'static str { "process_versions" }

    // A broken version service stops publishing and migrating; Runs and documents go on.
    fn level(&self) -> Level { Level::Optional }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { self.host.p.pool.get().await.map(|_| ()).map_err(|e| e.to_string()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let tick = Duration::from_secs(self.host.p.settings.tick_secs);
            loop {
                if *stop.borrow() { return Ok(()); }
                if self.host.dirty.load(Ordering::SeqCst) { self.host.reload().await; }
                match self.host.migrate_pass().await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(error) => tracing::warn!(target: AUDIT, %error, "a migration pass failed"),
                }
                tokio::select! {
                    changed = stop.changed() => { if changed.is_err() || *stop.borrow() { return Ok(()); } }
                    () = self.host.wake.notified() => {}
                    () = tokio::time::sleep(tick) => {}
                }
            }
        })
    }
}

/// The ports of the sites built so far, so a reload build mounts the same routes.
fn built() -> &'static Mutex<HashMap<String, Arc<dyn VersionPort>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<dyn VersionPort>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// Registers the process versions module: its service, the `VersionPort` and its routes. Inert when
/// `services.process_versions.enabled = false` or the process engine is off.
///
/// # Errors
/// A setting that does not decode or is out of range; tables that cannot be prepared; a port another
/// module already provided.
#[rustfmt::skip]
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_versions")?;
    settings.validate().map_err(Error)?;
    if !settings.enabled { return Ok(()); }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let port: Arc<dyn VersionPort> = if ctx.assembling() {
        let Some(process) = ctx.port::<ProcessHost>() else {
            tracing::info!(site = %site_name, "the process engine is off; no process versions");
            return Ok(());
        };
        let site = SiteId::new(&site_name).map_err(|e| Error(e.to_string()))?;
        let pool = env.backend.pool();
        let definitions = Arc::new(PgDefinitionStore::new(pool.clone()).with_tenant(site.as_str()));
        definitions.prepare().await.map_err(|e| Error(format!("the definition store: {}", e.code())))?;
        let principals = Arc::clone(&env.permission.principals) as Arc<dyn spot_document::engine::PrincipalSource>;
        let host = VersionHost::new(Parts {
            site, pool, definitions, process,
            access: Arc::new(LiveAccess { live: Arc::clone(env.live), principals }),
            guard: ctx.port::<PublishGuard>(),
            triggers: ctx.port::<TriggerHost>(),
            trigger_settings: ctx.settings_or_default("process_triggers")?,
            clock: Arc::clone(ctx.clock()),
            settings,
        });
        host.prepare().await?;
        ctx.add(Arc::new(VersionService { host: Arc::clone(&host) }));
        let port: Arc<dyn VersionPort> = host;
        ctx.provide::<dyn VersionPort>(Arc::clone(&port))?;
        built().lock().unwrap_or_else(|p| p.into_inner()).insert(site_name, Arc::clone(&port));
        port
    } else {
        match built().lock().unwrap_or_else(|p| p.into_inner()).get(&site_name).cloned() {
            Some(port) => port,
            None => return Ok(()),
        }
    };
    let mut ports = Ports::new();
    ports.provide::<dyn VersionPort>(port);
    // The two keyed writes (publish and migration start) replay by their key: held in memory
    // here, as the process routes' own store is.
    ports.provide::<dyn IdempotencyStore>(Arc::new(MemoryIdempotencyStore::new(Arc::clone(
        ctx.clock(),
    ))));
    let registration = Registration {
        name: "process_version", serves: routes::process_version::SERVES,
        required_ports: routes::process_version::required_ports, routes: routes::process_version::routes,
        public: None,
    };
    // The host authenticates every request before the router sees it, and the port again.
    let mounted = routes::mount(&[registration], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(module = *module, ?reason, "the process version routes were not mounted");
    }
    let router = mounted.authenticated;
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn settings_validate_and_refuse_unknown_keys() {
        assert!(Settings::default().validate().is_ok());
        assert!(toml::from_str::<Settings>("batches = 5").is_err());
        assert!(
            Settings {
                batch: 0,
                ..Settings::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            Settings {
                lease_secs: 5,
                ..Settings::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn a_mapping_survives_its_stored_form() {
        let mut mapping = PortMapping::default();
        mapping.nodes.insert("a".into(), "b".into());
        mapping.required_variables.push("x.y".into());
        mapping.variable_defaults.insert("x.y".into(), json!(1));
        assert_eq!(mapping_back(&mapping_json(Some(&mapping))), mapping);
        assert_eq!(code_of(&VersionPortError::NotFound), "process.not_found");
    }
}
