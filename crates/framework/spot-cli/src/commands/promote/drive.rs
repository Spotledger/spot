//! Runs a recorded promotion through `spot-promotion`'s service, on a plain thread.
//!
//! The blocking approval store refuses a thread inside a tokio runtime, so the whole run happens
//! on a plain thread; the service's futures are driven by [`wait`] (they only await ports that
//! answer at once) and every port that needs the database reaches the runtime through its handle.
//!
//! The run replays the operator's intent (see `log.rs`): request, plan (with the conflicts of the
//! hand-over's evaluation as plan issues), each recorded approval under its identity and time,
//! rehearsal on a clone of the target, the freshness guard, apply and verify.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use spot_audit_chain::AuditRecord;
use spot_core::Clock;
use spot_custom_bundle::Bundle;
use spot_environments::EnvRef;
use spot_migration_approval::{Act, ApprovalError, ApprovalStore, AuditSink, Identity};
use spot_migration_audit::RunId;
use spot_migration_ops_pg::PgMigrationStore;
use spot_promotion::{
    CustomBundleApplier, MemoryPromotionStore, PlanIssue, PromotionAudit, PromotionError,
    PromotionPolicy, PromotionService, PromotionState,
};

use super::clone::CloneRunner;
use super::log::Record;
use super::target::LiveTarget;
use super::{APPROVALS_MISSING, check_rehearsal};

/// Everything one apply run needs.
pub struct Job {
    /// The promotion as recorded.
    pub record: Record,
    /// The bundle (opened from the kept hand-over file).
    pub bundle: Bundle,
    /// The conflicts of the evaluation, as plan issues.
    pub issues: Vec<PlanIssue>,
    /// The source address.
    pub from_env: EnvRef,
    /// The target address.
    pub to_env: EnvRef,
    /// The target site, writing through to its records.
    pub target: LiveTarget,
    /// The rehearsal on a clone of the target.
    pub runner: CloneRunner,
    /// The target's PostgreSQL store (approvals and the plan authors).
    pub store: Arc<PgMigrationStore>,
    /// Both chains.
    pub audit: Arc<dyn PromotionAudit>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// The longest a rehearsal may be older than the apply, in seconds.
    pub max_age_secs: i64,
}

/// How the run ended.
pub enum Outcome {
    /// Verified: the lines to print.
    Done(Vec<String>),
    /// Refused before anything was written.
    Refused(String),
    /// Failed after work started.
    Failed(String),
}

struct Unpark(std::thread::Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` to completion on the current (plain) thread.
pub fn wait<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// The approval service's own records go to the tracing target of the migration audit; the
/// chains get every approval through the promotion audit.
struct TracingApprovalAudit;

impl AuditSink for TracingApprovalAudit {
    fn record(&self, record: AuditRecord) -> Result<(), ApprovalError> {
        tracing::info!(
            target: "spot::audit::migration",
            kind = %record.kind,
            actor = %record.actor,
            at = record.at,
            "promotion approval action"
        );
        Ok(())
    }
}

fn micros(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
}

/// Whether the error is a refusal (nothing was written) or a failure.
fn refused(error: &PromotionError) -> bool {
    matches!(
        error,
        PromotionError::BadId { .. }
            | PromotionError::NotFound { .. }
            | PromotionError::Duplicate { .. }
            | PromotionError::BadState { .. }
            | PromotionError::BundleMismatch { .. }
            | PromotionError::PlanFailed { .. }
            | PromotionError::Blocked { .. }
            | PromotionError::NothingToPromote
            | PromotionError::PlanChanged { .. }
            | PromotionError::ApprovalRequired { .. }
            | PromotionError::ApprovalRefused { .. }
            | PromotionError::RehearsalRequired { .. }
            | PromotionError::PolicyInvalid { .. }
            | PromotionError::BadRoute { .. }
            | PromotionError::EnvironmentMismatch { .. }
    )
}

fn outcome_of(error: &PromotionError) -> Outcome {
    if refused(error) {
        Outcome::Refused(error.to_string())
    } else {
        Outcome::Failed(error.to_string())
    }
}

/// Runs the promotion. Call it on a plain thread.
pub fn run(mut job: Job) -> Outcome {
    if let Err(error) = job.store.ensure_present() {
        return Outcome::Failed(format!("the migration stores are not ready: {error}"));
    }
    let approvals: Arc<dyn ApprovalStore> = job.store.clone();
    let service = match PromotionService::new(
        Arc::new(MemoryPromotionStore::new()),
        approvals,
        Arc::new(TracingApprovalAudit),
        Arc::clone(&job.audit),
        Arc::new(CustomBundleApplier::with_builtin_sections()),
        PromotionPolicy::default(),
    ) {
        Ok(service) => service,
        Err(error) => return outcome_of(&error),
    };
    let record = job.record.clone();
    let Ok(id) = RunId::new(record.id.clone()) else {
        return Outcome::Refused("promote.bad_id: the promotion id is not valid".to_owned());
    };
    let actor = format!("user:{}", record.requester);
    let at = record.requested_at;
    let mut lines = Vec::new();

    if let Err(error) = service.request(
        &record.id,
        &job.bundle,
        job.from_env.clone(),
        job.to_env.clone(),
        &record.requester,
        at,
    ) {
        return outcome_of(&error);
    }
    let mut promotion = match wait(service.plan(
        &id,
        &job.bundle,
        &job.target,
        job.issues.clone(),
        &actor,
        at,
    )) {
        Ok(promotion) => promotion,
        Err(error) => return outcome_of(&error),
    };
    lines.push(format!(
        "planned {} change(s) for {}",
        promotion.plan().map_or(0, |plan| plan.apply_changes()),
        job.to_env.role.name()
    ));

    for approval in &record.approvals {
        if promotion.state() != PromotionState::Planned {
            break;
        }
        let roles: Vec<&str> = approval.roles.iter().map(String::as_str).collect();
        let act = Act {
            identity: Identity::new(approval.approver.clone(), &roles),
            role: &approval.role,
            comment: &approval.comment,
            signature: None,
            now: approval.at,
        };
        promotion = match wait(service.approve(&id, &act)) {
            Ok(promotion) => promotion,
            Err(error) => return outcome_of(&error),
        };
    }
    match promotion.state() {
        PromotionState::Approved => lines.push("approved".to_owned()),
        PromotionState::Rejected => {
            return Outcome::Refused("promote.rejected: an approver rejected the plan".to_owned());
        }
        _ => {
            return Outcome::Refused(format!(
                "{APPROVALS_MISSING}: the recorded approvals do not satisfy the {} policy",
                job.to_env.role.name()
            ));
        }
    }

    let now = micros(job.clock.as_ref());
    if let Err(error) =
        wait(service.rehearse(&id, &job.bundle, &job.target, &job.runner, &actor, now))
    {
        return outcome_of(&error);
    }
    let rehearsed_at = job.clock.now_utc().unix_timestamp();
    lines.push("rehearsed on a clone of the target; the clone was dropped".to_owned());

    let now_secs = job.clock.now_utc().unix_timestamp();
    if let Err(message) = check_rehearsal(
        job.to_env.role.name(),
        Some(rehearsed_at),
        now_secs,
        job.max_age_secs,
    ) {
        return Outcome::Refused(message);
    }
    let now = micros(job.clock.as_ref());
    match wait(service.advance(&id, &job.bundle, &mut job.target, &actor, now)) {
        Ok(done) => {
            lines.push(format!(
                "applied {} change(s) and verified",
                done.apply().map_or(0, |evidence| evidence.steps)
            ));
            Outcome::Done(lines)
        }
        Err(error) => outcome_of(&error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_drives_a_ready_future() {
        assert_eq!(wait(async { 7 }), 7);
    }

    #[test]
    fn a_refusal_is_not_a_failure() {
        assert!(refused(&PromotionError::NothingToPromote));
        assert!(!refused(&PromotionError::ApplyFailed {
            message: String::new()
        }));
        assert!(matches!(
            outcome_of(&PromotionError::VerifyFailed { findings: 1 }),
            Outcome::Failed(_)
        ));
    }
}
