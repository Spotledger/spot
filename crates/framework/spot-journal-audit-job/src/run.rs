//! One pass over the tenants that are due.

use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};

use spot_conditions::{AuditSink, ConditionEngine};
use spot_journal_audit::VerifiedPosition;
use spot_journal_audit_run::{AuditRun, AuditRunError, Clock, FailureStage, run_audit};
use spot_journal_manifest::VerifyingKeys;
use spot_journal_rebuild::RebuildKeys;
use spot_journal_store::TenantId;
use tokio::runtime::Handle;

use crate::cadence::{TenantAuditPlan, due};
use crate::conditions;
use crate::error::AuditJobError;
use crate::providers::{AuditInputs, ResultSink, TenantDirectory};

/// Where and why a tenant's audit did not finish. Stable names and codes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailureInfo {
    /// `snapshots`, `plan` or `rebuild` (the runner's stages), `refused` (the runner or the job
    /// refused the wiring), `inputs` (a provider could not open the inputs) or `panic`.
    pub stage: &'static str,
    /// The stable code of the failing crate's error.
    pub code: &'static str,
}

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
    TenantOutcome::Failed(FailureInfo {
        stage: "refused",
        code: error.code(),
    })
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
) -> (TenantOutcome, Option<AuditRun>) {
    let mut opened = match inputs.open(&plan.tenant) {
        Ok(opened) => opened,
        Err(error) => {
            return (
                TenantOutcome::Failed(FailureInfo {
                    stage: "inputs",
                    code: error.code(),
                }),
                None,
            );
        }
    };
    if opened.options.tenant != plan.tenant {
        return (
            refused(&AuditJobError::TenantMismatch { side: "inputs" }),
            None,
        );
    }
    let manifest: &VerifyingKeys = &opened.manifest_keys;
    let keys = RebuildKeys {
        manifest,
        snapshot: opened.snapshot_keys.as_ref(),
    };
    let run = exec.block_on(run_audit(
        opened.store.as_ref(),
        &keys,
        opened.registry.as_ref(),
        opened.live.as_ref(),
        opened.scratch.as_mut(),
        &opened.options,
        clock,
    ));
    match run {
        Err(error) => (
            TenantOutcome::Failed(FailureInfo {
                stage: "refused",
                code: AuditRunError::code(&error),
            }),
            None,
        ),
        Ok(run @ AuditRun::Failed(failure)) => (
            TenantOutcome::Failed(FailureInfo {
                stage: failure_stage(failure.stage),
                code: failure.code,
            }),
            Some(run),
        ),
        Ok(run) if run.is_clean() => (TenantOutcome::Clean, Some(run)),
        Ok(run) => (TenantOutcome::Mismatch, Some(run)),
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
        let (outcome, run) = caught.unwrap_or_else(|_| {
            (
                TenantOutcome::Failed(FailureInfo {
                    stage: "panic",
                    code: AuditJobError::TenantPanicked.code(),
                }),
                None,
            )
        });
        let mut result = TenantResult {
            tenant: plan.tenant,
            started_millis: now_millis,
            finished_millis: clock.now_millis().max(now_millis),
            outcome,
            run,
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
