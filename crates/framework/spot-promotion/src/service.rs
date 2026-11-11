//! The promotion pipeline: request → plan → approve → rehearse → apply → verify.
//!
//! Every step is one method, and each refuses unless the promotion is in the state the lifecycle
//! says it comes from. Nothing writes to the target before the plan is approved and, where the
//! policy requires it, a rehearsal passed. A rehearsal failure or a verification failure moves
//! the promotion to [`Failed`](crate::PromotionState::Failed) with its evidence and leaves the
//! target untouched (rehearsal) or restored (verification).
//!
//! This is the migration lifecycle (ADR-22), specialised to a customisation bundle: the plan is
//! a `ReleasePlan`, approval is `spot-migration-approval`'s ticket, the rehearsal returns
//! `spot-rehearsal`'s report and verdict, and each transition is one event in the migration audit
//! store.

use std::sync::Arc;

use spot_audit_chain::AuditRecord;
use spot_custom_bundle::Bundle;
use spot_environments::EnvRef;
use spot_migration_approval::{
    Act, Approval, ApprovalError, ApprovalService, ApprovalStore, AuditSink, Checks, Decision,
    Refusal, SignatureHook, authorise_apply, effective_risk,
};
use spot_migration_audit::{
    ApplyEvent, ApprovalDecision, ApprovalEvent, ArtifactKind, ArtifactRef, FinishEvent, Outcome,
    PlanRef, PlanSummary, RehearsalEvent, RunId,
};

use crate::codes;
use crate::error::PromotionError;
use crate::plan::{PlanIssue, PromotionPlan};
use crate::policy::{PromotionPolicy, StoredAuthor};
use crate::ports::{
    BundleApplier, FailureEvidence, MigrationAudit, OpenRun, PromotionAudit, RehearsalEvidence,
    RehearsalRequest, RehearsalRunner, TargetSite, audit_verdict,
};
use crate::state::{Promotion, PromotionState};
use crate::store::PromotionStore;

/// The approval service's audit sink, shared by the per-environment services built from it.
struct SharedSink(Arc<dyn AuditSink>);

impl AuditSink for SharedSink {
    fn record(&self, record: AuditRecord) -> Result<(), ApprovalError> {
        self.0.record(record)
    }
}

/// An approval-service error as the promotion's own: a backend failure is a store failure, an
/// unwritable audit record an audit failure, and every refusal an
/// [`PromotionError::ApprovalRefused`] carrying the approval crate's code.
fn approval_error(error: ApprovalError) -> PromotionError {
    match error {
        ApprovalError::Backend { message } => PromotionError::StoreFailed {
            message: format!("the approval store: {message}"),
        },
        ApprovalError::AuditUnavailable { message } => PromotionError::AuditFailed {
            message: format!("the approval audit: {message}"),
        },
        other => PromotionError::ApprovalRefused {
            reason: other.to_string(),
        },
    }
}

/// The approval service of one environment: the policy differs per role, the store and the audit
/// sink are the pipeline's own.
type EnvApprovals = ApprovalService<Arc<dyn ApprovalStore>, SharedSink>;

/// The promotion pipeline.
///
/// Approvals are never written to the approval store directly: every approval and rejection goes
/// through [`ApprovalService`], which checks the verified identity, the role and the plan's
/// recorded author before it stores anything.
pub struct PromotionService {
    store: Arc<dyn PromotionStore>,
    approvals: Arc<dyn ApprovalStore>,
    approval_audit: Arc<dyn AuditSink>,
    signature: Option<Arc<dyn SignatureHook + Send + Sync>>,
    audit: Arc<dyn PromotionAudit>,
    applier: Arc<dyn BundleApplier>,
    policy: PromotionPolicy,
}

impl PromotionService {
    /// Refuses a target site that is not the promotion's target environment. The comparison is
    /// exact (set name, role and tenant), so any difference, including case or whitespace in the
    /// set name, fails closed. It runs before the site is read or written.
    fn check_environment(
        promotion: &Promotion,
        target: &dyn TargetSite,
    ) -> Result<(), PromotionError> {
        let expected = promotion.to_env();
        let found = target.environment();
        if expected == &found {
            return Ok(());
        }
        let describe = |env: &EnvRef| format!("{}:{}:{}", env.set, env.role.name(), env.tenant);
        Err(PromotionError::EnvironmentMismatch {
            expected: describe(expected),
            found: describe(&found),
        })
    }
    /// A service over a store, an approval store, the approval service's audit sink, an audit
    /// port, a bundle applier and a policy.
    ///
    /// The pipeline builds the [`ApprovalService`] itself, once per target role, over `approvals`
    /// and `approval_audit`: the approval policy differs per environment (a service holds one
    /// policy), and building it here keeps one store and one sink behind every approval.
    ///
    /// # Errors
    /// [`PromotionError::PolicyInvalid`] when a role's approval policy is not usable.
    pub fn new(
        store: Arc<dyn PromotionStore>,
        approvals: Arc<dyn ApprovalStore>,
        approval_audit: Arc<dyn AuditSink>,
        audit: Arc<dyn PromotionAudit>,
        applier: Arc<dyn BundleApplier>,
        policy: PromotionPolicy,
    ) -> Result<Self, PromotionError> {
        policy.validate()?;
        Ok(Self {
            store,
            approvals,
            approval_audit,
            signature: None,
            audit,
            applier,
            policy,
        })
    }

    /// A service over the migration audit chain.
    ///
    /// # Errors
    /// [`PromotionError::PolicyInvalid`] as [`PromotionService::new`].
    pub fn with_migration_audit(
        store: Arc<dyn PromotionStore>,
        approvals: Arc<dyn ApprovalStore>,
        approval_audit: Arc<dyn AuditSink>,
        audit: Arc<MigrationAudit>,
        applier: Arc<dyn BundleApplier>,
        policy: PromotionPolicy,
    ) -> Result<Self, PromotionError> {
        Self::new(store, approvals, approval_audit, audit, applier, policy)
    }

    /// Wires the hardware-signature check. Without it, a policy that requires a signature
    /// refuses every approval (fail closed).
    #[must_use]
    pub fn with_signature_hook(mut self, hook: Arc<dyn SignatureHook + Send + Sync>) -> Self {
        self.signature = Some(hook);
        self
    }

    /// The approval service for a promotion to `role`.
    fn approval_service(
        &self,
        role: spot_environments::Role,
    ) -> Result<EnvApprovals, PromotionError> {
        ApprovalService::new(
            Arc::clone(&self.approvals),
            SharedSink(Arc::clone(&self.approval_audit)),
            self.policy.approval_policy(role),
        )
        .map_err(|error| PromotionError::PolicyInvalid {
            message: format!("{}: {error}", role.name()),
        })
    }

    /// The policy this service enforces.
    #[must_use]
    pub fn policy(&self) -> &PromotionPolicy {
        &self.policy
    }

    /// Requests a promotion. Nothing is planned and nothing is written.
    ///
    /// # Errors
    /// [`PromotionError::BadId`], [`PromotionError::Duplicate`],
    /// [`PromotionError::BadRoute`] and [`PromotionError::StoreFailed`].
    pub fn request(
        &self,
        id: &str,
        bundle: &Bundle,
        from_env: EnvRef,
        to_env: EnvRef,
        requester: &str,
        at: i64,
    ) -> Result<Promotion, PromotionError> {
        let id = RunId::new(id).map_err(|_| PromotionError::BadId {
            message: id.to_owned(),
        })?;
        if self.store.load(&id)?.is_some() {
            return Err(PromotionError::Duplicate { id: id.to_string() });
        }
        let promotion = Promotion::request(id, bundle, from_env, to_env, requester.to_owned(), at)?;
        self.store.insert(promotion.clone())?;
        Ok(promotion)
    }

    /// Plans the bundle against the target and, when the policy needs no approval, approves it.
    ///
    /// Nothing on the target is written: `spot-custom-bundle` validates and diffs only.
    ///
    /// # Errors
    /// [`PromotionError::BadState`], [`PromotionError::BundleMismatch`],
    /// [`PromotionError::Blocked`] when a conflict blocks, [`PromotionError::PlanFailed`],
    /// [`PromotionError::NothingToPromote`] and [`PromotionError::AuditFailed`].
    pub async fn plan(
        &self,
        id: &RunId,
        bundle: &Bundle,
        target: &dyn TargetSite,
        issues: Vec<PlanIssue>,
        actor: &str,
        at: i64,
    ) -> Result<Promotion, PromotionError> {
        let mut promotion = self.load(id)?;
        self.require(&promotion, &[PromotionState::Requested])?;
        self.check_bundle(&promotion, bundle)?;

        Self::check_environment(&promotion, target)?;

        let blocking: Vec<&PlanIssue> = issues.iter().filter(|issue| issue.blocking).collect();
        if let Some(first) = blocking.first() {
            return Err(PromotionError::Blocked {
                count: blocking.len(),
                first: format!(
                    "{} {}: {}",
                    first.source.as_str(),
                    first.item,
                    first.message
                ),
            });
        }

        let apply = self.applier.plan(bundle, target)?;
        let binding = format!(
            "{}|{}|{}|{}|{}|{}",
            promotion.id(),
            promotion.from_env().set,
            promotion.from_env().tenant,
            promotion.to_env().set,
            promotion.to_env().tenant,
            promotion.to_env().role.name()
        );
        let plan = PromotionPlan::assemble(bundle, &apply, issues, &binding)?;
        // The requester is the plan's author, registered (and audited) before the plan is stored:
        // an author that cannot be registered fails the step closed, and an approval of a plan
        // with no recorded author is refused by the approval service.
        self.approval_service(promotion.to_env().role)?
            .register_plan_at(plan.release(), promotion.requester(), at)
            .map_err(approval_error)?;
        let changes = plan.apply_changes();
        let env_name = promotion.to_env().role.name().to_owned();
        promotion.set_plan(plan.clone());
        promotion.advance(
            PromotionState::Planned,
            at,
            actor,
            format!("planned {changes} change(s) against {env_name}"),
        )?;
        self.store.save(promotion.clone())?;

        let run = promotion.run_id()?;
        self.audit
            .open(OpenRun {
                promotion: promotion.id(),
                run,
                plan_digest: plan.release().plan_digest.as_str(),
                plan: plan_ref(&plan, promotion.id()),
                actor,
                at,
            })
            .await?;

        let role = promotion.to_env().role;
        let requirement = self
            .policy
            .requirement(role, effective_risk(plan.release()));
        if requirement.approvers == 0 {
            promotion.advance(
                PromotionState::Approved,
                at,
                actor,
                format!("auto-approved by the {} policy", role.name()),
            )?;
            self.audit
                .approval(
                    promotion.id(),
                    ApprovalEvent {
                        approver: "system".to_owned(),
                        role: "environment_policy".to_owned(),
                        decision: ApprovalDecision::Approved,
                        at,
                    },
                )
                .await?;
            self.store.save(promotion.clone())?;
        }
        Ok(promotion)
    }

    /// Records one approver's approval and advances to `Approved` when the policy is satisfied.
    ///
    /// The approval goes through [`ApprovalService::approve`]: the approver is the verified
    /// identity's subject, the role must be one the identity holds (and the one the policy
    /// asks for), and the plan's recorded author never counts. Nothing is appended to the approval
    /// store here.
    ///
    /// # Errors
    /// [`PromotionError::BadState`], [`PromotionError::PlanChanged`],
    /// [`PromotionError::ApprovalRefused`] when the approval service refuses (nothing is
    /// recorded), [`PromotionError::StoreFailed`] and [`PromotionError::AuditFailed`].
    pub async fn approve(&self, id: &RunId, act: &Act<'_>) -> Result<Promotion, PromotionError> {
        self.decide(id, act, Decision::Approve).await
    }

    /// Records one approver's rejection, which moves the promotion to `Rejected`.
    ///
    /// It goes through [`ApprovalService::reject`], so it needs the same verified identity and
    /// role as an approval.
    ///
    /// # Errors
    /// As [`PromotionService::approve`].
    pub async fn reject(&self, id: &RunId, act: &Act<'_>) -> Result<Promotion, PromotionError> {
        self.decide(id, act, Decision::Reject).await
    }

    /// One approver's decision, through the approval service.
    async fn decide(
        &self,
        id: &RunId,
        act: &Act<'_>,
        decision: Decision,
    ) -> Result<Promotion, PromotionError> {
        let mut promotion = self.load(id)?;
        self.require(&promotion, &[PromotionState::Planned])?;
        let plan = promotion
            .plan()
            .ok_or_else(|| PromotionError::BadState {
                id: id.to_string(),
                state: promotion.state().to_string(),
                required: "a planned promotion".to_owned(),
            })?
            .clone();
        plan.check_unchanged()?;

        let role = promotion.to_env().role;
        let service = self.approval_service(role)?;
        let author = StoredAuthor {
            store: self.approvals.as_ref(),
        };
        let checks = Checks {
            authorship: &author,
            signature: self
                .signature
                .as_deref()
                .map(|hook| hook as &dyn SignatureHook),
        };
        let approval: Approval = match decision {
            Decision::Approve => service.approve(plan.release(), act, &checks),
            Decision::Reject => service.reject(plan.release(), act, &checks),
        }
        .map_err(approval_error)?;
        self.audit
            .approval(
                promotion.id(),
                ApprovalEvent {
                    approver: approval.approver.clone(),
                    role: approval.role.clone(),
                    decision: match approval.decision {
                        Decision::Approve => ApprovalDecision::Approved,
                        Decision::Reject => ApprovalDecision::Rejected,
                    },
                    at: approval.at,
                },
            )
            .await?;

        let all = self
            .approvals
            .all()
            .map_err(|error| PromotionError::StoreFailed {
                message: format!("the approval store: {error}"),
            })?;
        let evaluation = spot_migration_approval::evaluate(
            plan.release(),
            &all,
            &self.policy.approval_policy(role),
            approval.at,
            &checks,
        );
        if !evaluation.rejected_by.is_empty() {
            promotion.advance(
                PromotionState::Rejected,
                approval.at,
                &approval.approver,
                format!("rejected by {}", evaluation.rejected_by.join(", ")),
            )?;
            self.store.save(promotion.clone())?;
            self.finish(promotion.id(), Outcome::Refused, approval.at)
                .await?;
            return Ok(promotion);
        }
        if evaluation.refusal().is_none() {
            promotion.advance(
                PromotionState::Approved,
                approval.at,
                &approval.approver,
                format!(
                    "approved by {}",
                    if evaluation.valid.is_empty() {
                        "the environment policy".to_owned()
                    } else {
                        evaluation.valid.join(", ")
                    }
                ),
            )?;
            self.store.save(promotion.clone())?;
        }
        Ok(promotion)
    }

    /// Rehearses the approved plan on a clone of the target.
    ///
    /// Any verdict other than clean or expected-changes moves the promotion to
    /// [`PromotionState::Failed`] and blocks apply: the target is never written.
    ///
    /// # Errors
    /// [`PromotionError::BadState`], [`PromotionError::BundleMismatch`],
    /// [`PromotionError::PlanChanged`], [`PromotionError::RehearsalFailed`],
    /// [`PromotionError::AuditFailed`] and the runner's own failure.
    pub async fn rehearse(
        &self,
        id: &RunId,
        bundle: &Bundle,
        target: &dyn TargetSite,
        runner: &dyn RehearsalRunner,
        actor: &str,
        at: i64,
    ) -> Result<Promotion, PromotionError> {
        let mut promotion = self.load(id)?;
        self.require(&promotion, &[PromotionState::Approved])?;
        let plan = promotion.check_plan(bundle)?.clone();
        Self::check_environment(&promotion, target)?;
        let ticket = self.authorise(&promotion, at)?;
        let report = runner.rehearse(&RehearsalRequest {
            plan: &plan,
            target,
            ticket: &ticket,
        })?;
        let evidence = RehearsalEvidence { report };
        let findings = evidence.findings();
        let verdict = evidence.verdict();
        let json = evidence.to_json();
        self.audit
            .rehearsal(
                promotion.id(),
                RehearsalEvent {
                    report: ArtifactRef::of(
                        ArtifactKind::RehearsalReport,
                        format!("promotions/{id}/rehearsal.json"),
                        json.as_bytes(),
                    ),
                    verdict: audit_verdict(evidence.report().outcome),
                    findings,
                    at,
                },
            )
            .await?;
        promotion.set_rehearsal(evidence);
        if promotion
            .rehearsal()
            .is_some_and(RehearsalEvidence::admits_apply)
        {
            promotion.advance(
                PromotionState::Rehearsed,
                at,
                actor,
                format!("rehearsal {verdict} with {findings} finding(s)"),
            )?;
            self.store.save(promotion.clone())?;
            return Ok(promotion);
        }
        self.fail(
            &mut promotion,
            FailureEvidence {
                stage: "rehearsal".to_owned(),
                code: codes::REHEARSAL_FAILED.to_owned(),
                message: format!("the rehearsal reported {verdict}"),
                findings: Vec::new(),
            },
            at,
            actor,
        )
        .await?;
        Err(PromotionError::RehearsalFailed {
            verdict: verdict.to_owned(),
            findings,
        })
    }

    /// Writes the approved bundle to the target and journals the apply.
    ///
    /// Apply runs the plan the approval covers: the stored plan is re-hashed, the bundle is
    /// re-planned against the target and its hash compared, and the approval ticket is
    /// re-issued, so a plan changed after approval is refused before anything is written.
    ///
    /// # Errors
    /// [`PromotionError::BadState`], [`PromotionError::RehearsalRequired`],
    /// [`PromotionError::PlanChanged`], [`PromotionError::ApprovalRequired`],
    /// [`PromotionError::ApplyFailed`] and [`PromotionError::AuditFailed`].
    pub async fn apply(
        &self,
        id: &RunId,
        bundle: &Bundle,
        target: &mut dyn TargetSite,
        actor: &str,
        at: i64,
    ) -> Result<Promotion, PromotionError> {
        let mut promotion = self.load(id)?;
        self.require(
            &promotion,
            &[PromotionState::Approved, PromotionState::Rehearsed],
        )?;
        let role = promotion.to_env().role;
        if self.policy.requires_rehearsal(role) && promotion.state() != PromotionState::Rehearsed {
            return Err(PromotionError::RehearsalRequired {
                environment: role.name().to_owned(),
            });
        }
        let plan = promotion.check_plan(bundle)?.clone();

        Self::check_environment(&promotion, target)?;

        let current = self.applier.plan(bundle, target)?;
        let already_written = current.is_empty();
        if !already_written && current.hash() != plan.apply_hash() {
            return Err(PromotionError::PlanChanged {
                expected: plan.apply_hash().to_owned(),
                found: current.hash().to_owned(),
            });
        }
        let ticket = self.authorise(&promotion, at)?;
        if !ticket.matches(plan.release()) {
            return Err(PromotionError::PlanChanged {
                expected: plan.digest().as_hex().to_owned(),
                found: spot_migration_approval::PlanDigest::of(plan.release())
                    .as_hex()
                    .to_owned(),
            });
        }

        let evidence = if already_written {
            // A previous execution wrote the bundle but died before journaling it. The target is
            // already at the approved state, so this is a resume, not a second apply.
            crate::ports::ApplyEvidence {
                bundle_digest: bundle.digest().to_hex(),
                plan_hash: plan.apply_hash().to_owned(),
                steps: 0,
                warnings: Vec::new(),
            }
        } else {
            let pre_image = self.applier.capture(target)?;
            promotion.set_pre_image(pre_image);
            self.store.save(promotion.clone())?;
            match self.applier.apply(&current, target) {
                Ok(evidence) => evidence,
                Err(error) => {
                    let message = error.to_string();
                    self.fail(
                        &mut promotion,
                        FailureEvidence {
                            stage: "apply".to_owned(),
                            code: codes::APPLY_FAILED.to_owned(),
                            message,
                            findings: Vec::new(),
                        },
                        at,
                        actor,
                    )
                    .await?;
                    return Err(error);
                }
            }
        };

        promotion.set_apply(evidence.clone());
        promotion.advance(
            PromotionState::Applied,
            at,
            actor,
            format!("applied {} change(s)", evidence.steps),
        )?;
        self.store.save(promotion.clone())?;
        let json = serde_json::to_vec(&evidence).unwrap_or_default();
        self.audit
            .applied(
                promotion.id(),
                ApplyEvent {
                    journal: ArtifactRef::of(
                        ArtifactKind::ApplyJournal,
                        format!("promotions/{id}/apply.json"),
                        &json,
                    ),
                    steps: evidence.steps,
                    at,
                },
            )
            .await?;
        Ok(promotion)
    }

    /// Verifies that the target matches the approved plan.
    ///
    /// On a mismatch the captured pre-image is applied back, so the target returns to exactly
    /// its pre-promotion customisations, and the promotion is marked failed with the evidence.
    ///
    /// # Errors
    /// [`PromotionError::BadState`], [`PromotionError::VerifyFailed`],
    /// [`PromotionError::RestoreFailed`] and [`PromotionError::AuditFailed`].
    pub async fn verify(
        &self,
        id: &RunId,
        bundle: &Bundle,
        target: &mut dyn TargetSite,
        actor: &str,
        at: i64,
    ) -> Result<Promotion, PromotionError> {
        let mut promotion = self.load(id)?;
        self.require(&promotion, &[PromotionState::Applied])?;
        let plan = promotion.check_plan(bundle)?.clone();

        Self::check_environment(&promotion, target)?;

        let evidence = self.applier.verify(plan.apply_plan(), bundle, target)?;
        if evidence.passed {
            promotion.advance(
                PromotionState::Verified,
                at,
                actor,
                format!("verified {} change(s)", plan.apply_changes()),
            )?;
            self.store.save(promotion.clone())?;
            self.finish(promotion.id(), Outcome::Verified, at).await?;
            return Ok(promotion);
        }

        let findings = evidence.findings.clone();
        let restore = match promotion.pre_image() {
            Some(pre_image) => self.applier.restore(pre_image, target),
            None => Err(PromotionError::RestoreFailed {
                message: "the promotion holds no pre-image to restore".to_owned(),
            }),
        };
        let message = match restore {
            Ok(_) => format!(
                "verification found {} difference(s); the target was restored",
                findings.len()
            ),
            Err(error) => format!(
                "verification found {} difference(s) and the restore failed: {error}",
                findings.len()
            ),
        };
        self.fail(
            &mut promotion,
            FailureEvidence {
                stage: "verify".to_owned(),
                code: codes::VERIFY_FAILED.to_owned(),
                message,
                findings: findings.clone(),
            },
            at,
            actor,
        )
        .await?;
        Err(PromotionError::VerifyFailed {
            findings: findings.len(),
        })
    }

    /// Applies and then verifies, the common case.
    ///
    /// # Errors
    /// As [`PromotionService::apply`] and [`PromotionService::verify`].
    pub async fn advance(
        &self,
        id: &RunId,
        bundle: &Bundle,
        target: &mut dyn TargetSite,
        actor: &str,
        at: i64,
    ) -> Result<Promotion, PromotionError> {
        self.apply(id, bundle, target, actor, at).await?;
        self.verify(id, bundle, target, actor, at).await
    }

    /// Reattaches a promotion after a process restart and opens a new audit run for it.
    ///
    /// The promotion keeps its state, journal and pre-image; the caller continues with
    /// [`apply`](Self::apply) (from `Rehearsed`) or [`verify`](Self::verify) (from `Applied`).
    /// The new run id is derived from the promotion id, so the migration audit store keeps both
    /// executions without a duplicate run.
    ///
    /// # Errors
    /// [`PromotionError::NotFound`], [`PromotionError::AuditFailed`] and
    /// [`PromotionError::StoreFailed`].
    pub async fn resume(
        &self,
        id: &RunId,
        actor: &str,
        at: i64,
    ) -> Result<Promotion, PromotionError> {
        let mut promotion = self.load(id)?;
        if promotion.state().is_terminal() {
            return Ok(promotion);
        }
        let run = promotion.next_run_id()?;
        self.store.save(promotion.clone())?;
        if let Some(plan) = promotion.plan().cloned() {
            self.audit
                .open(OpenRun {
                    promotion: promotion.id(),
                    run,
                    plan_digest: plan.release().plan_digest.as_str(),
                    plan: plan_ref(&plan, promotion.id()),
                    actor,
                    at,
                })
                .await?;
        }
        Ok(promotion)
    }

    /// Marks the promotion failed, journals the evidence and closes the audit run.
    async fn fail(
        &self,
        promotion: &mut Promotion,
        evidence: FailureEvidence,
        at: i64,
        actor: &str,
    ) -> Result<(), PromotionError> {
        let message = evidence.message.clone();
        promotion.set_failure(evidence);
        promotion.advance(PromotionState::Failed, at, actor, message)?;
        self.store.save(promotion.clone())?;
        self.finish(promotion.id(), Outcome::Failed, at).await
    }

    /// Issues the approval ticket for the plan in hand, or refuses.
    fn authorise(
        &self,
        promotion: &Promotion,
        at: i64,
    ) -> Result<spot_migration_approval::ApplyTicket, PromotionError> {
        let plan = promotion.plan().ok_or_else(|| PromotionError::BadState {
            id: promotion.id().to_string(),
            state: promotion.state().to_string(),
            required: "a planned promotion".to_owned(),
        })?;
        let all = self
            .approvals
            .all()
            .map_err(|error| PromotionError::StoreFailed {
                message: format!("the approval store: {error}"),
            })?;
        let author = StoredAuthor {
            store: self.approvals.as_ref(),
        };
        authorise_apply(
            plan.release(),
            &all,
            &self.policy.approval_policy(promotion.to_env().role),
            at,
            &Checks {
                authorship: &author,
                signature: self
                    .signature
                    .as_deref()
                    .map(|hook| hook as &dyn SignatureHook),
            },
        )
        .map_err(|refusal: Refusal| PromotionError::ApprovalRequired {
            refusal: refusal.to_string(),
        })
    }

    /// Closes the current audit run with a terminal outcome.
    async fn finish(&self, id: &RunId, outcome: Outcome, at: i64) -> Result<(), PromotionError> {
        self.audit.finish(id, FinishEvent { outcome, at }).await
    }

    /// Loads a promotion or says it does not exist.
    fn load(&self, id: &RunId) -> Result<Promotion, PromotionError> {
        self.store
            .load(id)?
            .ok_or_else(|| PromotionError::NotFound { id: id.to_string() })
    }

    /// Checks the promotion is in one of the states the step starts from.
    fn require(
        &self,
        promotion: &Promotion,
        states: &[PromotionState],
    ) -> Result<(), PromotionError> {
        if states.contains(&promotion.state()) {
            return Ok(());
        }
        let required = states
            .iter()
            .map(|state| state.as_str())
            .collect::<Vec<_>>()
            .join(" or ");
        Err(PromotionError::BadState {
            id: promotion.id().to_string(),
            state: promotion.state().to_string(),
            required,
        })
    }

    /// Checks the bundle in hand is the bundle the promotion was requested for.
    fn check_bundle(&self, promotion: &Promotion, bundle: &Bundle) -> Result<(), PromotionError> {
        let found = bundle.digest();
        if found == promotion.bundle_digest() {
            return Ok(());
        }
        Err(PromotionError::BundleMismatch {
            id: promotion.id().to_string(),
            expected: promotion.bundle_digest().to_hex(),
            found: found.to_hex(),
        })
    }
}

/// The stored plan document and its summary, for the audit record.
fn plan_ref(plan: &PromotionPlan, id: &RunId) -> PlanRef {
    let json = plan.release().json();
    PlanRef {
        artifact: ArtifactRef::of(
            ArtifactKind::Plan,
            format!("promotions/{id}/plan.json"),
            json.as_bytes(),
        ),
        summary: PlanSummary::of(plan.release()),
    }
}
