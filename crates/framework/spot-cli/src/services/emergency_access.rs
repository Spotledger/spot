//! Emergency access, the firefighter pattern, hosted (WP-IAM-08.T14, ADR-070 section 9).
//!
//! `spot-iam-questions` holds the whole life of an emergency grant as a pure state machine
//! (`EmergencyGrant`). This module makes it a feature of a running site: a user raises a request
//! with a reason, the approver decides it on the process engine, an approved grant becomes a
//! time-boxed assignment with source `emergency`, the session's actions are recorded, the grant
//! ends by itself at `valid_to`, and a controller who is not the user reviews it (a second,
//! distinct check when the controller is also the approver), escalating at the deadline.
//!
//! - **Off by default.** `[services.emergency_access] enabled = false`. A site that turns it on
//!   must name the roles that may be requested (`emergency_roles`), who may approve
//!   (`approvers`), who reviews (`controllers`) and where an unanswered approval or review
//!   escalates (`escalate_to`); an empty or malformed list, a duration outside 1 to 1440 minutes
//!   or an escalation target that is blank or has a control character refuses to start the site
//!   (`emergency_access_host.settings_invalid`). The other settings are
//!   `max_duration_minutes` (60), `approval_sla_minutes` (60), `review_within_hours` (24),
//!   `escalate_after_hours` (24), `max_escalations` (3), `second_check` (true),
//!   `allow_sod_override` (false) and `sweep_seconds` (5).
//! - **The approval is a Process** launched through the process host
//!   ([`seams::EmergencyLauncher`]), keyed by the grant id, never a private workflow. The approver
//!   decides through [`EmergencyAccess::approve`] / `reject` (the Decision node's completion and
//!   the `spot access emergency approve|reject` commands call it). Approving twice grants once.
//! - **Activation goes through the governed path.** The segregation-of-duties gate
//!   (`GrantGuard::recheck_approved`) runs first; a conflict refuses the grant unless the policy
//!   allows an override and the approver submitted a reason and a mitigating control, which the
//!   gate records. With no gate, no journal or no process host the module reports itself degraded
//!   and grants nothing.
//! - **One store, one record per grant** ([`store`]): a restart resumes an active grant, a pending
//!   approval and an open review. An unknown version, a record under the wrong key or tenant, or a
//!   digest that does not match refuses to load.
//! - **The sweep** ([`flow::Emergency::sweep`]) advances every grant: launches waiting approvals,
//!   expires lapsed ones, activates approved ones, ends sessions at `valid_to` (the permission
//!   engine already refuses the assignment after it, the sweep ends the record and the assignment
//!   and opens the review), archives the recording and escalates due reviews. It runs on its own
//!   loop and as the job `emergency_access_sweep`; it is idempotent.
//! - **Every state change** appends a `ChangeEvent` to the access-change journal (the "what
//!   changed" question reads it) and, when an assignment is written or ended, publishes the
//!   `identity` topic event in the same transaction.
//!
//! Level: optional. Guide: `docs/running-spot/emergency_access.md`.

use std::sync::Arc;
use std::time::Duration;

use spot_bpm_engine::InMemoryTimerPort;
use spot_http::api::service::BoxFuture;
use spot_iam_governance::OverrideSubmission;
use spot_jobs::worker::HandlerResult;
use tokio::sync::watch;

#[path = "emergency_access/actions.rs"]
pub mod actions;
#[path = "emergency_access/flow.rs"]
pub mod flow;
#[path = "emergency_access/model.rs"]
pub mod model;
#[path = "emergency_access/ops.rs"]
pub mod ops;
#[path = "emergency_access/seams.rs"]
pub mod seams;
#[path = "emergency_access/store.rs"]
pub mod store;
#[path = "emergency_access/sweep.rs"]
mod sweep;

pub use flow::{Emergency, Parts, RefusalLog, SweepReport};
pub use model::{
    CODE_APPROVAL_UNAVAILABLE, CODE_APPROVER_NOT_ALLOWED, CODE_AUDIT_UNAVAILABLE, CODE_CONFLICT,
    CODE_DISABLED, CODE_NOT_FOUND, CODE_NOT_PERMITTED, CODE_NOT_SELF, CODE_OVERRIDE_REQUIRED,
    CODE_RECORDING_UNAVAILABLE, CODE_REVIEW_UNASSIGNED, CODE_SESSION_FULL, CODE_SETTINGS_INVALID,
    CODE_STATE_INVALID, CODE_STATE_VERSION, CODE_UNAVAILABLE, CODE_WRITE_FAILED, EmergencyError,
    GrantView, NewRequest, Policy, RecordingStatus, Settings,
};

use crate::services::access_questions_model::history::{ChainJournal, ChangeJournal};
use crate::services::break_glass::BreakGlassPort;
use crate::services::governance::{GovernanceStore, GrantGuard};
use crate::services::process::ProcessHost;
use crate::services::roles::RoleChangeSink;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// After the break-glass archive (1170): it reads that port and the governance gate (1070) and
/// the process host (1100).
pub const ORDER: u32 = 1180;

/// The job name the sweep is registered under.
pub const SWEEP_JOB: &str = "emergency_access_sweep";

/// The in-process port: every call acts as the user the caller passes (an authenticated user id,
/// never a name from a request body), and goes through the same policy as every other path.
pub trait EmergencyAccess: Send + Sync {
    /// Raises a request for `by`.
    fn request<'a>(
        &'a self,
        by: &'a str,
        new: NewRequest,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>>;
    /// Approves grant `id` as `by`.
    fn approve<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        override_: Option<OverrideSubmission>,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>>;
    /// Rejects grant `id` as `by`.
    fn reject<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>>;
    /// Ends the active session of grant `id` as `by`.
    fn end<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>>;
    /// Records a controller's check of grant `id`.
    fn review<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        confirm: bool,
        note: &'a str,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>>;
    /// One grant.
    fn status<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>>;
    /// Every grant, and how many stored records could not be read.
    fn list<'a>(
        &'a self,
    ) -> BoxFuture<'a, std::result::Result<(Vec<GrantView>, usize), EmergencyError>>;
    /// Records one action of `by`'s session under grant `id`.
    fn record_action<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        action: &'a str,
        detail: Option<&'a str>,
    ) -> BoxFuture<'a, std::result::Result<(), EmergencyError>>;
}

impl EmergencyAccess for Emergency {
    fn request<'a>(
        &'a self,
        by: &'a str,
        new: NewRequest,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>> {
        Box::pin(Emergency::request(self, by, new))
    }
    fn approve<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        override_: Option<OverrideSubmission>,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>> {
        Box::pin(Emergency::approve(self, id, by, override_))
    }
    fn reject<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>> {
        Box::pin(Emergency::reject(self, id, by, reason))
    }
    fn end<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>> {
        Box::pin(Emergency::end(self, id, by))
    }
    fn review<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        confirm: bool,
        note: &'a str,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>> {
        Box::pin(Emergency::review(self, id, by, confirm, note))
    }
    fn status<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<GrantView, EmergencyError>> {
        Box::pin(Emergency::status(self, id))
    }
    fn list<'a>(
        &'a self,
    ) -> BoxFuture<'a, std::result::Result<(Vec<GrantView>, usize), EmergencyError>> {
        Box::pin(Emergency::list(self))
    }
    fn record_action<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        action: &'a str,
        detail: Option<&'a str>,
    ) -> BoxFuture<'a, std::result::Result<(), EmergencyError>> {
        Box::pin(Emergency::record_action(self, id, by, action, detail))
    }
}

/// The service: its check is the module's readiness, its loop is the sweep.
struct EmergencyService {
    emergency: Arc<Emergency>,
    recorder: Arc<actions::ActionRecorder<Arc<Emergency>>>,
    every: Duration,
}

impl Service for EmergencyService {
    fn name(&self) -> &'static str {
        "emergency_access"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let (gate, launcher, journal) = self.emergency.wired();
            let mut missing = Vec::new();
            if !gate {
                missing.push("the governance gate");
            }
            if !launcher {
                missing.push("the process host");
            }
            if !journal {
                missing.push("the access-change journal");
            }
            if !missing.is_empty() {
                return Err(format!(
                    "{CODE_UNAVAILABLE}: emergency access cannot grant or start approvals without {}",
                    missing.join(", ")
                ));
            }
            self.emergency
                .probe()
                .await
                .map_err(|e| format!("{}: the emergency grants cannot be read", e.code))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                let report = self.emergency.sweep().await;
                // Actions that could not be recorded when they happened are retried here.
                self.recorder.retry().await;
                if report.failed > 0 || report.unreadable > 0 {
                    tracing::warn!(
                        failed = report.failed,
                        unreadable = report.unreadable,
                        "the emergency access sweep had failures"
                    );
                }
                tokio::select! {
                    _ = stop.changed() => return Ok(()),
                    () = tokio::time::sleep(self.every) => {}
                }
            }
        })
    }
}

/// Joins the site when `[services.emergency_access] enabled = true`.
///
/// # Errors
///
/// A setting that does not decode or is out of range: the site does not start with it.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings = ctx.settings_or_default::<Settings>("emergency_access")?;
    if !settings.enabled {
        return Ok(());
    }
    let policy = settings.validate().map_err(|name| {
        Error(format!(
            "{CODE_SETTINGS_INVALID}: services.emergency_access.{name} is missing or out of range"
        ))
    })?;
    let (site, pool, backend, catalog) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            env.backend.pool(),
            env.backend.clone(),
            env.permission.catalog.clone(),
        )
    };
    let kv = store::PgKv::new(pool.clone(), &site);
    kv.prepare().await.map_err(|_| {
        Error(format!(
            "{CODE_UNAVAILABLE}: the emergency access table could not be prepared"
        ))
    })?;
    let kv: Arc<dyn GovernanceStore> = Arc::new(kv);
    let clock = Arc::clone(ctx.clock());
    let launcher: Option<Arc<dyn spot_iam_governance::ProcessLauncher + Send + Sync>> =
        ctx.port::<ProcessHost>().map(|host| {
            Arc::new(seams::EmergencyLauncher::new(host, Arc::clone(&clock)))
                as Arc<dyn spot_iam_governance::ProcessLauncher + Send + Sync>
        });
    let recording: Arc<dyn seams::Recording> = match ctx.port::<dyn BreakGlassPort>() {
        Some(port) => Arc::new(seams::BreakGlassRecording::new(port, &site)),
        None => Arc::new(seams::NoRecording),
    };
    let journal: Arc<dyn ChangeJournal> = Arc::new(ChainJournal::new(
        spot_audit_store::PgAuditStore::new(pool),
        &site,
    ));
    let rights: Option<Arc<dyn RoleChangeSink>> = ctx.port::<dyn RoleChangeSink>();
    let every = Duration::from_secs(policy.sweep_seconds);
    let emergency = Arc::new(Emergency::new(Parts {
        site: site.clone(),
        clock,
        policy,
        store: store::GrantStore::new(kv, &site),
        assignments: Arc::new(seams::PgAssignments::new(&site, backend, catalog)),
        guard: ctx.port::<dyn GrantGuard>(),
        launcher,
        journal: Some(journal),
        recording,
        ids: Arc::new(seams::RecordIds),
        timers: Arc::new(InMemoryTimerPort::new()),
        rights,
        refusals: Arc::new(RefusalLog::default()),
    }));
    let port: Arc<dyn EmergencyAccess> = Arc::clone(&emergency) as Arc<dyn EmergencyAccess>;
    ctx.provide(port)?;
    let for_job = Arc::clone(&emergency);
    ctx.job_handlers().register(SWEEP_JOB, move |_job| {
        let emergency = Arc::clone(&for_job);
        Box::pin(async move {
            let report = emergency.sweep().await;
            Ok(if report.failed == 0 && report.unreadable == 0 {
                HandlerResult::Ok
            } else {
                HandlerResult::Retry("the emergency access sweep had failures".to_owned())
            })
        })
    });
    let recorder = Arc::new(actions::ActionRecorder::new(Arc::clone(&emergency)));
    let door: Arc<dyn actions::EmergencyActions> =
        Arc::clone(&recorder) as Arc<dyn actions::EmergencyActions>;
    ctx.provide(door)?;
    ctx.add(Arc::new(EmergencyService {
        emergency,
        recorder,
        every,
    }));
    Ok(())
}
