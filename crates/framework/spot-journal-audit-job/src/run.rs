//! One pass over the tenants that are due.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

use spot_conditions::{AuditSink, ConditionEngine};
use spot_journal_audit::VerifiedPosition;
use spot_journal_audit_run::{
    AuditOptions, AuditRun, AuditRunError, ClassifierKind, Clock, EventClassifier, FailureStage,
    REBUILD_UNKNOWN_TOPIC, UnknownTopics, run_audit,
};
use spot_journal_manifest::VerifyingKeys;
use spot_journal_rebuild::RebuildKeys;
use spot_journal_store::TenantId;
use tokio::runtime::Handle;

use crate::cadence::{TenantAuditPlan, due};
use crate::conditions;
use crate::error::AuditJobError;
use crate::providers::{AuditInputs, ResultSink, TenantDirectory, TenantInputs};

/// Where and why a tenant's audit did not finish. Stable names and codes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailureInfo {
    /// `snapshots`, `plan` or `rebuild` (the runner's stages), `refused` (the runner or the job
    /// refused the wiring), `inputs` (a provider could not open the inputs) or `panic`.
    pub stage: &'static str,
    /// The stable code of the failing crate's error.
    pub code: &'static str,
    /// Why, for the condition's remedy: `unknown_topics` when the rebuild stopped at a topic the
    /// classifier did not know (strict policy), `audit_failed` for every other failure.
    pub reason: &'static str,
}

impl FailureInfo {
    /// A failure at `stage` with `code`; the reason follows from the code.
    #[must_use]
    pub fn new(stage: &'static str, code: &'static str) -> Self {
        Self {
            stage,
            code,
            reason: if code == REBUILD_UNKNOWN_TOPIC {
                REASON_UNKNOWN_TOPICS
            } else {
                REASON_AUDIT_FAILED
            },
        }
    }
}

/// [`FailureInfo::reason`] of a rebuild that stopped at an unknown topic.
pub const REASON_UNKNOWN_TOPICS: &str = "unknown_topics";

/// [`FailureInfo::reason`] of every other failure.
pub const REASON_AUDIT_FAILED: &str = "audit_failed";

/// How one tenant's audit ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TenantOutcome {
    /// The records and the fingerprints match.
    Clean,
    /// The audit ran and found a difference.
    Mismatch,
    /// The audit did not finish.
    Failed(FailureInfo),
}

/// One tenant's result row. Carries digests, counts, identities and codes: never record
/// content.
#[derive(Clone, Debug, PartialEq)]
pub struct TenantResult {
    /// The tenant.
    pub tenant: TenantId,
    /// When the audit started (the `now` of the pass), in unix milliseconds.
    pub started_millis: i64,
    /// When it finished, from the clock.
    pub finished_millis: i64,
    /// How it ended.
    pub outcome: TenantOutcome,
    /// The audit run, when the runner produced one (not for a refusal, a provider failure or a
    /// panic).
    pub run: Option<AuditRun>,
    /// The code of the condition framework's refusal, if raising or clearing failed.
    pub condition_failure: Option<&'static str>,
    /// Which classifier decided this tenant's run (`registry` or `injected`).
    pub classifier: ClassifierKind,
    /// The topics the classifier did not know: names only, at most 50, then a count. Under the
    /// strict policy a failed run lists the one topic that stopped it.
    pub unknown_topics: UnknownTopics,
}

impl TenantResult {
    /// The verified position of a clean run, for the recovery report.
    #[must_use]
    pub fn verified_position(&self) -> Option<VerifiedPosition> {
        self.run.as_ref().and_then(AuditRun::verified_position)
    }
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunSummary {
    /// The tenants audited in this pass, in directory order.
    pub results: Vec<TenantResult>,
    /// Tenants skipped because they were not due.
    pub not_due: usize,
    /// Results the sink could not record (those tenants are audited again by the next pass).
    pub sink_failures: usize,
}

impl RunSummary {
    /// The result of one tenant in this pass.
    #[must_use]
    pub fn result(&self, tenant: &str) -> Option<&TenantResult> {
        self.results.iter().find(|r| r.tenant.as_str() == tenant)
    }

    /// How many results could not raise or clear their conditions (for example a registry
    /// without the catalogue's `JOURNAL_AUDIT_MISMATCH`).
    #[must_use]
    pub fn condition_failures(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.condition_failure.is_some())
            .count()
    }
}

fn refused(error: &AuditJobError) -> TenantOutcome {
    TenantOutcome::Failed(FailureInfo::new("refused", error.code()))
}

/// What an audit of one tenant produced besides the outcome.
struct Audited {
    outcome: TenantOutcome,
    run: Option<AuditRun>,
    classifier: ClassifierKind,
}

impl Audited {
    fn without_run(outcome: TenantOutcome) -> Self {
        Self {
            outcome,
            run: None,
            classifier: ClassifierKind::Registry,
        }
    }
}

/// The unknown topics a run reports: a completed run's list, or the one topic that stopped a
/// strict rebuild.
fn unknown_topics_of(run: Option<&AuditRun>) -> UnknownTopics {
    match run {
        Some(AuditRun::Completed(report)) => report.unknown_topics.clone(),
        Some(AuditRun::Failed(failure)) => failure
            .unknown_topic
            .as_deref()
            .map_or_else(UnknownTopics::none, |name| {
                UnknownTopics::from_names([name], false)
            }),
        None => UnknownTopics::none(),
    }
}

fn failure_stage(stage: FailureStage) -> &'static str {
    match stage {
        FailureStage::Snapshots => "snapshots",
        FailureStage::Plan => "plan",
        FailureStage::Rebuild => "rebuild",
    }
}

/// Opens and runs one tenant's audit. Panics are the caller's to catch.
fn audit_tenant(
    exec: &Handle,
    plan: &TenantAuditPlan,
    inputs: &dyn AuditInputs,
    clock: &dyn Clock,
) -> Audited {
    let opened = match inputs.open(&plan.tenant) {
        Ok(opened) => opened,
        Err(error) => {
            return Audited::without_run(TenantOutcome::Failed(FailureInfo::new(
                "inputs",
                error.code(),
            )));
        }
    };
    let TenantInputs {
        options,
        live,
        mut scratch,
        store,
        manifest_keys,
        snapshot_keys,
        registry,
        classifier,
        strict_topics,
    } = opened;
    if options.tenant != plan.tenant {
        return Audited::without_run(refused(&AuditJobError::TenantMismatch { side: "inputs" }));
    }
    // This tenant's classifier and policy, and nobody else's: they live in this call only. A
    // classifier the host already put in the options stays unless the inputs carry one, and the
    // policy is relaxed when either says so (neither can silently make the other strict again).
    let mut options: AuditOptions<'_> = options;
    if let Some(classifier) = classifier.as_ref() {
        options.classifier = Some(classifier.as_ref() as &dyn EventClassifier);
    }
    options.strict_topics = options.strict_topics && strict_topics;
    let kind = if options.classifier.is_some() {
        ClassifierKind::Injected
    } else {
        ClassifierKind::Registry
    };
    let manifest: &VerifyingKeys = &manifest_keys;
    let keys = RebuildKeys {
        manifest,
        snapshot: snapshot_keys.as_ref(),
    };
    let run = exec.block_on(run_audit(
        store.as_ref(),
        &keys,
        registry.as_ref(),
        live.as_ref(),
        scratch.as_mut(),
        &options,
        clock,
    ));
    let (outcome, run) = match run {
        Err(error) => (
            TenantOutcome::Failed(FailureInfo::new("refused", AuditRunError::code(&error))),
            None,
        ),
        Ok(run) => {
            let outcome = match &run {
                AuditRun::Failed(failure) => TenantOutcome::Failed(FailureInfo::new(
                    failure_stage(failure.stage),
                    failure.code,
                )),
                _ if run.is_clean() => TenantOutcome::Clean,
                _ => TenantOutcome::Mismatch,
            };
            (outcome, Some(run))
        }
    };
    Audited {
        outcome,
        run,
        classifier: kind,
    }
}

/// Audits every tenant that is due at `now_millis`.
///
/// Each tenant runs in isolation: an error, a refusal or a panic for one tenant is that tenant's
/// [`TenantOutcome::Failed`] and never stops the others. After each audit the conditions are
/// updated and the result is handed to `sink`. A tenant listed twice is audited once.
///
/// This is blocking and heavy: call it from a blocking thread. `exec` drives the async journal
/// reads of the rebuild from that thread; it must not be called from inside the runtime's
/// async context.
///
/// # Errors
///
/// [`AuditJobError::ProviderFailed`] when the directory cannot be read.
pub fn run_due<S: AuditSink>(
    exec: &Handle,
    now_millis: i64,
    directory: &dyn TenantDirectory,
    inputs: &dyn AuditInputs,
    clock: &dyn Clock,
    conditions: &mut ConditionEngine<S>,
    sink: &dyn ResultSink,
) -> Result<RunSummary, AuditJobError> {
    let plans = directory.tenants()?;
    let mut summary = RunSummary::default();
    let mut seen = BTreeSet::new();
    for plan in plans {
        if !seen.insert(plan.tenant.clone()) {
            continue;
        }
        if !due(&plan, now_millis) {
            summary.not_due += 1;
            continue;
        }
        let caught = catch_unwind(AssertUnwindSafe(|| {
            audit_tenant(exec, &plan, inputs, clock)
        }));
        let audited = caught.unwrap_or_else(|_| {
            Audited::without_run(TenantOutcome::Failed(FailureInfo::new(
                "panic",
                AuditJobError::TenantPanicked.code(),
            )))
        });
        let mut result = TenantResult {
            tenant: plan.tenant,
            started_millis: now_millis,
            finished_millis: clock.now_millis().max(now_millis),
            unknown_topics: unknown_topics_of(audited.run.as_ref()),
            classifier: audited.classifier,
            outcome: audited.outcome,
            run: audited.run,
            condition_failure: None,
        };
        if let Err(AuditJobError::ConditionFailed { code }) = conditions::apply(conditions, &result)
        {
            result.condition_failure = Some(code);
        }
        if sink.record(&result.tenant, &result).is_err() {
            summary.sink_failures += 1;
        }
        summary.results.push(result);
    }
    Ok(summary)
}
