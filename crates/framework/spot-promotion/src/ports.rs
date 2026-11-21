//! The ports the pipeline drives, and the concrete bundle applier.
//!
//! The pipeline never touches a site or the audit chain itself. It plans and writes through a
//! [`BundleApplier`], rehearses through a [`RehearsalRunner`], and records every transition
//! through a [`PromotionAudit`]. The production wiring supplies a storage-backed site, a
//! snapshot-backed rehearsal runner and the migration audit chain; the tests supply in-memory
//! doubles, which is why a failed rehearsal can be proved to leave the target untouched.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_custom_bundle::{
    ApplyPlan, Bundle, CaptureOptions, CustomBundleError, Limits, SectionRegistry, SiteReader,
    SiteWriter, apply, capture_with, plan_apply,
};
use spot_migration_approval::ApplyTicket;
use spot_migration_audit::{
    ApplyEvent as AuditApply, ApprovalEvent as AuditApproval, FinishEvent as AuditFinish, PlanRef,
    RehearsalEvent as AuditRehearsal, RunHandle, RunId, RunRecorder, RunSpec,
};
use spot_rehearsal::RehearsalReport;

use crate::error::PromotionError;
use crate::plan::PromotionPlan;
use spot_environments::EnvRef;

/// A site the pipeline can read and write: a target environment's customisation surface.
pub trait TargetSite {
    /// The environment this site belongs to.
    fn environment(&self) -> EnvRef;

    /// The site as the reader planning and capture take.
    fn as_reader(&self) -> &dyn SiteReader;
    /// The site as the writer apply and restore take.
    fn as_writer(&mut self) -> &mut dyn SiteWriter;
}

/// What an apply wrote.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApplyEvidence {
    /// The digest of the applied bundle.
    pub bundle_digest: String,
    /// The apply plan's hash: the exact plan that ran.
    pub plan_hash: String,
    /// How many items were written.
    pub steps: u64,
    /// The bundle planner's warnings, in plain text.
    pub warnings: Vec<String>,
}

/// What verification found.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VerifyEvidence {
    /// Whether the target matches the approved plan.
    pub passed: bool,
    /// The items that still differ, when it does not.
    pub findings: Vec<String>,
    /// The hash of the plan re-derived against the target after apply.
    pub replanned_hash: String,
}

/// What a restore of the pre-image did.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RestoreEvidence {
    /// Whether the restore completed.
    pub restored: bool,
    /// How many items were written back.
    pub steps: u64,
}

/// The recorded rehearsal: the verdict, the findings and the report a reviewer reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RehearsalEvidence {
    /// The rehearsal report.
    pub report: spot_rehearsal::RehearsalReport,
}

impl RehearsalEvidence {
    /// The verdict text (`clean`, `expected_changes`, `unexplained_differences`,
    /// `invariants_failed`).
    #[must_use]
    pub fn verdict(&self) -> &'static str {
        verdict_name(self.report().outcome)
    }

    /// The rehearsal report itself.
    #[must_use]
    pub fn report(&self) -> &RehearsalReport {
        &self.report
    }

    /// How many findings the rehearsal reported: unexplained differences and invariant
    /// violations.
    #[must_use]
    pub fn findings(&self) -> u64 {
        let report = self.report();
        let unexplained = u64::try_from(report.unexplained().count()).unwrap_or(u64::MAX);
        unexplained.saturating_add(report.invariants.total())
    }

    /// Whether the rehearsal admits apply.
    #[must_use]
    pub fn admits_apply(&self) -> bool {
        self.report().invariants.passed()
            && matches!(
                self.report().outcome,
                spot_rehearsal::RehearsalOutcome::Clean
                    | spot_rehearsal::RehearsalOutcome::ExpectedChanges
            )
    }

    /// The report as the JSON artefact stored in the audit record.
    #[must_use]
    pub fn to_json(&self) -> String {
        self.report().to_json()
    }
}

/// The stable text of a rehearsal verdict.
fn verdict_name(outcome: spot_rehearsal::RehearsalOutcome) -> &'static str {
    match outcome {
        spot_rehearsal::RehearsalOutcome::Clean => "clean",
        spot_rehearsal::RehearsalOutcome::ExpectedChanges => "expected_changes",
        spot_rehearsal::RehearsalOutcome::UnexplainedDifferences => "unexplained_differences",
        spot_rehearsal::RehearsalOutcome::InvariantsFailed => "invariants_failed",
    }
}

/// The migration-audit verdict matching a rehearsal outcome.
#[must_use]
pub(crate) fn audit_verdict(
    outcome: spot_rehearsal::RehearsalOutcome,
) -> spot_migration_audit::RehearsalVerdict {
    use spot_migration_audit::RehearsalVerdict;
    match outcome {
        spot_rehearsal::RehearsalOutcome::Clean => RehearsalVerdict::Clean,
        spot_rehearsal::RehearsalOutcome::ExpectedChanges => RehearsalVerdict::ExpectedChanges,
        spot_rehearsal::RehearsalOutcome::UnexplainedDifferences => {
            RehearsalVerdict::UnexplainedDifferences
        }
        spot_rehearsal::RehearsalOutcome::InvariantsFailed => RehearsalVerdict::InvariantsFailed,
    }
}

/// The evidence a failed step leaves behind.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FailureEvidence {
    /// The step that failed (`rehearsal`, `apply`, `verify`, `restore`).
    pub stage: String,
    /// The stable code of the failure.
    pub code: String,
    /// What happened, in a sentence (never content or a secret).
    pub message: String,
    /// The verification findings, when the failure was verification.
    pub findings: Vec<String>,
}

/// The apply, verify and restore operations the pipeline drives.
///
/// The real implementation is [`CustomBundleApplier`]; a host that stores customisations in a
/// database implements the same trait over its storage-backed [`TargetSite`]. The trait is a
/// port, not a second engine: it only executes the plan it is handed.
pub trait BundleApplier: Send + Sync {
    /// Plans the bundle against the target. Must not write anything.
    ///
    /// # Errors
    /// [`PromotionError::PlanFailed`] when the bundle is invalid or incompatible.
    fn plan(&self, bundle: &Bundle, target: &dyn TargetSite) -> Result<ApplyPlan, PromotionError>;

    /// Captures the target's current customisations as a bundle, so a failed verification can
    /// restore exactly that state.
    ///
    /// # Errors
    /// [`PromotionError::ApplyFailed`] when the target cannot be captured.
    fn capture(&self, target: &dyn TargetSite) -> Result<Bundle, PromotionError>;

    /// Writes the plan to the target in one transaction.
    ///
    /// # Errors
    /// [`PromotionError::ApplyFailed`] when the site refuses a change; the site is left as it
    /// was.
    fn apply(
        &self,
        plan: &ApplyPlan,
        target: &mut dyn TargetSite,
    ) -> Result<ApplyEvidence, PromotionError>;

    /// Verifies the target matches the applied plan: a second plan of the same bundle must be
    /// empty.
    ///
    /// # Errors
    /// [`PromotionError::PlanFailed`] when the target cannot be re-planned, and
    /// [`PromotionError::ApplyFailed`] when the capture-equivalent check fails.
    fn verify(
        &self,
        plan: &ApplyPlan,
        bundle: &Bundle,
        target: &dyn TargetSite,
    ) -> Result<VerifyEvidence, PromotionError>;

    /// Applies the captured pre-image back onto the target.
    ///
    /// # Errors
    /// [`PromotionError::RestoreFailed`] when the target cannot be restored.
    fn restore(
        &self,
        pre_image: &Bundle,
        target: &mut dyn TargetSite,
    ) -> Result<RestoreEvidence, PromotionError>;
}

/// The bundle applier: `spot-custom-bundle`'s validate/plan/apply, plus capture for the
/// pre-image and a re-plan for verification.
pub struct CustomBundleApplier {
    registry: SectionRegistry,
    limits: Limits,
}

impl CustomBundleApplier {
    /// An applier using `registry` for the bundle's sections and `limits` for capture.
    #[must_use]
    pub fn new(registry: SectionRegistry, limits: Limits) -> Self {
        Self { registry, limits }
    }

    /// An applier with the built-in sections and the default limits.
    #[must_use]
    pub fn with_builtin_sections() -> Self {
        Self::new(SectionRegistry::new(), Limits::default())
    }

    /// One custom bundle error as a plan failure naming its stable code.
    fn plan_error(errors: Vec<CustomBundleError>) -> PromotionError {
        let message = errors.first().map_or_else(
            || "the bundle was refused against the target".to_owned(),
            |error| format!("{}: {}", error.code(), error.item()),
        );
        PromotionError::PlanFailed { message }
    }

    /// The fixed capture options of a pre-image: the bundle has no user-facing identity, and
    /// nothing here reads a clock.
    fn pre_image_options() -> CaptureOptions {
        CaptureOptions::new(
            "promotion-pre-image",
            "0.0.0",
            "spot-promotion",
            "1970-01-01T00:00:00Z",
        )
    }
}

impl BundleApplier for CustomBundleApplier {
    fn plan(&self, bundle: &Bundle, target: &dyn TargetSite) -> Result<ApplyPlan, PromotionError> {
        plan_apply(bundle, target.as_reader(), &self.registry).map_err(Self::plan_error)
    }

    fn capture(&self, target: &dyn TargetSite) -> Result<Bundle, PromotionError> {
        capture_with(
            target.as_reader(),
            &Self::pre_image_options(),
            &self.registry,
            &self.limits,
        )
        .map_err(|error| PromotionError::ApplyFailed {
            message: format!("the target could not be captured: {}", error.code()),
        })
    }

    fn apply(
        &self,
        plan: &ApplyPlan,
        target: &mut dyn TargetSite,
    ) -> Result<ApplyEvidence, PromotionError> {
        let report = apply(plan, target.as_writer(), &self.registry).map_err(|error| {
            PromotionError::ApplyFailed {
                message: format!("{}: {}", error.code(), error.item()),
            }
        })?;
        Ok(ApplyEvidence {
            bundle_digest: plan.digest().to_hex(),
            plan_hash: plan.hash().to_owned(),
            steps: u64::try_from(report.applied().len()).unwrap_or(u64::MAX),
            warnings: report
                .warnings()
                .iter()
                .map(|problem| format!("{}: {}", problem.code, problem.message))
                .collect(),
        })
    }

    fn verify(
        &self,
        _plan: &ApplyPlan,
        bundle: &Bundle,
        target: &dyn TargetSite,
    ) -> Result<VerifyEvidence, PromotionError> {
        let remaining =
            plan_apply(bundle, target.as_reader(), &self.registry).map_err(Self::plan_error)?;
        let findings: Vec<String> = remaining
            .changes()
            .iter()
            .map(|change| format!("{}/{}:{}", change.section, change.key, change.hash))
            .collect();
        // A second plan of the same bundle against the applied target is empty: that is the
        // whole verification. The empty plan's own hash is not the applied plan's hash.
        let passed = remaining.is_empty();
        Ok(VerifyEvidence {
            passed,
            findings,
            replanned_hash: remaining.hash().to_owned(),
        })
    }

    fn restore(
        &self,
        pre_image: &Bundle,
        target: &mut dyn TargetSite,
    ) -> Result<RestoreEvidence, PromotionError> {
        let plan = plan_apply(pre_image, target.as_reader(), &self.registry).map_err(|errors| {
            PromotionError::RestoreFailed {
                message: errors.first().map_or_else(
                    || "the pre-image could not be planned".to_owned(),
                    |error| format!("{}: {}", error.code(), error.item()),
                ),
            }
        })?;
        let report = apply(&plan, target.as_writer(), &self.registry).map_err(|error| {
            PromotionError::RestoreFailed {
                message: format!("{}: {}", error.code(), error.item()),
            }
        })?;
        Ok(RestoreEvidence {
            restored: true,
            steps: u64::try_from(report.applied().len()).unwrap_or(u64::MAX),
        })
    }
}

/// Everything a rehearsal needs: the approved plan, a clone of the target, and the approved
/// ticket that binds the plan digest.
pub struct RehearsalRequest<'a> {
    /// The plan being rehearsed.
    pub plan: &'a PromotionPlan,
    /// A clone of the target environment. The runner must never write the live target.
    pub target: &'a dyn TargetSite,
    /// The approval ticket `authorise_apply` issued for this plan; it cannot be built elsewhere.
    pub ticket: &'a ApplyTicket,
}

/// Rehearses a promotion on a clone.
///
/// The production implementation clones the target with `spot-snapshot`, applies the bundle to
/// the clone through a [`BundleApplier`], and runs the fingerprint and invariant sets through
/// `spot-rehearsal`'s engine, returning its [`RehearsalReport`]. The pipeline treats any verdict
/// other than clean or expected-changes as a failure.
pub trait RehearsalRunner: Send + Sync {
    /// Rehearses the plan and returns the report.
    ///
    /// # Errors
    /// [`PromotionError::RehearsalFailed`] when the clone cannot be made or the report cannot be
    /// produced. A rehearsal that *ran* and found problems returns its report; the pipeline
    /// decides from the verdict.
    fn rehearse(&self, request: &RehearsalRequest<'_>) -> Result<RehearsalReport, PromotionError>;
}

/// What opening a migration-audit run needs, plus the promotion it belongs to.
pub struct OpenRun<'a> {
    /// The promotion whose execution this run records.
    pub promotion: &'a RunId,
    /// The run id of this execution (the promotion id, or a resume id).
    pub run: RunId,
    /// The approved plan digest.
    pub plan_digest: &'a str,
    /// The stored plan document and its summary.
    pub plan: PlanRef,
    /// The actor in the audit chain's form (`system`, `user:<id>` or `agent:<id>`).
    pub actor: &'a str,
    /// Unix microseconds.
    pub at: i64,
}

/// Where every promotion transition is recorded: the migration audit store.
///
/// The record is plan-scoped, so [`open`](PromotionAudit::open) is called when the promotion is
/// planned (and again by a resume, under a derived run id); the `Requested` transition is in the
/// promotion journal the store persists with the state. Rehearsal, apply, approval and every
/// terminal outcome are audit events carrying the promotion id as the run id.
///
/// [`MigrationAudit`] is the production implementation. The trait exists so the pipeline can be
/// tested without building an audit chain, and so a host that stores the chain elsewhere can
/// bind it once.
pub trait PromotionAudit: Send + Sync {
    /// Opens the run for one execution and records the plan it covers.
    ///
    /// # Errors
    /// [`PromotionError::AuditFailed`] when the record cannot be appended.
    fn open<'a>(&'a self, request: OpenRun<'a>) -> PromotionFuture<'a>;

    /// Records one approval or rejection.
    ///
    /// # Errors
    /// [`PromotionError::AuditFailed`], or a refusal when no run is open for the promotion.
    fn approval<'a>(&'a self, promotion: &'a RunId, event: AuditApproval) -> PromotionFuture<'a>;

    /// Records a rehearsal report and its verdict.
    ///
    /// # Errors
    /// [`PromotionError::AuditFailed`], or a refusal when no run is open for the promotion.
    fn rehearsal<'a>(&'a self, promotion: &'a RunId, event: AuditRehearsal) -> PromotionFuture<'a>;

    /// Records that apply ran, with its journal.
    ///
    /// # Errors
    /// [`PromotionError::AuditFailed`], or a refusal when no run is open for the promotion.
    fn applied<'a>(&'a self, promotion: &'a RunId, event: AuditApply) -> PromotionFuture<'a>;

    /// Records the terminal outcome.
    ///
    /// # Errors
    /// [`PromotionError::AuditFailed`], or a refusal when no run is open for the promotion.
    fn finish<'a>(&'a self, promotion: &'a RunId, event: AuditFinish) -> PromotionFuture<'a>;
}

/// The future a [`PromotionAudit`] method returns.
pub type PromotionFuture<'a> = futures::future::BoxFuture<'a, Result<(), PromotionError>>;

/// The migration audit store: one run per execution, its events appended in order.
///
/// One recorder per site, because the audit chain is per site. The adapter keeps the
/// [`RunHandle`] the recorder issued; a fresh process calls
/// [`PromotionService::resume`](crate::PromotionService::resume), which opens a new run with a
/// derived id, so an event is never appended to a handle this process does not hold.
pub struct MigrationAudit {
    recorder: Arc<dyn RunRecorder>,
    open: Mutex<BTreeMap<String, RunHandle>>,
}

impl MigrationAudit {
    /// An adapter over `recorder`.
    #[must_use]
    pub fn new(recorder: Arc<dyn RunRecorder>) -> Self {
        Self {
            recorder,
            open: Mutex::new(BTreeMap::new()),
        }
    }

    /// The handle for a promotion's current execution.
    fn handle(&self, promotion: &RunId) -> Result<RunHandle, PromotionError> {
        self.open
            .lock()
            .map_err(|_| PromotionError::AuditFailed {
                message: "the open-run map is poisoned".to_owned(),
            })?
            .get(promotion.as_str())
            .cloned()
            .ok_or_else(|| PromotionError::AuditFailed {
                message: format!("no audit run is open for promotion {promotion}; resume it first"),
            })
    }
}

/// One migration-audit error as a promotion audit failure.
fn audit_error(error: spot_migration_audit::MigrationAuditError) -> PromotionError {
    PromotionError::AuditFailed {
        message: error.to_string(),
    }
}

impl PromotionAudit for MigrationAudit {
    fn open<'a>(&'a self, request: OpenRun<'a>) -> PromotionFuture<'a> {
        Box::pin(async move {
            let spec = RunSpec {
                id: request.run,
                plan_digest: request.plan_digest.to_owned(),
                plan: request.plan,
                actor: request.actor.to_owned(),
                at: request.at,
            };
            let handle = self.recorder.begin(spec).await.map_err(audit_error)?;
            self.open
                .lock()
                .map_err(|_| PromotionError::AuditFailed {
                    message: "the open-run map is poisoned".to_owned(),
                })?
                .insert(request.promotion.to_string(), handle);
            Ok(())
        })
    }

    fn approval<'a>(&'a self, promotion: &'a RunId, event: AuditApproval) -> PromotionFuture<'a> {
        Box::pin(async move {
            let handle = self.handle(promotion)?;
            self.recorder
                .record_approval(&handle, event)
                .await
                .map_err(audit_error)
        })
    }

    fn rehearsal<'a>(&'a self, promotion: &'a RunId, event: AuditRehearsal) -> PromotionFuture<'a> {
        Box::pin(async move {
            let handle = self.handle(promotion)?;
            self.recorder
                .record_rehearsal(&handle, event)
                .await
                .map_err(audit_error)
        })
    }

    fn applied<'a>(&'a self, promotion: &'a RunId, event: AuditApply) -> PromotionFuture<'a> {
        Box::pin(async move {
            let handle = self.handle(promotion)?;
            self.recorder
                .record_apply(&handle, event)
                .await
                .map_err(audit_error)
        })
    }

    fn finish<'a>(&'a self, promotion: &'a RunId, event: AuditFinish) -> PromotionFuture<'a> {
        Box::pin(async move {
            let handle = self.handle(promotion)?;
            self.recorder
                .finish(&handle, event)
                .await
                .map_err(audit_error)
        })
    }
}
