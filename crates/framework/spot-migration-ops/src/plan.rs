//! Owned by WP-MIG-04.T7 (plan and show).
//!
//! `plan` computes the release plan of a site, applies the gates and stores the plan by hash with
//! its author; `show` reads it back with its approval status. Nothing here changes the site.
//!
//! # `plan`
//!
//! 1. The actor needs the `migration.run` permission.
//! 2. The plan is `spot_migration_plan::plan_with` over the **from** and **to** system
//!    descriptions, the live schema diff and the site's risk table and cost model, all handed in
//!    as [`PlanInputs`] (the host loads them; this crate never reads a database or the apps).
//! 3. **Expand/contract** (WP-MIG-09): a contract change (a drop, a rename, a tightening) in the
//!    same release as the code that stops using the object is refused with the finding named. When
//!    the plan is destructive and the host supplies no gate, the plan is refused too (fail closed).
//! 4. **Conflicts** (WP-MIG-03): the findings about the site's customisations are reported next to
//!    the plan. They are informational: they are not part of the plan's digest.
//! 5. The plan is stored by hash (`PlanStore`), its author registered with the approval service
//!    (audited), and an additive plan the site policy auto-approves moves to `approved`.
//!
//! The same inputs give the same hash and no second row: a repeated `plan` (even by another
//! person, or two at once) returns the stored plan with `created == false`.
//!
//! # `show`
//!
//! Prints the stored plan, its state, its risk, and its approval status. With the current
//! inputs it also notices that the installed apps changed after planning: the plan is then
//! reported (and marked) `superseded`.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_expand_contract::Report as ExpandContractReport;
use spot_mgmt_ops::{OpId, Validated};
use spot_migration_apply::BoxFuture;
use spot_migration_approval::{
    ApprovalError, Checks, PlanDigest, Status, effective_risk, risk_name,
};
use spot_migration_conflicts::ConflictReport;
use spot_migration_plan::{
    BlockingClass, CostModel, ReleasePlan, RiskTable, SchemaPlan, SystemDescription, plan_with,
};

use crate::catalogue::{HandlerOutput, HandlerRegistry, OperationHandler};
use crate::context::{
    Actor, LifecycleContext, PlanHash, PlanState, PlanStore, StoredPlan, off_runtime,
};
use crate::error::LifecycleError;

/// The conflict detector, closed over the site's customisations by the host.
pub type ConflictsFn = Arc<dyn Fn(&ReleasePlan) -> ConflictReport + Send + Sync>;

/// The expand/contract gate, closed over the usage manifests of this and the previous release.
pub type ExpandContractFn = Arc<dyn Fn(&ReleasePlan) -> ExpandContractReport + Send + Sync>;

/// What `plan` needs, loaded by the host.
#[derive(Clone)]
pub struct PlanInputs {
    /// The live system (published metadata, applied patches, statistics).
    pub from: SystemDescription,
    /// The target system (the installed apps' effective metadata and release metadata).
    pub to: SystemDescription,
    /// The live schema diff (`spot-pg-migrate`'s plan mapped to a [`SchemaPlan`]).
    pub schema: SchemaPlan,
    /// The risk table (built-in rules plus the site policy).
    pub risk_table: RiskTable,
    /// The duration cost model.
    pub cost: CostModel,
    /// The conflict detector; `None` reports no findings.
    pub conflicts: Option<ConflictsFn>,
    /// The expand/contract gate; `None` refuses a destructive plan.
    pub expand_contract: Option<ExpandContractFn>,
}

impl PlanInputs {
    /// Inputs with the built-in risk table and cost model and no gates.
    #[must_use]
    pub fn new(from: SystemDescription, to: SystemDescription, schema: SchemaPlan) -> Self {
        Self {
            from,
            to,
            schema,
            risk_table: RiskTable::builtin(),
            cost: CostModel::default(),
            conflicts: None,
            expand_contract: None,
        }
    }

    /// The plan these inputs describe.
    #[must_use]
    pub fn release_plan(&self) -> ReleasePlan {
        plan_with(
            &self.from,
            &self.to,
            &self.schema,
            &self.risk_table,
            &self.cost,
        )
    }
}

/// Where the host loads [`PlanInputs`] from (installed apps, live database).
pub trait PlanSource: Send + Sync {
    /// The current inputs. An error is a refusal to plan, never an empty plan.
    fn inputs(&self) -> BoxFuture<'_, Result<PlanInputs, LifecycleError>>;
}

/// One finding of a gate: a stable code, the thing it is about, and a sentence (names only).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// A stable code.
    pub code: String,
    /// The object or artefact.
    pub target: String,
    /// What is wrong and what to do.
    pub detail: String,
}

/// A plan that was refused, with the findings that say why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanRefused {
    /// The refusal.
    pub error: LifecycleError,
    /// The findings behind it (may be empty).
    pub findings: Vec<Finding>,
}

impl From<LifecycleError> for PlanRefused {
    fn from(error: LifecycleError) -> Self {
        Self {
            error,
            findings: Vec::new(),
        }
    }
}

/// The result of `plan`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanReport {
    /// The plan hash (64 lowercase hex characters).
    pub hash: PlanHash,
    /// The state after planning (`approved` when the policy auto-approved it).
    pub state: PlanState,
    /// The risk class: `additive`, `needs_review` or `destructive`.
    pub risk: String,
    /// The author's subject.
    pub author: String,
    /// `false` when the same plan already existed and was returned unchanged.
    pub created: bool,
    /// How many changes the plan carries.
    pub changes: usize,
    /// The expected duration, in the estimate's own unit.
    pub estimate_expected: u64,
    /// The high bound of the duration.
    pub estimate_high: u64,
    /// `online`, `brief_lock` or `maintenance_window`.
    pub blocking: &'static str,
    /// How many customisation conflicts were found (informational).
    pub conflicts: usize,
    /// The conflict findings.
    pub findings: Vec<Finding>,
}

impl PlanReport {
    /// The plan as text, ending with the exact next command.
    #[must_use]
    pub fn render(&self, site: &str) -> String {
        let mut out = format!(
            "plan {}\n  risk: {}\n  state: {}\n  author: {}\n  changes: {}\n  estimated duration: {} (high {}), {}\n",
            self.hash.as_str(),
            self.risk,
            self.state.as_str(),
            self.author,
            self.changes,
            self.estimate_expected,
            self.estimate_high,
            self.blocking,
        );
        if !self.created {
            out.push_str("  (this plan already existed and is unchanged)\n");
        }
        if self.conflicts > 0 {
            out.push_str(&format!("  customisation conflicts: {}\n", self.conflicts));
            for finding in &self.findings {
                out.push_str(&format!(
                    "    {}: {} ({})\n",
                    finding.code, finding.target, finding.detail
                ));
            }
        }
        match self.state {
            PlanState::Approved => out.push_str(&format!(
                "next: spot migrate-plan show {} --site {site}\n",
                self.hash.as_str()
            )),
            _ => out.push_str(&format!(
                "next: spot migrate-approve approve {} --site {site} --comment \"...\"\n",
                self.hash.as_str()
            )),
        }
        out
    }
}

/// The subject an approval or a plan author is recorded under: the user, or `system`.
#[must_use]
pub fn subject(actor: &Actor) -> String {
    if actor.principal().is_system {
        "system".to_owned()
    } else {
        actor.principal().user.clone()
    }
}

fn blocking_name(blocking: BlockingClass) -> &'static str {
    match blocking {
        BlockingClass::Online => "online",
        BlockingClass::BriefLock => "brief_lock",
        BlockingClass::MaintenanceWindow => "maintenance_window",
    }
}

pub(crate) fn map_approval(error: &ApprovalError) -> LifecycleError {
    match error {
        ApprovalError::NotPermitted { .. }
        | ApprovalError::RoleNotHeld { .. }
        | ApprovalError::AuthorUnknown
        | ApprovalError::BadCommand { .. } => LifecycleError::NotPermitted,
        ApprovalError::AuthorConflict => LifecycleError::PlanChanged,
        _ => LifecycleError::Store,
    }
}

/// Parses the stored text back into a plan and checks it is the plan the hash names.
pub(crate) fn parse_stored(stored: &StoredPlan) -> Result<ReleasePlan, LifecycleError> {
    let plan: ReleasePlan =
        serde_json::from_str(&stored.json).map_err(|_| LifecycleError::PlanChanged)?;
    if PlanHash::from(&PlanDigest::of(&plan)) != stored.hash {
        return Err(LifecycleError::PlanChanged);
    }
    Ok(plan)
}

/// Loads a stored plan by the text of its hash.
pub(crate) fn load(
    ctx: &LifecycleContext,
    hash: &str,
) -> Result<(PlanHash, StoredPlan), LifecycleError> {
    let hash = PlanHash::parse(hash)?;
    let stored = ctx
        .plans()
        .load(&hash)?
        .ok_or(LifecycleError::UnknownPlan)?;
    Ok((hash, stored))
}

/// Moves a plan to `Approved` when the policy says it is ready, tolerating a lost race.
pub(crate) fn settle(
    ctx: &LifecycleContext,
    plan: &ReleasePlan,
    stored: &StoredPlan,
) -> Result<PlanState, LifecycleError> {
    let status = ctx
        .approvals()
        .status(plan, ctx.now(), &Checks::none())
        .map_err(|e| map_approval(&e))?;
    if status.ready && stored.state == PlanState::Proposed {
        return match ctx.plans().mark_state(
            &stored.hash,
            PlanState::Proposed,
            stored.version,
            PlanState::Approved,
        ) {
            Ok(_) => Ok(PlanState::Approved),
            Err(LifecycleError::PlanChanged) => Ok(ctx
                .plans()
                .load(&stored.hash)?
                .map_or(stored.state, |p| p.state)),
            Err(other) => Err(other),
        };
    }
    Ok(stored.state)
}

fn findings_of(report: &ExpandContractReport) -> Vec<Finding> {
    report
        .refusals()
        .iter()
        .map(|r| Finding {
            code: r.code.to_owned(),
            target: r.target.clone(),
            detail: r.detail.clone(),
        })
        .collect()
}

/// Plans, gates and stores.
///
/// # Errors
/// [`PlanRefused`] with `migration.not_permitted`, `migration.risk_requires_review` (an
/// expand/contract refusal or an ungated destructive plan, with the findings) or
/// `migration.store_failed`.
pub fn plan(ctx: &LifecycleContext, inputs: &PlanInputs) -> Result<PlanReport, PlanRefused> {
    ctx.require_permission()?;
    let release = inputs.release_plan();
    let digest = PlanDigest::of(&release);
    let hash = PlanHash::from(&digest);
    let risk = effective_risk(&release);

    match &inputs.expand_contract {
        Some(gate) => {
            let report = gate(&release);
            if !report.is_ok() {
                return Err(PlanRefused {
                    error: LifecycleError::RiskRequiresReview,
                    findings: findings_of(&report),
                });
            }
        }
        None if risk == spot_migration_plan::Risk::Destructive => {
            return Err(PlanRefused {
                error: LifecycleError::RiskRequiresReview,
                findings: vec![Finding {
                    code: "expand_contract.not_configured".to_owned(),
                    target: hash.as_str().to_owned(),
                    detail:
                        "the plan is destructive and no usage manifests are configured to prove \
                             the code stopped using what it removes"
                            .to_owned(),
                }],
            });
        }
        None => {}
    }

    let (conflicts, findings) = match &inputs.conflicts {
        Some(detect) => {
            let report = detect(&release);
            let findings = report
                .items
                .iter()
                .map(|item| Finding {
                    code: item.code.to_owned(),
                    target: item.cause.release_item.clone(),
                    detail: item.resolution.clone(),
                })
                .collect();
            (report.len(), findings)
        }
        None => (0, Vec::new()),
    };

    let author = subject(ctx.actor());
    let now = ctx.now();
    let (stored, created) = match ctx.plans().load(&hash)? {
        Some(existing) => (existing, false),
        None => {
            // The author is registered (and audited) before the plan can be approved by anyone.
            match ctx.approvals().register_plan_at(&release, &author, now) {
                Ok(()) | Err(ApprovalError::AuthorConflict) => {}
                Err(error) => return Err(map_approval(&error).into()),
            }
            let new = StoredPlan {
                hash: hash.clone(),
                json: release.json(),
                author: author.clone(),
                created_at: now,
                state: PlanState::Proposed,
                version: 1,
            };
            match ctx.plans().save(new.clone()) {
                Ok(()) => (new, true),
                // Another plan with the same hash won the race: converge on it.
                Err(LifecycleError::PlanChanged) => (
                    ctx.plans()
                        .load(&hash)?
                        .ok_or(LifecycleError::UnknownPlan)?,
                    false,
                ),
                Err(other) => return Err(other.into()),
            }
        }
    };
    let state = settle(ctx, &release, &stored)?;
    Ok(PlanReport {
        hash,
        state,
        risk: risk_name(risk).to_owned(),
        author: stored.author,
        created,
        changes: release.changes.len(),
        estimate_expected: release.estimate.expected,
        estimate_high: release.estimate.high,
        blocking: blocking_name(release.estimate.blocking),
        conflicts,
        findings,
    })
}

/// The approval status of a plan, flattened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalView {
    /// Distinct valid approvers needed.
    pub required: u32,
    /// Approvers whose approval counts.
    pub approved_by: Vec<String>,
    /// Approvers whose latest decision is a rejection.
    pub rejected_by: Vec<String>,
    /// Whether apply would be authorised now.
    pub ready: bool,
    /// The refusal code that would be returned otherwise.
    pub blocked_by: Option<String>,
}

impl From<Status> for ApprovalView {
    fn from(status: Status) -> Self {
        Self {
            required: status.required,
            approved_by: status.approved_by,
            rejected_by: status.rejected_by,
            ready: status.ready,
            blocked_by: status.blocked_by,
        }
    }
}

/// The result of `show`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShowReport {
    /// The plan hash.
    pub hash: PlanHash,
    /// The state (`superseded` when the apps changed after planning).
    pub state: PlanState,
    /// The risk class.
    pub risk: String,
    /// The author's subject.
    pub author: String,
    /// When it was planned.
    pub created_at: i64,
    /// The approval status.
    pub approvals: ApprovalView,
    /// The plan as text (changes, risk, estimate).
    pub text: String,
}

/// Shows a stored plan. With `current` inputs it also detects a superseded plan.
///
/// # Errors
/// `migration.not_permitted`, `migration.unknown_plan`, `migration.plan_changed` (the stored text
/// no longer matches its hash), `migration.store_failed`.
pub fn show(
    ctx: &LifecycleContext,
    hash: &str,
    current: Option<&PlanInputs>,
) -> Result<ShowReport, LifecycleError> {
    ctx.require_permission()?;
    let (hash, stored) = load(ctx, hash)?;
    let release = parse_stored(&stored)?;
    let now = ctx.now();
    ctx.approvals()
        .record_shown(&release, &subject(ctx.actor()), now)
        .map_err(|e| map_approval(&e))?;
    let status = ctx
        .approvals()
        .status(&release, now, &Checks::none())
        .map_err(|e| map_approval(&e))?;
    let mut state = stored.state;
    if let Some(inputs) = current {
        let live = PlanHash::from(&PlanDigest::of(&inputs.release_plan()));
        let open = matches!(
            state,
            PlanState::Proposed | PlanState::Approved | PlanState::Rehearsed
        );
        if live != hash && open {
            // Best effort: a lost race only means somebody else already moved it.
            let _ = ctx
                .plans()
                .mark_state(&hash, state, stored.version, PlanState::Superseded);
            state = PlanState::Superseded;
        }
    }
    Ok(ShowReport {
        hash,
        state,
        risk: risk_name(effective_risk(&release)).to_owned(),
        author: stored.author,
        created_at: stored.created_at,
        approvals: status.into(),
        text: release.render_text(),
    })
}

fn text_param(call: &Validated, name: &str) -> Option<String> {
    call.params
        .iter()
        .find(|p| p.name == name)
        .and_then(|p| match &p.value {
            spot_mgmt_ops::ParamValue::Text(text) => Some(text.clone()),
            _ => None,
        })
}

pub(crate) fn check_site(ctx: &LifecycleContext, call: &Validated) -> Result<(), LifecycleError> {
    match text_param(call, "site") {
        Some(site) if site == ctx.site().as_str() => Ok(()),
        _ => Err(LifecycleError::NotPermitted),
    }
}

pub(crate) fn param(call: &Validated, name: &str) -> Option<String> {
    text_param(call, name)
}

struct PlanHandler {
    ctx: Arc<LifecycleContext>,
    source: Arc<dyn PlanSource>,
}

impl OperationHandler for PlanHandler {
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        Box::pin(async move {
            check_site(&self.ctx, call)?;
            let inputs = self.source.inputs().await?;
            let ctx = Arc::clone(&self.ctx);
            // The stores reach PostgreSQL through the blocking bridge: never on a runtime thread.
            let report = off_runtime(move || plan(&ctx, &inputs))
                .await?
                .map_err(|r| r.error)?;
            let mut fields = BTreeMap::new();
            fields.insert("plan_hash".to_owned(), report.hash.as_str().to_owned());
            fields.insert("risk".to_owned(), report.risk.clone());
            fields.insert("state".to_owned(), report.state.as_str().to_owned());
            fields.insert("created".to_owned(), report.created.to_string());
            Ok(HandlerOutput {
                message: format!("plan {} ({})", report.hash.as_str(), report.risk),
                fields,
            })
        })
    }
}

struct ShowHandler {
    ctx: Arc<LifecycleContext>,
    source: Option<Arc<dyn PlanSource>>,
}

impl OperationHandler for ShowHandler {
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        Box::pin(async move {
            check_site(&self.ctx, call)?;
            let hash = param(call, "plan_hash").ok_or(LifecycleError::UnknownPlan)?;
            let current = match &self.source {
                Some(source) => Some(source.inputs().await?),
                None => None,
            };
            let ctx = Arc::clone(&self.ctx);
            let report = off_runtime(move || show(&ctx, &hash, current.as_ref())).await??;
            let mut fields = BTreeMap::new();
            fields.insert("plan_hash".to_owned(), report.hash.as_str().to_owned());
            fields.insert("state".to_owned(), report.state.as_str().to_owned());
            fields.insert("risk".to_owned(), report.risk.clone());
            fields.insert("author".to_owned(), report.author.clone());
            fields.insert(
                "approved_by".to_owned(),
                report.approvals.approved_by.join(","),
            );
            fields.insert("required".to_owned(), report.approvals.required.to_string());
            Ok(HandlerOutput {
                message: report.text,
                fields,
            })
        })
    }
}

/// Registers `site.migrate.show` without the superseded check (the host has no plan source).
/// `site.migrate.plan` stays a fail-closed stub until [`register_with`] gives it a source.
pub fn register(ctx: &Arc<LifecycleContext>, registry: &mut HandlerRegistry) {
    if let Some(id) = OpId::new("site.migrate.show") {
        registry.register(
            id,
            Arc::new(ShowHandler {
                ctx: Arc::clone(ctx),
                source: None,
            }),
        );
    }
}

/// Registers `site.migrate.plan` and `site.migrate.show` with `source`, which also lets `show`
/// notice a superseded plan.
pub fn register_with(
    ctx: &Arc<LifecycleContext>,
    registry: &mut HandlerRegistry,
    source: Arc<dyn PlanSource>,
) {
    if let Some(id) = OpId::new("site.migrate.plan") {
        registry.register(
            id,
            Arc::new(PlanHandler {
                ctx: Arc::clone(ctx),
                source: Arc::clone(&source),
            }),
        );
    }
    if let Some(id) = OpId::new("site.migrate.show") {
        registry.register(
            id,
            Arc::new(ShowHandler {
                ctx: Arc::clone(ctx),
                source: Some(source),
            }),
        );
    }
}

/// The plan store a context carries, for callers that only need to list.
#[must_use]
pub fn store(ctx: &LifecycleContext) -> &Arc<dyn PlanStore> {
    ctx.plans()
}
