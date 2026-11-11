//! The two red conditions of the scheduled audit and how a result raises or clears them.

use spot_conditions::AuditSink;
use spot_conditions::{
    ConditionDef, ConditionEngine, ConditionParams, ConditionsError, Millis, Registry, Severity,
};

use crate::error::AuditJobError;
use crate::run::{FailureInfo, TenantOutcome, TenantResult};

pub use spot_journal_audit::JOURNAL_AUDIT_MISMATCH;

/// The condition raised when the audit could not run or could not finish (a failed rebuild, a
/// provider that could not open the inputs, a panic). Not in the ADR-067 table yet: see the
/// crate's open questions.
pub const JOURNAL_AUDIT_FAILED: &str = "JOURNAL_AUDIT_FAILED";

/// The subject kind of both conditions (the catalogue's `JOURNAL_AUDIT_MISMATCH` agrees).
pub const SUBJECT_KIND: &str = "tenant";

/// Registers [`JOURNAL_AUDIT_FAILED`], the one definition this crate owns until it has an ADR-067
/// row.
///
/// The host builds its registry from `spot_sync_conditions::catalogue()`, which holds
/// [`JOURNAL_AUDIT_MISMATCH`], and then calls this function on it. Registering the mismatch here
/// as well would be a duplicate id, so this crate does not. A host that skips the catalogue gets
/// a typed [`AuditJobError::ConditionFailed`] (code `conditions.unknown_id`) on every mismatch
/// and every clean pass, counted by [`RunSummary::condition_failures`](crate::RunSummary), never
/// a panic or a silent skip.
///
/// # Errors
///
/// [`ConditionsError`] when the definition is already registered.
pub fn register_conditions(registry: &mut Registry) -> Result<(), ConditionsError> {
    registry.register(
        ConditionDef::new(
            JOURNAL_AUDIT_FAILED,
            Severity::Red,
            SUBJECT_KIND,
            concat!(
                "sync",
                ".condition",
                ".journal_audit_failed",
                ".explanation"
            ),
        )
        .with_param("stage")
        .with_param("code")
        .with_action("re_verify"),
    )
}

fn failed(error: &ConditionsError) -> AuditJobError {
    AuditJobError::ConditionFailed { code: error.code() }
}

/// What differs first, as `(record, difference)`: `entity/id` and the difference kind, or the
/// first differing fingerprint's name. Identity and kind only, never content.
fn first_difference(result: &TenantResult) -> (String, &'static str) {
    if let Some(run) = &result.run {
        if let Some(first) = run.first_difference() {
            return (first.record(), first.kind.name());
        }
        if let Some(report) = run.report()
            && let Some(name) = report.fingerprints.names().first()
        {
            return ((*name).to_owned(), "fingerprint_differs");
        }
    }
    ("unknown".to_owned(), "unknown")
}

/// Raises or clears the conditions for one tenant's result.
///
/// A clean run clears both; a mismatch raises `JOURNAL_AUDIT_MISMATCH` (the audit ran, so any
/// open failure clears); a failed run raises `JOURNAL_AUDIT_FAILED` and leaves a mismatch
/// alone, because a failed run proves nothing about it.
///
/// # Errors
///
/// [`AuditJobError::ConditionFailed`] when the framework refuses. An unregistered
/// `JOURNAL_AUDIT_MISMATCH` (the host forgot the catalogue) arrives here as `UnknownId` and is
/// reported as this typed failure.
pub fn apply<S: AuditSink>(
    engine: &mut ConditionEngine<S>,
    result: &TenantResult,
) -> Result<(), AuditJobError> {
    let subject = result.tenant.as_str();
    let now = Millis::new(u64::try_from(result.finished_millis).unwrap_or(0));
    match &result.outcome {
        TenantOutcome::Clean => {
            engine
                .observe_cleared(JOURNAL_AUDIT_MISMATCH, subject, now)
                .map_err(|e| failed(&e))?;
            engine
                .observe_cleared(JOURNAL_AUDIT_FAILED, subject, now)
                .map_err(|e| failed(&e))?;
        }
        TenantOutcome::Mismatch => {
            let (record, difference) = first_difference(result);
            let params = ConditionParams::new()
                .with("first_differing_record", record)
                .with("difference", difference);
            engine
                .raise(JOURNAL_AUDIT_MISMATCH, subject, params, now)
                .map_err(|e| failed(&e))?;
            engine
                .observe_cleared(JOURNAL_AUDIT_FAILED, subject, now)
                .map_err(|e| failed(&e))?;
        }
        TenantOutcome::Failed(FailureInfo { stage, code }) => {
            let params = ConditionParams::new()
                .with("stage", *stage)
                .with("code", *code);
            engine
                .raise(JOURNAL_AUDIT_FAILED, subject, params, now)
                .map_err(|e| failed(&e))?;
        }
    }
    Ok(())
}
