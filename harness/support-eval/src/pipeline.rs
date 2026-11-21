//! The bridge from a remedy to the fix pipeline.
//!
//! Every evaluated change goes through `spot-fix-pipeline`: the remedy's sealed dry-run plan is
//! normalised to a [`FixPlan`], the policy chooses the path, the rehearsal runs on a clone, the
//! live apply captures pre-images or a snapshot, verification re-checks the incident's success
//! condition, and a failed verification rolls back and compares fingerprints for residue.
//!
//! The ports here are over the simulated tenant. They are deliberately strict: the pre-image
//! restore replaces the whole world with the captured clone, and the rehearsal applies the remedy
//! to a fresh clone through a fresh registry, so neither path can borrow the live fast path.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::Value as Json;
use spot_agent_tiers::{AgentIdentity, Approval, Domain, Policy, Tier};
use spot_fix_pipeline::{
    AgentRef, ApprovalPort, ChangeRecord, DeploymentEpoch, FixAuditEvent, FixContext, FixEngine,
    FixError, FixExecutor, FixId, FixOutcome, FixPlan, FixPolicy, FixPorts, FixStep, FixVerifier,
    InMemoryJournal, MemoryFixAudit, NoFaults, PreImage, PreImageStore, RehearsalEvidence,
    RehearsalPort, SnapshotPort, SnapshotRef, StepChange, VerifyReport,
};
use spot_rehearsal::Fingerprint;
use spot_remedies::{DocTypeRef, Gate, Plan, Registry, RemedyError, RemedyId, RemedySettings};

use crate::corpus::SuccessCheck;
use crate::error::{EvalError, Result};
use crate::tenant::Tenant;

/// The state one fix run shares: the tenant, its registry and the acting identity.
#[derive(Clone)]
pub(crate) struct Shared {
    /// The simulated tenant, shared by every port of one fix.
    pub tenant: Rc<RefCell<Tenant>>,
    /// The live remedy registry the apply runs through.
    pub registry: Rc<Registry>,
    /// The agent the fix acts as.
    pub agent: AgentIdentity,
    /// The tenant id the fix is scoped to.
    pub site: String,
    /// The caller's instant.
    pub now: i64,
}

impl Shared {
    /// A shared state for `tenant` and `agent`.
    pub(crate) fn new(tenant: Tenant, agent: AgentIdentity, now: i64) -> Result<Self> {
        let site = tenant.site.clone();
        let registry = build_registry()?;
        Ok(Self {
            tenant: Rc::new(RefCell::new(tenant)),
            registry: Rc::new(registry),
            agent,
            site,
            now,
        })
    }

    /// The registry the evaluator uses, with every seed remedy plus the document recompute
    /// remedy the hard-floor case needs.
    pub(crate) fn registry(&self) -> Rc<Registry> {
        Rc::clone(&self.registry)
    }
}

/// Builds the registry the evaluation runs: the seven seed remedies plus the document recompute
/// remedy (not a seed, but public in `spot-remedies` for hard-floor proofs).
pub(crate) fn build_registry() -> Result<Registry> {
    let mut remedies = spot_remedies::seeds::all();
    let affects = DocTypeRef::new("Financial Record").map_err(|error| EvalError::Simulation {
        detail: error.to_string(),
    })?;
    remedies.push(Box::new(spot_remedies::seeds::RecomputeField { affects }));
    Registry::new(remedies, Policy::default(), RemedySettings::default()).map_err(|error| {
        EvalError::Simulation {
            detail: format!("the remedy registry could not be built: {error}"),
        }
    })
}

/// The agent identity the scripted evaluation runs as: a support agent with a T2 ceiling and no
/// deep registration.
#[must_use]
pub(crate) fn support_agent(id: &str, tier: Tier) -> AgentIdentity {
    AgentIdentity::new(id, "spot-support", Domain::Support, tier)
}

/// What one remedy run produced.
#[derive(Clone, Debug)]
pub(crate) struct FixRun {
    /// The pipeline's final state, when a plan was reached.
    pub outcome: Option<FixOutcome>,
    /// The pipeline's events, in order.
    pub audit: Vec<FixAuditEvent>,
    /// How many changes the apply recorded.
    pub changes: usize,
    /// Whether the run stopped at an explicit refusal (a tier, catalogue or precondition check).
    pub refused: bool,
    /// A one-line reason when the run could not start or finish.
    pub error: Option<String>,
}

impl FixRun {
    /// A run that stopped before the pipeline, with `detail`.
    fn stopped(detail: &str, refused: bool) -> Self {
        Self {
            outcome: None,
            audit: Vec::new(),
            changes: 0,
            refused,
            error: Some(detail.to_owned()),
        }
    }

    /// Whether the run finished verified.
    #[must_use]
    pub fn verified(&self) -> bool {
        self.outcome
            .as_ref()
            .is_some_and(|outcome| outcome.state == spot_fix_pipeline::FixState::Verified)
    }

    /// Whether the rollback residue was empty, when a rollback ran.
    #[must_use]
    pub fn residue_empty(&self) -> Option<bool> {
        self.outcome
            .as_ref()
            .and_then(|outcome| outcome.residue.as_ref())
            .map(spot_fix_pipeline::ResidueEvidence::is_empty)
    }
}

/// Runs one remedy end to end through the fix pipeline.
///
/// # Errors
///
/// [`EvalError::Pipeline`] only for an infrastructure failure the pipeline reports as an error;
/// a policy refusal or a failed verification is an outcome, not an error.
pub(crate) fn run_remedy(
    shared: &Shared,
    remedy: &RemedyId,
    params: &Json,
    check: &SuccessCheck,
    seq: u64,
) -> Result<FixRun> {
    // The sealed dry-run: the only plan the registry will apply.
    let plan = match preview(shared, remedy, params)? {
        Ok(plan) => plan,
        Err(error) => {
            return Ok(FixRun::stopped(&error.sentence(), is_refusal(&error)));
        }
    };
    let Some(meta) = shared.registry.meta(remedy) else {
        return Ok(FixRun::stopped(
            &format!("remedy `{remedy}` is not registered"),
            false,
        ));
    };
    let fix_plan = match FixPlan::from_remedy(&meta, &plan) {
        Ok(plan) => plan,
        Err(error) => {
            return Err(EvalError::Pipeline {
                detail: format!("the remedy plan could not be normalised: {error}"),
            });
        }
    };
    {
        let mut tenant = shared.tenant.borrow_mut();
        for target in &fix_plan.affected {
            tenant.track(target);
        }
    }

    let fix = FixId::derive(fix_plan.hash.as_str(), seq);
    let journal = InMemoryJournal::new();
    let mut audit = MemoryFixAudit::new();
    let context = FixContext::new(
        &shared.site,
        AgentRef::from_identity(&shared.agent),
        DeploymentEpoch::new(1),
        shared.now,
    );

    let mut pre_images = RemedyPreImages::new(shared.clone());
    let mut snapshots = RemedySnapshots::new(shared.clone());
    let mut executor = RemedyExecutor::new(shared.clone(), remedy.clone(), plan.clone());
    let mut verifier = RemedyVerifier::new(shared.clone(), check.clone());
    let mut rehearsal = RemedyRehearsal::new(
        shared.clone(),
        remedy.clone(),
        params.clone(),
        check.clone(),
    );
    let mut approval = EvalApproval { now: shared.now };

    let run = {
        let ports = FixPorts {
            pre_images: &mut pre_images,
            snapshots: &mut snapshots,
            executor: &mut executor,
            verifier: &mut verifier,
            rehearsal: &mut rehearsal,
            approval: &mut approval,
        };
        let mut engine =
            FixEngine::new(FixPolicy::default(), &journal, &mut audit, ports, &NoFaults);
        engine.run(&fix, &fix_plan, &context)
    };

    let changes = executor.changes.get();
    let events = audit.events().to_vec();
    match run {
        Ok(outcome) => Ok(FixRun {
            outcome: Some(outcome),
            audit: events,
            changes,
            refused: false,
            error: None,
        }),
        Err(error) => {
            let refused = matches!(
                error,
                FixError::Refused { .. }
                    | FixError::TierNotFixable { .. }
                    | FixError::ApprovalRequired
                    | FixError::ApprovalRefused { .. }
                    | FixError::RehearsalRequired
            );
            Ok(FixRun {
                outcome: None,
                audit: events,
                changes,
                refused,
                error: Some(format!("{error}")),
            })
        }
    }
}

/// Whether a remedy error is an explicit policy refusal the evaluation reports as `refused`.
fn is_refusal(error: &RemedyError) -> bool {
    matches!(
        error,
        RemedyError::Unknown { .. }
            | RemedyError::Refused { .. }
            | RemedyError::ParamsInvalid { .. }
            | RemedyError::PreconditionFailed { .. }
    )
}

/// Authorises and dry-runs a remedy without changing the tenant. The outer error is harness
/// infrastructure; the inner error is the remedy framework's own refusal.
///
/// # Errors
///
/// [`EvalError::Simulation`] when the shared state is unusable.
pub(crate) fn preview(
    shared: &Shared,
    remedy: &RemedyId,
    params: &Json,
) -> Result<std::result::Result<Plan, RemedyError>> {
    let mut tenant = shared.tenant.borrow_mut();
    let settings = tenant.settings.clone();
    let actor = shared.agent.id.clone();
    let now = tenant.now;
    let mut context = tenant.world.ctx(&actor, now, &settings);
    Ok(shared
        .registry
        .dry_run(&shared.agent, remedy, params, &Gate::change(), &mut context))
}

/// The pre-image store over the tenant: a capture is a whole-world clone keyed by each target's
/// digest, and a restore puts that clone back. Whole-world restore is stricter than restoring
/// each field and makes the residue check exact.
struct RemedyPreImages {
    shared: Shared,
    captured: RefCell<BTreeMap<String, Tenant>>,
}

impl RemedyPreImages {
    fn new(shared: Shared) -> Self {
        Self {
            shared,
            captured: RefCell::new(BTreeMap::new()),
        }
    }
}

impl PreImageStore for RemedyPreImages {
    fn capture(&mut self, targets: &[String]) -> spot_fix_pipeline::Result<Vec<PreImage>> {
        let tenant = self.shared.tenant.borrow();
        let mut captured = self.captured.borrow_mut();
        let mut images = Vec::with_capacity(targets.len());
        for target in targets {
            let digest = tenant.target_digest(target);
            captured.insert(digest.clone(), tenant.clone());
            images.push(PreImage::new(
                target,
                &digest,
                serde_json::json!({"site": tenant.site, "target": target, "digest": digest}),
            ));
        }
        Ok(images)
    }

    fn restore(&mut self, image: &PreImage) -> spot_fix_pipeline::Result<()> {
        let snapshot = self
            .captured
            .borrow()
            .get(&image.digest)
            .cloned()
            .ok_or_else(|| FixError::PreImageFailed {
                detail: format!("no captured image for `{}`", image.target),
            })?;
        *self.shared.tenant.borrow_mut() = snapshot;
        Ok(())
    }

    fn digest(&mut self, target: &str) -> spot_fix_pipeline::Result<String> {
        Ok(self.shared.tenant.borrow().target_digest(target))
    }
}

/// The snapshot port over the tenant.
struct RemedySnapshots {
    shared: Shared,
    taken: RefCell<BTreeMap<String, Tenant>>,
    gate_closed: Cell<bool>,
}

impl RemedySnapshots {
    fn new(shared: Shared) -> Self {
        Self {
            shared,
            taken: RefCell::new(BTreeMap::new()),
            gate_closed: Cell::new(false),
        }
    }
}

impl SnapshotPort for RemedySnapshots {
    fn take(&mut self, fix: &FixId) -> spot_fix_pipeline::Result<SnapshotRef> {
        let tenant = self.shared.tenant.borrow();
        let digest = tenant.fingerprint().digest();
        self.taken
            .borrow_mut()
            .insert(fix.as_str().to_owned(), tenant.clone());
        Ok(SnapshotRef::new(fix.as_str(), &digest))
    }

    fn restore(&mut self, snapshot: &SnapshotRef) -> spot_fix_pipeline::Result<()> {
        let image = self
            .taken
            .borrow()
            .get(&snapshot.id)
            .cloned()
            .ok_or_else(|| FixError::SnapshotFailed {
                detail: format!("no snapshot `{}`", snapshot.id),
            })?;
        *self.shared.tenant.borrow_mut() = image;
        Ok(())
    }

    fn close_gate(&mut self, _reason: &str) -> spot_fix_pipeline::Result<()> {
        self.gate_closed.set(true);
        Ok(())
    }

    fn open_gate(&mut self) -> spot_fix_pipeline::Result<()> {
        self.gate_closed.set(false);
        Ok(())
    }
}

/// The executor: the first normalised step applies the sealed remedy plan, later steps read the
/// same post-state. A remedy is one atomic typed change in this catalogue, so the mapping is
/// exact; the pipeline still journals every step.
struct RemedyExecutor {
    shared: Shared,
    remedy: RemedyId,
    plan: Plan,
    undo: RefCell<Option<spot_remedies::UndoToken>>,
    applied: Cell<bool>,
    changes: Cell<usize>,
}

impl RemedyExecutor {
    fn new(shared: Shared, remedy: RemedyId, plan: Plan) -> Self {
        Self {
            shared,
            remedy,
            plan,
            undo: RefCell::new(None),
            applied: Cell::new(false),
            changes: Cell::new(0),
        }
    }
}

impl FixExecutor for RemedyExecutor {
    fn run_step(
        &mut self,
        step: &FixStep,
        _idempotency_key: &str,
    ) -> spot_fix_pipeline::Result<StepChange> {
        if self.applied.get() {
            let tenant = self.shared.tenant.borrow();
            let digest = tenant.target_digest(&step.target);
            return Ok(StepChange {
                before: digest.clone(),
                after: digest,
            });
        }
        let mut tenant = self.shared.tenant.borrow_mut();
        let settings = tenant.settings.clone();
        let actor = self.shared.agent.id.clone();
        let now = tenant.now;
        let applied = {
            let mut context = tenant.world.ctx(&actor, now, &settings);
            self.shared.registry.apply(
                &self.shared.agent,
                &self.remedy,
                &self.plan,
                &Gate::change(),
                &mut context,
            )
        };
        let applied = applied.map_err(|error| match error {
            RemedyError::Refused { detail, .. } => FixError::Refused { detail },
            other => FixError::StepFailed {
                index: step.index,
                id: step.id.clone(),
                detail: other.sentence(),
            },
        })?;
        self.changes.set(applied.changes.len());
        self.undo.replace(Some(applied.undo_token.clone()));
        self.applied.set(true);
        let digest = tenant.target_digest(&step.target);
        drop(tenant);
        Ok(StepChange {
            before: digest.clone(),
            after: digest,
        })
    }

    fn undo_step(
        &mut self,
        step: &FixStep,
        _change: &ChangeRecord,
    ) -> spot_fix_pipeline::Result<()> {
        if step.index != 0 {
            return Ok(());
        }
        let Some(token) = self.undo.borrow().clone() else {
            return Ok(());
        };
        let mut tenant = self.shared.tenant.borrow_mut();
        let settings = tenant.settings.clone();
        let actor = self.shared.agent.id.clone();
        let now = tenant.now;
        let mut context = tenant.world.ctx(&actor, now, &settings);
        self.shared
            .registry
            .undo(
                &self.shared.agent,
                &self.remedy,
                &token,
                &Gate::change(),
                &mut context,
            )
            .map(|_| ())
            .map_err(|error| FixError::StepFailed {
                index: step.index,
                id: step.id.clone(),
                detail: error.sentence(),
            })
    }
}

/// The verifier: the incident's success check is the remedy-specific check and the symptom
/// re-check; the named tenant invariants are the invariant set.
struct RemedyVerifier {
    shared: Shared,
    check: SuccessCheck,
}

impl RemedyVerifier {
    fn new(shared: Shared, check: SuccessCheck) -> Self {
        Self { shared, check }
    }
}

impl FixVerifier for RemedyVerifier {
    fn verify(
        &mut self,
        _plan: &FixPlan,
        _changes: &[ChangeRecord],
    ) -> spot_fix_pipeline::Result<VerifyReport> {
        let tenant = self.shared.tenant.borrow();
        let violations = tenant.evaluate(&self.check);
        let symptom_cleared = tenant.symptom_cleared(&self.check);
        Ok(VerifyReport {
            invariants: tenant.invariant_report(&self.check.invariants),
            remedy_verified: violations.is_empty(),
            symptom_cleared,
            violations,
            fingerprint: tenant.fingerprint(),
        })
    }

    fn invariants(&mut self) -> spot_fix_pipeline::Result<spot_rehearsal::InvariantReport> {
        let tenant = self.shared.tenant.borrow();
        Ok(tenant.invariant_report(&self.check.invariants))
    }

    fn fingerprint(&mut self) -> spot_fix_pipeline::Result<Fingerprint> {
        Ok(self.shared.tenant.borrow().fingerprint())
    }
}

/// The rehearsal: apply the same remedy to a clone through a fresh registry and evaluate the same
/// success check there.
struct RemedyRehearsal {
    shared: Shared,
    remedy: RemedyId,
    params: Json,
    check: SuccessCheck,
}

impl RemedyRehearsal {
    fn new(shared: Shared, remedy: RemedyId, params: Json, check: SuccessCheck) -> Self {
        Self {
            shared,
            remedy,
            params,
            check,
        }
    }
}

impl RehearsalPort for RemedyRehearsal {
    fn rehearse(&mut self, _plan: &FixPlan) -> spot_fix_pipeline::Result<RehearsalEvidence> {
        let mut clone = self.shared.tenant.borrow().clone();
        let registry = build_registry().map_err(|error| FixError::RehearsalFailed {
            detail: error.to_string(),
        })?;
        let settings = clone.settings.clone();
        let actor = self.shared.agent.id.clone();
        let now = clone.now;
        let applied = {
            let mut context = clone.world.ctx(&actor, now, &settings);
            registry
                .dry_run(
                    &self.shared.agent,
                    &self.remedy,
                    &self.params,
                    &Gate::change(),
                    &mut context,
                )
                .and_then(|plan| {
                    registry.apply(
                        &self.shared.agent,
                        &self.remedy,
                        &plan,
                        &Gate::change(),
                        &mut context,
                    )
                })
                .map_err(|error| FixError::RehearsalFailed {
                    detail: error.sentence(),
                })
        };
        let passed = applied.is_ok() && clone.evaluate(&self.check).is_empty();
        let detail = if passed {
            "the clone cleared the symptom and held every invariant".to_owned()
        } else {
            clone
                .evaluate(&self.check)
                .first()
                .cloned()
                .unwrap_or_else(|| "the clone did not verify".to_owned())
        };
        let fingerprint = clone.fingerprint();
        Ok(RehearsalEvidence {
            pre_digest: self.shared.tenant.borrow().fingerprint().digest(),
            post_digest: fingerprint.digest(),
            fingerprint,
            invariants: clone.invariant_report(&self.check.invariants),
            passed,
            detail,
        })
    }
}

/// An approval port that stands in for the human inbox in the harness's negative proofs.
struct EvalApproval {
    now: i64,
}

impl ApprovalPort for EvalApproval {
    fn request(
        &mut self,
        fix: &FixId,
        _plan: &FixPlan,
        _agent: &AgentRef,
        _reason: &str,
    ) -> spot_fix_pipeline::Result<Option<Approval>> {
        Ok(Some(Approval {
            approver: "support-lead".to_owned(),
            reference: format!("eval-{fix}"),
            at: self.now,
        }))
    }
}
