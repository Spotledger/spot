//! Grant approval on the process engine (WP-IAM-08.T11): the `ProcessLauncher` of
//! `spot-iam-governance` over the hosted process engine, set into the governance module's
//! [`LauncherSlot`] once the process host exists.
//!
//! - **Launch**: publishes the approval process the crate built (`approval_process`) into the engine and
//!   starts a Case of it, keyed by the grant request id (`start_run`'s business key), so a launch retried
//!   after a crash resumes the same Case and never starts a second one. The case data is the request id
//!   only; the requester, subject, role, scope and reason are already in the process version's nodes
//!   (the crate's spec), and nothing else is passed.
//! - **Fail closed**: with no process host (not configured, or its engine not running) the slot stays
//!   empty and the gate refuses a grant that needs approval (`governance_approval_host.unavailable`); a
//!   launch error is the same answer. Nothing is auto-approved at any timeout (the process escalates).
//! - **The Decision node's completion** is the only path to a grant, and [`GrantCompletion::complete`]
//!   is that path (WP-IAM-08.T12), provided as a port for the task inbox completion to call. In order:
//!   it takes a [`StepUpVerified`] witness (the caller checked the approver's recent step-up, the way
//!   `auth_step_up` does; nothing else can build one by accident), decides the pending approval as the
//!   authenticated completing user (the named approver or the escalation target only, the grantee
//!   excluded), runs the gate again without the approval step (an approval never overrides a conflict),
//!   asks the [`GrantExecutor`] to make the grant under the original request id, and only then marks it
//!   granted. The request id makes every step idempotent: a retry after a crash resumes from the stored
//!   state, and a repeat of a finished completion is [`Completion::AlreadyGranted`], executing nothing.
//!
//! Settings `[services.governance_approval]`: `enabled` (true). Level: optional.

use std::sync::Arc;

use serde::Deserialize;
use spot_bpm_model::ProcessVersion;
use spot_core::Clock;
use spot_iam_governance::approval::LaunchReceipt;
use spot_iam_governance::{GovError, GrantRequestId, ProcessLauncher};

use crate::Result;
use crate::services::governance::{
    ApprovalResult, GrantExecutor, GrantGuard, GrantOutcome, LauncherSlot,
};
use crate::services::process::ProcessHost;
use crate::services::{Idle, Level, SiteCtx};

/// After the process host (1100) and the task inbox (1110).
pub const ORDER: u32 = 1120;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Starts approval Cases on the hosted engine.
pub struct HostLauncher {
    host: Arc<ProcessHost>,
    clock: Arc<dyn Clock>,
}

impl ProcessLauncher for HostLauncher {
    fn launch(
        &self,
        process: &ProcessVersion,
        request: &GrantRequestId,
    ) -> std::result::Result<LaunchReceipt, GovError> {
        let failed = |detail: &str| GovError::ApprovalProcess {
            detail: detail.to_owned(),
        };
        let engine = self
            .host
            .engine()
            .map_err(|_| failed("the process engine is not running"))?;
        engine
            .publish(process)
            .map_err(|_| failed("the approval process did not compile"))?;
        let host = Arc::clone(&self.host);
        let now = self.clock.now_utc();
        let key = format!("grant-approval:{}", request.as_str());
        let name = process.name.clone();
        let input = serde_json::json!({"request": request.as_str()});
        // The trait is synchronous; the caller runs it under `block_in_place`.
        let case = tokio::runtime::Handle::current()
            .block_on(async move { host.start_run(&name, None, input, None, &key, now).await })
            .map_err(|_| failed("the approval case could not be started"))?;
        Ok(LaunchReceipt {
            case_id: case.to_string(),
            process_hash: process.content_hash().to_string(),
        })
    }
}

/// The witness that the approver's step-up was checked, as the identity layer does for `auth_step_up`.
/// The completion path takes one so a call without that check does not type-check; building one is the
/// caller's statement that the check passed.
#[derive(Debug)]
pub struct StepUpVerified(());

impl StepUpVerified {
    /// The caller checked that the completing user's step-up is recent.
    #[must_use]
    pub fn confirmed_by_caller() -> StepUpVerified {
        StepUpVerified(())
    }
}

/// How a completion ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Completion {
    /// Approved, re-checked, and the grant was made (once).
    Granted,
    /// The grant of this request was already made: nothing was executed.
    AlreadyGranted,
    /// The approver rejected it (or it expired): nothing was granted.
    Rejected,
    /// Approved, but the gate refused the grant now (a conflict arose since the request); the stable
    /// codes. The approval stays recorded; completing again after the conflict is resolved resumes it.
    Refused(Vec<String>),
}

/// The completion of a grant approval: the guard and the executor that makes the grant.
pub struct GrantCompletion {
    guard: Arc<dyn GrantGuard>,
    executor: Option<Arc<dyn GrantExecutor>>,
}

impl GrantCompletion {
    /// A completion over `guard`; without an `executor` an approved grant cannot be made and the
    /// completion fails (the approval stays recorded).
    #[must_use]
    pub fn new(guard: Arc<dyn GrantGuard>, executor: Option<Arc<dyn GrantExecutor>>) -> Self {
        Self { guard, executor }
    }

    /// Completes the Decision node of `request` as the authenticated user `by`.
    ///
    /// # Errors
    /// The stable code of why nothing was decided or granted: the approval refusals
    /// (`iam_governance.not_the_approver`, `approver_is_grantee`, `approval_not_pending`), a gate or store
    /// that is unavailable, or an executor that failed. Nothing is granted on any of them.
    pub async fn complete(
        &self,
        _step_up: StepUpVerified,
        request: &str,
        by: &str,
        approve: bool,
        reason: Option<String>,
    ) -> std::result::Result<Completion, String> {
        let original = match self
            .guard
            .decide_approval(request, by, approve, reason)
            .await
        {
            ApprovalResult::Approved(original) => *original,
            ApprovalResult::Ended => return Ok(Completion::Rejected),
            ApprovalResult::AlreadyGranted => return Ok(Completion::AlreadyGranted),
            ApprovalResult::Refused(code) => return Err(code),
        };
        match self.guard.recheck_approved(original.clone()).await {
            GrantOutcome::Allowed => {}
            GrantOutcome::Blocked(codes) | GrantOutcome::NeedsOverride(codes) => {
                return Ok(Completion::Refused(codes));
            }
            GrantOutcome::Unavailable(code) => return Err(code.to_owned()),
            // The approval step is not asked for twice, so the gate cannot answer this; refuse anyway.
            GrantOutcome::Pending { .. } => return Err("iam_governance.request_invalid".to_owned()),
        }
        let Some(executor) = &self.executor else {
            return Err("governance_approval_host.unavailable".to_owned());
        };
        executor.execute(&original, by).await?;
        // A grant made but not recorded is safe to repeat (the executor is idempotent): the approval
        // stays `approved` and the next completion resumes here.
        if self.guard.mark_granted(request).await {
            Ok(Completion::Granted)
        } else {
            Err("governance_host.state_unavailable".to_owned())
        }
    }
}

/// Joins the site unless disabled, once the governance module and the process host exist.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("governance_approval")?;
    if !settings.enabled {
        return Ok(());
    }
    let slot = ctx.port::<LauncherSlot>();
    let host = ctx.port::<ProcessHost>();
    let guard = ctx.port::<dyn GrantGuard>();
    let executor = ctx.port::<dyn GrantExecutor>();
    let ready = slot.is_some() && host.is_some();
    if let (Some(slot), Some(host)) = (slot, host) {
        slot.set(Arc::new(HostLauncher {
            host,
            clock: Arc::clone(ctx.clock()),
        }));
    }
    if let Some(guard) = guard {
        ctx.provide(Arc::new(GrantCompletion::new(guard, executor)))?;
    }
    if ctx.assembling() {
        ctx.add(Idle::new("governance_approval", Level::Optional, move || {
            Box::pin(async move {
                if ready {
                    Ok(())
                } else {
                    Err("grant approval is unavailable (no governance gate or no process host): grants that need approval are refused".to_owned())
                }
            })
        }));
    }
    Ok(())
}
