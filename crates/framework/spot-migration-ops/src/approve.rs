//! Owned by WP-MIG-04.T7 (approve and reject).
//!
//! `approve` and `reject` record a decision on a stored plan, bound to its hash.
//!
//! # Rules
//!
//! * The actor needs the `migration.run` permission and must be a person: the system does not
//!   approve (`migration.not_permitted`).
//! * **Separation of duties:** the approver (and the rejecter) is not the plan's author
//!   (`migration.self_approval`). An unknown author fails closed (`migration.not_permitted`), as
//!   the approval service does.
//! * The **role** the decision is recorded under is the one the site policy requires for the
//!   plan's risk class (`release_manager` by default), and the person must hold it; with no
//!   required role it is the first role the person holds.
//! * The policy decides when a plan is approved: `additive` may auto-approve, `needs_review`
//!   needs one approver, `destructive` needs two. When enough distinct approvers have approved,
//!   the plan moves from `proposed` to `approved` (a compare-and-set; a lost race is harmless).
//! * A rejection moves a plan that is not yet being applied to `rejected`.
//! * Every decision is audited by the approval service **before** it is stored: a decision that
//!   cannot be audited is not taken.
//!
//! The hash binds everything: the stored text is parsed back and its digest recomputed on every
//! call, so a plan changed in the store (`migration.plan_changed`) can be neither approved nor
//! rejected.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_mgmt_ops::{OpId, Validated};
use spot_migration_apply::BoxFuture;
use spot_migration_approval::{Act, Checks, Identity, effective_risk};

use crate::catalogue::{HandlerOutput, HandlerRegistry, OperationHandler};
use crate::context::{LifecycleContext, PlanState, off_runtime};
use crate::error::LifecycleError;
use crate::plan::{
    ApprovalView, check_site, load, map_approval, param, parse_stored, settle, subject,
};

/// The result of `approve` or `reject`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionReport {
    /// The plan hash.
    pub hash: String,
    /// The state after the decision.
    pub state: PlanState,
    /// The approval status after the decision.
    pub approvals: ApprovalView,
}

fn identity(ctx: &LifecycleContext) -> Result<Identity, LifecycleError> {
    let principal = ctx.actor().principal();
    if principal.is_system || principal.user.trim().is_empty() {
        return Err(LifecycleError::NotPermitted);
    }
    let roles = principal
        .assignments
        .iter()
        .flat_map(|assignment| assignment.roles.iter().map(|role| role.0.clone()))
        .collect();
    Ok(Identity {
        subject: principal.user.clone(),
        roles,
    })
}

fn decide(
    ctx: &LifecycleContext,
    hash: &str,
    comment: &str,
    approve: bool,
) -> Result<DecisionReport, LifecycleError> {
    ctx.require_permission()?;
    let identity = identity(ctx)?;
    let (hash, stored) = load(ctx, hash)?;
    let release = parse_stored(&stored)?;
    // A plan that is already decided or in flight takes no further decision.
    let open = if approve {
        matches!(stored.state, PlanState::Proposed | PlanState::Approved)
    } else {
        matches!(
            stored.state,
            PlanState::Proposed | PlanState::Approved | PlanState::Rehearsed
        )
    };
    if !open {
        return Err(LifecycleError::PlanChanged);
    }
    if stored.author == subject(ctx.actor()) {
        return Err(LifecycleError::SelfApproval);
    }
    let required = ctx
        .approvals()
        .policy()
        .requirement(effective_risk(&release))
        .role;
    let role = match required {
        Some(role) => role,
        None => identity
            .roles
            .first()
            .cloned()
            .ok_or(LifecycleError::NotPermitted)?,
    };
    let act = Act {
        identity,
        role: &role,
        comment,
        signature: None,
        now: ctx.now(),
    };
    let checks = Checks::none();
    if approve {
        ctx.approvals()
            .approve(&release, &act, &checks)
            .map_err(|e| map_approval(&e))?;
    } else {
        ctx.approvals()
            .reject(&release, &act, &checks)
            .map_err(|e| map_approval(&e))?;
    }
    let state = if approve {
        settle(ctx, &release, &stored)?
    } else {
        match ctx
            .plans()
            .mark_state(&hash, stored.state, stored.version, PlanState::Rejected)
        {
            Ok(_) => PlanState::Rejected,
            // Somebody moved it first: report where it is.
            Err(LifecycleError::PlanChanged) => {
                ctx.plans().load(&hash)?.map_or(stored.state, |p| p.state)
            }
            Err(other) => return Err(other),
        }
    };
    let status = ctx
        .approvals()
        .status(&release, ctx.now(), &checks)
        .map_err(|e| map_approval(&e))?;
    Ok(DecisionReport {
        hash: hash.as_str().to_owned(),
        state,
        approvals: status.into(),
    })
}

/// Approves the plan `hash`.
///
/// # Errors
/// `migration.not_permitted`, `migration.unknown_plan`, `migration.plan_changed` (changed text, or
/// a state that takes no approval), `migration.self_approval`, `migration.store_failed`.
pub fn approve(
    ctx: &LifecycleContext,
    hash: &str,
    comment: &str,
) -> Result<DecisionReport, LifecycleError> {
    decide(ctx, hash, comment, true)
}

/// Rejects the plan `hash`.
///
/// # Errors
/// As [`approve`].
pub fn reject(
    ctx: &LifecycleContext,
    hash: &str,
    comment: &str,
) -> Result<DecisionReport, LifecycleError> {
    decide(ctx, hash, comment, false)
}

struct DecisionHandler {
    ctx: Arc<LifecycleContext>,
    approve: bool,
}

impl OperationHandler for DecisionHandler {
    fn handle<'a>(
        &'a self,
        call: &'a Validated,
    ) -> BoxFuture<'a, Result<HandlerOutput, LifecycleError>> {
        Box::pin(async move {
            check_site(&self.ctx, call)?;
            let hash = param(call, "plan_hash").ok_or(LifecycleError::UnknownPlan)?;
            let comment = param(call, "reason").unwrap_or_default();
            let (ctx, approve) = (Arc::clone(&self.ctx), self.approve);
            let report = off_runtime(move || decide(&ctx, &hash, &comment, approve)).await??;
            let mut fields = BTreeMap::new();
            fields.insert("plan_hash".to_owned(), report.hash.clone());
            fields.insert("state".to_owned(), report.state.as_str().to_owned());
            fields.insert(
                "approved_by".to_owned(),
                report.approvals.approved_by.join(","),
            );
            fields.insert("required".to_owned(), report.approvals.required.to_string());
            let verb = if self.approve { "approved" } else { "rejected" };
            Ok(HandlerOutput {
                message: format!(
                    "plan {} {verb}; state {}",
                    report.hash,
                    report.state.as_str()
                ),
                fields,
            })
        })
    }
}

/// Registers the handlers of `site.migrate.approve` and `site.migrate.reject`.
pub fn register(ctx: &Arc<LifecycleContext>, registry: &mut HandlerRegistry) {
    for (name, approve) in [
        ("site.migrate.approve", true),
        ("site.migrate.reject", false),
    ] {
        if let Some(id) = OpId::new(name) {
            registry.register(
                id,
                Arc::new(DecisionHandler {
                    ctx: Arc::clone(ctx),
                    approve,
                }),
            );
        }
    }
}
