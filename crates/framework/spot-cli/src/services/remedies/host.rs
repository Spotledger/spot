//! The hosted remedy registry: list, dry-run, apply and undo as audited operations, behind the tier
//! authoriser, on a world the site provides.
//!
//! # The rules this host adds to `spot_remedies::Registry`
//!
//! - **Tenant-scoped.** A host serves one site. A call naming another tenant is refused
//!   (`remedies_host.wrong_tenant`) and audited; the agent's identity is looked up in this site's
//!   registry, so an id registered elsewhere is not an agent here.
//! - **The tier comes from stored data**, through [`AgentSource`] (production: the agent tier
//!   service, whose effective tier is T0 while the unattended policy is closed). The registry's
//!   authoriser then decides every call; a refusal is recorded by the framework.
//! - **Listing is tier-filtered**: an agent sees the remedies its tier, domain and status allow.
//! - **Dry-run changes nothing**: it plans on the live world through read-only operations and
//!   returns the sealed plan. Apply accepts only a plan this host's dry-run issued.
//! - **Apply goes through the fix pipeline** ([`ApplyPath`]) for every remedy above T1 or not
//!   reversible. Until the pipeline is installed those applies are refused
//!   (`remedies_host.apply_closed`); a reversible T1 remedy (a setting) runs directly.
//! - **Everything is audited, apply twice**: an intent record is appended to the audit chain
//!   *before* the world is touched (if it cannot be written, nothing runs), and the framework's own
//!   events (authorised, refused, planned, applied, undone, undo refused) after. Records carry the
//!   remedy id, the agent, the instant and a plain sentence, never a parameter value.
//! - **Undo is time-boxed** by `undo_window_secs` and refuses a later conflict, as the framework
//!   does.
//! - **Undo survives a restart** ([`UndoStore`]): the record of every live apply is written to the
//!   store before the answer returns, removed after an undo, restored into the registry at start
//!   ([`RemedyHost::restore_undo_records`]) and swept when its window has closed
//!   ([`RemedyHost::sweep_undo`]). If the record cannot be written the host undoes the change at
//!   once, audits the refusal and answers `remedies_host.undo_store_unavailable`: a remedy applied
//!   with no way back is never left standing. The rehearsal registry never touches the store.
//!
//! The framework's operations are synchronous, so each runs on a blocking thread.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use serde_json::Value as Json;
use spot_agent_tiers::{AgentIdentity, Approval, Policy, Status, Tier};
use spot_audit_chain::{AuditRecord, Payload, PayloadValue};
use spot_core::Clock;
use spot_mcp::AuditSink;
use spot_remedies::testing::{Failure, Report};
use spot_remedies::{
    Applied, AuditEvent, Gate, Plan, Registry, RemedyError, RemedyId, RemedyMeta, RemedySettings,
    Risk, UndoToken, Undone,
};

use super::undo_store::{MemoryUndoStore, StoredUndo, UndoStore};
use super::world::{Catalogue, Collect, CtxRequest, WorldAccess, WorldKind};

/// The code of an apply that was rolled back because its undo record could not be kept.
pub const UNDO_STORE_UNAVAILABLE: &str = "remedies_host.undo_store_unavailable";
/// The code of the readiness text for stored undo records that could not be restored.
pub const UNDO_RESTORE_FAILED: &str = "remedies_host.undo_restore_failed";

/// A boxed `Send` future.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Who the agent is, from stored data.
pub trait AgentSource: Send + Sync {
    /// The agent `agent_id` of `tenant` as the tier authoriser sees it (its effective tier as
    /// `max_tier`), or `None` when it is not a registered agent there.
    fn identity<'a>(&'a self, tenant: &'a str, agent_id: &'a str)
    -> Fut<'a, Option<AgentIdentity>>;
}

/// What the fix pipeline is handed to apply one plan.
pub struct PipelineRun<'a> {
    /// The tenant.
    pub tenant: &'a str,
    /// The acting agent as the authoriser sees it.
    pub agent: &'a AgentIdentity,
    /// The plan this host's dry-run issued.
    pub plan: &'a Plan,
    /// The pipeline's way to run the remedy: on a rehearsal copy first, then live.
    pub direct: DirectApply<'a>,
}

/// The apply path of the fix pipeline (WP-AI-08.T4 installs it). The host never applies a T2 or T3
/// remedy itself: the pipeline rehearses, applies live through [`DirectApply`], verifies and rolls
/// back through [`DirectApply::undo`].
pub trait ApplyPath: Send + Sync {
    /// Applies `run.plan`.
    fn apply<'a>(&'a self, run: PipelineRun<'a>) -> Fut<'a, Result<Applied, HostError>>;
}

/// Why a host operation did not happen. [`HostError::code`] is the stable code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostError {
    /// The framework refused or failed (carries its own `remedies.*` code).
    Remedy(RemedyError),
    /// The call named another tenant.
    WrongTenant,
    /// The caller is not a registered agent of this site.
    NotAnAgent,
    /// No world is provided, so no remedy can run.
    NoWorld,
    /// The world could not be opened.
    WorldUnavailable,
    /// The remedy must go through the fix pipeline and none is installed.
    ApplyClosed,
    /// The audit chain did not take the record, so nothing ran (or, after a run, the result is
    /// reported through [`RunReport`]).
    AuditUnavailable,
    /// The undo store could not be read or written. After an apply the change was undone at once
    /// (see [`RemedyHost::apply`]).
    UndoStoreUnavailable,
    /// The blocking task failed.
    Internal,
}

impl HostError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            HostError::Remedy(error) => error.code(),
            HostError::WrongTenant => "remedies_host.wrong_tenant",
            HostError::NotAnAgent => "remedies_host.not_an_agent",
            HostError::NoWorld => "remedies_host.no_world",
            HostError::WorldUnavailable => "remedies_host.world_unavailable",
            HostError::ApplyClosed => "remedies_host.apply_closed",
            HostError::AuditUnavailable => "remedies_host.audit_unavailable",
            HostError::UndoStoreUnavailable => UNDO_STORE_UNAVAILABLE,
            HostError::Internal => "remedies_host.internal",
        }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Remedy(error) => write!(f, "{}: {}", error.code(), error.sentence()),
            other => {
                write!(f, "{}: ", other.code())?;
                f.write_str(match other {
                    HostError::WrongTenant => "the call is about another tenant",
                    HostError::NotAnAgent => "the caller is not a registered agent of this site",
                    HostError::NoWorld => "no world is provided, so no remedy can run",
                    HostError::WorldUnavailable => "the world could not be opened",
                    HostError::ApplyClosed => {
                        "this remedy must go through the fix pipeline, which is not installed"
                    }
                    HostError::AuditUnavailable => "the audit chain did not take the record",
                    HostError::UndoStoreUnavailable => {
                        "the undo record could not be kept, so the change was not left standing"
                    }
                    _ => "the operation failed",
                })
            }
        }
    }
}

impl std::error::Error for HostError {}

impl From<RemedyError> for HostError {
    fn from(error: RemedyError) -> Self {
        HostError::Remedy(error)
    }
}

/// One remedy as an agent sees it in a listing.
#[derive(Clone, Debug, PartialEq)]
pub struct RemedySummary {
    /// The remedy id.
    pub id: RemedyId,
    /// The title.
    pub title: String,
    /// The tier it needs.
    pub tier: Tier,
    /// How much it can go wrong.
    pub risk: Risk,
    /// The document types it may affect.
    pub affects: Vec<String>,
    /// The precondition, in words.
    pub preconditions: String,
    /// Whether undo is meaningful.
    pub reversible: bool,
    /// The parameter schema (JSON Schema).
    pub params_schema: Json,
}

/// The self-test of one remedy, run on a rehearsal world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelfTest {
    /// The remedy.
    pub remedy: RemedyId,
    /// What the run produced.
    pub outcome: Result<Report, Failure>,
}

/// How an apply is routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    /// Directly, on the live world: a reversible T1 setting.
    Direct,
    /// Through the fix pipeline.
    Pipeline,
}

/// The pipeline's handle on one agent's plan: run the remedy on a chosen world.
pub struct DirectApply<'a> {
    host: &'a RemedyHost,
    agent: &'a AgentIdentity,
    plan: &'a Plan,
    approval: Option<Approval>,
}

impl DirectApply<'_> {
    /// Applies the plan on `kind`. On the rehearsal world the remedy is planned afresh on the
    /// snapshot first; on the live world the plan must be the one the host issued (the hash binds
    /// it), and the framework stores the undo record.
    ///
    /// # Errors
    ///
    /// A [`HostError`] from the framework (refusal, stale plan, failed precondition or operation).
    pub async fn apply(&self, kind: WorldKind) -> Result<Applied, HostError> {
        let gate = self.gate();
        let agent = self.agent.clone();
        let id = self.plan.remedy.clone();
        match kind {
            WorldKind::Live => self.host.apply_live(self.agent, self.plan, gate).await,
            WorldKind::Rehearsal => {
                let params = self.plan.params.clone();
                let planned = {
                    let (agent, id, gate) = (agent.clone(), id.clone(), gate.clone());
                    self.host
                        .run(kind, &self.host.rehearsal, self.agent, move |reg, ctx| {
                            reg.dry_run(&agent, &id, &params, &gate, ctx)
                        })
                        .await?
                };
                self.host
                    .run(kind, &self.host.rehearsal, self.agent, move |reg, ctx| {
                        reg.apply(&agent, &id, &planned, &gate, ctx)
                    })
                    .await
            }
        }
    }

    /// Undoes an apply on `kind` by its token (the pipeline's rollback).
    ///
    /// # Errors
    ///
    /// A [`HostError`] from the framework (unknown or expired token, conflict, irreversible).
    pub async fn undo(&self, kind: WorldKind, token: &UndoToken) -> Result<Undone, HostError> {
        let gate = self.gate();
        let agent = self.agent.clone();
        let id = self.plan.remedy.clone();
        let registry = match kind {
            WorldKind::Live => &self.host.live,
            WorldKind::Rehearsal => &self.host.rehearsal,
        };
        let kept = token.clone();
        let token = token.clone();
        let undone = self
            .host
            .run(kind, registry, self.agent, move |reg, ctx| {
                reg.undo(&agent, &id, &token, &gate, ctx)
            })
            .await?;
        if kind == WorldKind::Live {
            self.host.forget_stored(&kept).await;
        }
        Ok(undone)
    }

    fn gate(&self) -> Gate {
        let gate = Gate::change();
        match &self.approval {
            Some(approval) => gate.with_approval(approval.clone()),
            None => gate,
        }
    }
}

/// What the host is built from.
pub struct Deps {
    /// The site (the only tenant this host serves).
    pub site: String,
    /// Where the remedies come from.
    pub catalogue: Arc<dyn Catalogue>,
    /// The framework settings (undo window, domain).
    pub settings: RemedySettings,
    /// The world remedies run on; `None` leaves the host unable to run one.
    pub world: Option<Arc<dyn WorldAccess>>,
    /// Who an agent is.
    pub agents: Arc<dyn AgentSource>,
    /// The fix pipeline, when installed.
    pub apply_path: Option<Arc<dyn ApplyPath>>,
    /// The audit chain.
    pub sink: Arc<dyn AuditSink>,
    /// The injected clock.
    pub clock: Arc<dyn Clock>,
}

/// The hosted registry of one site.
pub struct RemedyHost {
    site: String,
    catalogue: Arc<dyn Catalogue>,
    settings: RemedySettings,
    world: Option<Arc<dyn WorldAccess>>,
    agents: Arc<dyn AgentSource>,
    apply_path: RwLock<Option<Arc<dyn ApplyPath>>>,
    sink: Arc<dyn AuditSink>,
    clock: Arc<dyn Clock>,
    live: Arc<Registry>,
    rehearsal: Arc<Registry>,
    undo_store: Arc<dyn UndoStore>,
    restore_failed: AtomicUsize,
}

/// What [`RemedyHost::restore_undo_records`] did with the stored rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Rows put back into the live registry (rows already there count as restored).
    pub restored: usize,
    /// Rows kept in the store but not restored: another site's row, a row that does not parse,
    /// or a record the registry refused. The operator sees the count in readiness.
    pub failed: usize,
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

impl RemedyHost {
    /// Builds the host and its two registries (live and rehearsal) from the catalogue.
    ///
    /// # Errors
    ///
    /// A [`RemedyError`] when a remedy breaks a registry rule.
    pub fn new(deps: Deps) -> Result<Self, RemedyError> {
        let live = Registry::new(
            deps.catalogue.remedies(),
            Policy::default(),
            deps.settings.clone(),
        )?;
        let rehearsal = Registry::new(
            deps.catalogue.remedies(),
            Policy::default(),
            deps.settings.clone(),
        )?;
        Ok(Self {
            site: deps.site,
            catalogue: deps.catalogue,
            settings: deps.settings,
            world: deps.world,
            agents: deps.agents,
            apply_path: RwLock::new(deps.apply_path),
            sink: deps.sink,
            clock: deps.clock,
            live: Arc::new(live),
            rehearsal: Arc::new(rehearsal),
            undo_store: Arc::new(MemoryUndoStore::new()),
            restore_failed: AtomicUsize::new(0),
        })
    }

    /// Keeps undo records in `store` instead of the in-memory default (the site's database in
    /// production). Call it right after [`RemedyHost::new`], before the host serves a call.
    #[must_use]
    pub fn with_undo_store(mut self, store: Arc<dyn UndoStore>) -> Self {
        self.undo_store = store;
        self
    }

    /// Creates what the undo store needs (idempotent).
    ///
    /// # Errors
    ///
    /// [`HostError::UndoStoreUnavailable`].
    pub async fn prepare_undo_store(&self) -> Result<(), HostError> {
        self.undo_store
            .ensure_schema()
            .await
            .map_err(|_| HostError::UndoStoreUnavailable)
    }

    /// Puts every unexpired stored undo record back into the live registry, so a token handed out
    /// before a restart can still be undone. Idempotent: a record already restored is a no-op.
    ///
    /// A row that belongs to another site, does not parse, or is refused by the registry is
    /// **kept** in the store and counted in [`RestoreReport::failed`] (also available from
    /// [`RemedyHost::undo_restore_failures`]); it is never dropped silently. Nothing about the row
    /// is logged beyond the count.
    ///
    /// # Errors
    ///
    /// [`HostError::UndoStoreUnavailable`] when the rows cannot be read.
    pub async fn restore_undo_records(&self) -> Result<RestoreReport, HostError> {
        let now = self.now();
        let entries = self
            .undo_store
            .list_open(now)
            .await
            .map_err(|_| HostError::UndoStoreUnavailable)?;
        let mut report = RestoreReport::default();
        for entry in entries {
            let restorable = match entry.record {
                Some(record) if entry.site == self.site => {
                    self.live.restore_undo(record, now).is_ok()
                }
                _ => false,
            };
            if restorable {
                report.restored += 1;
            } else {
                report.failed += 1;
            }
        }
        self.restore_failed.store(report.failed, Ordering::Relaxed);
        if report.failed > 0 {
            tracing::warn!(
                site = %self.site,
                code = UNDO_RESTORE_FAILED,
                failed = report.failed,
                "remedies: stored undo records could not be restored and were kept"
            );
        }
        Ok(report)
    }

    /// How many stored undo records the last restore could not put back.
    #[must_use]
    pub fn undo_restore_failures(&self) -> usize {
        self.restore_failed.load(Ordering::Relaxed)
    }

    /// Deletes the stored undo records whose window has closed and returns how many went.
    ///
    /// # Errors
    ///
    /// [`HostError::UndoStoreUnavailable`].
    pub async fn sweep_undo(&self) -> Result<u64, HostError> {
        self.undo_store
            .sweep(self.now())
            .await
            .map_err(|_| HostError::UndoStoreUnavailable)
    }

    /// Applies a plan on the live world and keeps its undo record. If the record cannot be
    /// written, a reversible change is undone at once and the refusal is audited.
    async fn apply_live(
        &self,
        agent: &AgentIdentity,
        plan: &Plan,
        gate: Gate,
    ) -> Result<Applied, HostError> {
        let (who, id, sealed, applying) = (
            agent.clone(),
            plan.remedy.clone(),
            plan.clone(),
            gate.clone(),
        );
        let applied = self
            .run(WorldKind::Live, &self.live, agent, move |reg, ctx| {
                reg.apply(&who, &id, &sealed, &applying, ctx)
            })
            .await?;
        self.keep_undo(agent, &applied, gate).await?;
        Ok(applied)
    }

    /// Writes the undo record of a live apply to the store (fail closed, see [`Self::apply_live`]).
    async fn keep_undo(
        &self,
        agent: &AgentIdentity,
        applied: &Applied,
        gate: Gate,
    ) -> Result<(), HostError> {
        let Some(record) = self.live.undo_record(&applied.undo_token) else {
            return Ok(());
        };
        let row = StoredUndo {
            site: self.site.clone(),
            agent: agent.id.clone(),
            record,
        };
        if self.undo_store.put(&row).await.is_ok() {
            return Ok(());
        }
        let remedy = row.record.remedy.clone();
        if !row.record.undo.is_reversible() {
            // There was never a way back for this remedy, so there is nothing to roll back; the
            // gap is on the audit chain.
            self.append(&[self.host_record(
                "undo_store_unavailable",
                &remedy,
                &agent.id,
                "the undo record of an irreversible remedy could not be kept",
            )])
            .await;
            return Ok(());
        }
        let (who, id, token) = (agent.clone(), remedy.clone(), applied.undo_token.clone());
        let rolled = self
            .run(WorldKind::Live, &self.live, agent, move |reg, ctx| {
                reg.undo(&who, &id, &token, &gate, ctx)
            })
            .await;
        let detail = if rolled.is_ok() {
            "the undo record could not be kept, so the change was undone at once"
        } else {
            "the undo record could not be kept and the change could not be undone; it needs an operator"
        };
        self.append(&[self.host_record("undo_store_unavailable", &remedy, &agent.id, detail)])
            .await;
        Err(HostError::UndoStoreUnavailable)
    }

    /// Removes a token's stored record after a successful undo. A failure is logged (the code
    /// only): the row then waits for the sweep, and a restore of it can only conflict.
    async fn forget_stored(&self, token: &UndoToken) {
        if self.undo_store.remove(token).await.is_err() {
            tracing::error!(
                site = %self.site,
                code = UNDO_STORE_UNAVAILABLE,
                "remedies: an undone record could not be removed from the undo store"
            );
        }
    }

    /// How many remedies are in the catalogue.
    #[must_use]
    pub fn len(&self) -> usize {
        self.live.len()
    }

    /// Whether the catalogue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// Whether a world is provided (a remedy can run).
    #[must_use]
    pub fn has_world(&self) -> bool {
        self.world.is_some()
    }

    /// Whether the fix pipeline is installed.
    #[must_use]
    pub fn has_pipeline(&self) -> bool {
        self.pipeline().is_some()
    }

    /// Installs the fix pipeline's apply path. Ports flow forward only (a module sees what an
    /// earlier one provides), and the pipeline registers after this host, so it installs itself
    /// here, the way the MCP service takes its gates. Installing again replaces it.
    pub fn install_apply_path(&self, path: Arc<dyn ApplyPath>) {
        *self
            .apply_path
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
    }

    fn pipeline(&self) -> Option<Arc<dyn ApplyPath>> {
        self.apply_path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// One remedy's metadata (tier, risk, undo kind, parameter schema), for a pipeline that
    /// normalises its plan.
    #[must_use]
    pub fn meta(&self, id: &RemedyId) -> Option<RemedyMeta> {
        self.live.meta(id)
    }

    /// The world remedies run on, when one is provided (the pipeline reads state fingerprints
    /// through it; it never changes the world except through [`DirectApply`]).
    #[must_use]
    pub fn world(&self) -> Option<Arc<dyn WorldAccess>> {
        self.world.clone()
    }

    /// The framework settings.
    #[must_use]
    pub fn settings(&self) -> &RemedySettings {
        &self.settings
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// Proves every remedy's own scenario (precondition, dry-run, apply, invariants, undo or the
    /// irreversible refusal) on the in-memory rehearsal world, on a fresh registry so the live
    /// undo log is untouched.
    ///
    /// # Errors
    ///
    /// A [`RemedyError`] when the registry cannot be built.
    pub fn self_test_all(&self) -> Result<Vec<SelfTest>, RemedyError> {
        let registry = Registry::new(
            self.catalogue.remedies(),
            Policy::default(),
            self.settings.clone(),
        )?;
        let mut out = Vec::new();
        for id in registry.ids() {
            let Some(remedy) = registry.get(id) else {
                continue;
            };
            let spec = remedy.self_test();
            out.push(SelfTest {
                remedy: id.clone(),
                outcome: spot_remedies::remedy_test::run(&registry, id, &spec),
            });
        }
        Ok(out)
    }

    async fn agent(&self, tenant: &str, agent_id: &str) -> Result<AgentIdentity, HostError> {
        if tenant != self.site {
            return Err(HostError::WrongTenant);
        }
        self.agents
            .identity(tenant, agent_id)
            .await
            .ok_or(HostError::NotAnAgent)
    }

    /// The remedies the agent's tier, domain and status allow, in id order.
    ///
    /// # Errors
    ///
    /// [`HostError::WrongTenant`] and [`HostError::NotAnAgent`].
    pub async fn list(
        &self,
        tenant: &str,
        agent_id: &str,
    ) -> Result<Vec<RemedySummary>, HostError> {
        let agent = self.agent(tenant, agent_id).await?;
        if agent.status != Status::Active || agent.domain != self.settings.domain {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for id in self.live.ids() {
            let Some(meta) = self.live.meta(id) else {
                continue;
            };
            if meta.tier > agent.max_tier {
                continue;
            }
            out.push(RemedySummary {
                id: id.clone(),
                title: meta.title.clone(),
                tier: meta.tier,
                risk: meta.risk,
                affects: meta.affects.iter().map(|d| d.as_str().to_owned()).collect(),
                preconditions: meta.preconditions.clone(),
                reversible: meta.undo.is_reversible(),
                params_schema: meta.params.to_json_schema(),
            });
        }
        Ok(out)
    }

    /// Plans a remedy without changing anything and returns the sealed plan.
    ///
    /// # Errors
    ///
    /// A [`HostError`]: wrong tenant, not an agent, no world, the framework's refusal (tier,
    /// parameters, precondition), or an audit chain that did not take the records.
    pub async fn dry_run(
        &self,
        tenant: &str,
        agent_id: &str,
        remedy: &RemedyId,
        params: &Json,
        approval: Option<Approval>,
    ) -> Result<Plan, HostError> {
        let agent = self.refusable(tenant, agent_id, remedy).await?;
        let gate = gate_of(approval);
        let (id, params, who) = (remedy.clone(), params.clone(), agent.clone());
        self.run(WorldKind::Live, &self.live, &agent, move |reg, ctx| {
            reg.dry_run(&who, &id, &params, &gate, ctx)
        })
        .await
    }

    /// Applies a plan this host's dry-run issued: directly for a reversible T1 remedy, through the
    /// fix pipeline for anything else (refused while none is installed).
    ///
    /// # Errors
    ///
    /// A [`HostError`]; the intent record is written first and a failure to write it stops the
    /// call before the world is touched.
    pub async fn apply(
        &self,
        tenant: &str,
        agent_id: &str,
        plan: &Plan,
        approval: Option<Approval>,
    ) -> Result<Applied, HostError> {
        let agent = self.refusable(tenant, agent_id, &plan.remedy).await?;
        let route = self.route_of(&plan.remedy)?;
        let pipeline = self.pipeline();
        if route == Route::Pipeline && pipeline.is_none() {
            self.append(&[self.host_record(
                "apply_closed",
                &plan.remedy,
                agent_id,
                "the remedy needs the fix pipeline, which is not installed",
            )])
            .await;
            return Err(HostError::ApplyClosed);
        }
        self.intent("apply_started", &plan.remedy, agent_id).await?;
        match (route, &pipeline) {
            (Route::Pipeline, Some(path)) => {
                let run = PipelineRun {
                    tenant,
                    agent: &agent,
                    plan,
                    direct: DirectApply {
                        host: self,
                        agent: &agent,
                        plan,
                        approval,
                    },
                };
                path.apply(run).await
            }
            _ => self.apply_live(&agent, plan, gate_of(approval)).await,
        }
    }

    /// Undoes an apply by its token, inside the undo window.
    ///
    /// # Errors
    ///
    /// A [`HostError`]; the framework's unknown, expired, conflict and irreversible refusals are
    /// audited.
    pub async fn undo(
        &self,
        tenant: &str,
        agent_id: &str,
        remedy: &RemedyId,
        token: &UndoToken,
        approval: Option<Approval>,
    ) -> Result<Undone, HostError> {
        let agent = self.refusable(tenant, agent_id, remedy).await?;
        self.intent("undo_started", remedy, agent_id).await?;
        let gate = gate_of(approval);
        let kept = token.clone();
        let (who, id, token) = (agent.clone(), remedy.clone(), token.clone());
        let undone = self
            .run(WorldKind::Live, &self.live, &agent, move |reg, ctx| {
                reg.undo(&who, &id, &token, &gate, ctx)
            })
            .await?;
        self.forget_stored(&kept).await;
        Ok(undone)
    }

    /// The agent, or a refusal that is audited.
    async fn refusable(
        &self,
        tenant: &str,
        agent_id: &str,
        remedy: &RemedyId,
    ) -> Result<AgentIdentity, HostError> {
        match self.agent(tenant, agent_id).await {
            Ok(agent) => Ok(agent),
            Err(error) => {
                self.append(&[self.host_record(
                    "call_refused",
                    remedy,
                    agent_id,
                    &error.to_string(),
                )])
                .await;
                Err(error)
            }
        }
    }

    fn route_of(&self, id: &RemedyId) -> Result<Route, HostError> {
        let meta = self.live.meta(id).ok_or_else(|| {
            HostError::Remedy(RemedyError::Unknown {
                remedy: id.as_str().to_owned(),
            })
        })?;
        if meta.tier <= Tier::T1 && meta.undo.is_reversible() {
            Ok(Route::Direct)
        } else {
            Ok(Route::Pipeline)
        }
    }

    /// Appends the intent record that must exist before the world is touched.
    async fn intent(&self, name: &str, remedy: &RemedyId, agent_id: &str) -> Result<(), HostError> {
        let record = self.host_record(name, remedy, agent_id, "about to run");
        self.sink
            .record(record)
            .await
            .map_err(|_| HostError::AuditUnavailable)
    }

    fn host_record(
        &self,
        name: &str,
        remedy: &RemedyId,
        agent_id: &str,
        detail: &str,
    ) -> AuditRecord {
        let mut payload = Payload::new();
        payload.insert(
            "remedy".to_owned(),
            PayloadValue::Text(remedy.as_str().to_owned()),
        );
        payload.insert("tenant".to_owned(), PayloadValue::Text(self.site.clone()));
        payload.insert(
            "detail".to_owned(),
            PayloadValue::Text(truncate(detail, 300)),
        );
        AuditRecord {
            kind: format!("remedy.{name}"),
            actor: format!("agent:{agent_id}"),
            on_behalf_of: None,
            subject: None,
            at: self.now().saturating_mul(1_000_000),
            payload,
            masked: true,
        }
    }

    fn record_of(&self, event: &AuditEvent) -> AuditRecord {
        let mut record = self.host_record(
            event.kind.as_str(),
            &event.remedy,
            &event.actor,
            &event.detail,
        );
        record.at = event.at.saturating_mul(1_000_000);
        record
    }

    /// Appends records; a failure is logged (the code only) and returned as a bool.
    async fn append(&self, records: &[AuditRecord]) -> bool {
        let mut all = true;
        for record in records {
            if self.sink.record(record.clone()).await.is_err() {
                all = false;
                tracing::error!(
                    site = %self.site,
                    "remedies: an audit record could not be appended to the chain"
                );
            }
        }
        all
    }

    /// Runs one framework call on a blocking thread over a world of `kind`, appends the events it
    /// left, and returns its result. The call's own result stands even when an event could not be
    /// appended after the world changed; before the world changed (a plan), a failed append is an
    /// error.
    async fn run<R: Send + 'static>(
        &self,
        kind: WorldKind,
        registry: &Arc<Registry>,
        agent: &AgentIdentity,
        call: impl FnOnce(&Registry, &mut spot_remedies::RemedyCtx<'_>) -> Result<R, RemedyError>
        + Send
        + 'static,
    ) -> Result<R, HostError> {
        let world = self.world.clone().ok_or(HostError::NoWorld)?;
        let registry = Arc::clone(registry);
        let settings = self.settings.clone();
        let actor = agent.id.clone();
        let now = self.now();
        let joined = tokio::task::spawn_blocking(move || {
            let mut audit = Collect::default();
            let mut slot: Option<Result<R, RemedyError>> = None;
            let mut call = Some(call);
            let opened = world.with_ctx(
                kind,
                CtxRequest {
                    actor: &actor,
                    now,
                    settings: &settings,
                    audit: &mut audit,
                },
                &mut |ctx| {
                    if let Some(call) = call.take() {
                        slot = Some(call(&registry, ctx));
                    }
                },
            );
            (opened, slot, audit.into_events())
        })
        .await
        .map_err(|_| HostError::Internal)?;
        let (opened, slot, events) = joined;
        if opened.is_err() {
            return Err(HostError::WorldUnavailable);
        }
        let records: Vec<AuditRecord> = events.iter().map(|e| self.record_of(e)).collect();
        let appended = self.append(&records).await;
        match slot {
            Some(Ok(value)) => {
                if !appended && kind == WorldKind::Live && is_read_only_plan(&records) {
                    return Err(HostError::AuditUnavailable);
                }
                Ok(value)
            }
            Some(Err(error)) => Err(HostError::Remedy(error)),
            None => Err(HostError::WorldUnavailable),
        }
    }
}

/// Whether the records are a plan only (nothing in them says the world changed).
fn is_read_only_plan(records: &[AuditRecord]) -> bool {
    !records
        .iter()
        .any(|r| r.kind.ends_with(".applied") || r.kind.ends_with(".undone"))
}

fn gate_of(approval: Option<Approval>) -> Gate {
    let gate = Gate::change();
    match approval {
        Some(approval) => gate.with_approval(approval),
        None => gate,
    }
}
