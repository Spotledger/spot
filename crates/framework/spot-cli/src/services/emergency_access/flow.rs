//! The life of an emergency grant on a running site: request, approval, the time-boxed
//! assignment, the recorded session, the end, the review and the escalation. The rules are the
//! pure state machine of `spot-iam-questions` (`EmergencyGrant`); this module is what makes them
//! durable and safe on a server:
//!
//! - **Every step is a transition of one stored record**, written with a compare-and-swap, so two
//!   nodes (the sweep, a command, a request handler) never both advance one grant, and a restart
//!   resumes from the record: an active grant, a pending approval and an open review all survive.
//! - **Effects are ordered so a crash repeats, never loses or doubles:** the access-change event is
//!   written first (a duplicate is possible after a crash, a lost one is not); the assignment write
//!   is idempotent by a record id derived from the grant; the activation instant is fixed in the
//!   record *before* the assignment is written, so a retry writes the same window; the record is
//!   committed last.
//! - **No grant without its audit:** the journal write and the segregation-of-duties gate come
//!   before the assignment; a journal that cannot be written, or a gate that cannot answer, grants
//!   nothing.
//! - **The end is enforced twice:** the assignment carries `valid_to`, which the permission engine
//!   checks on every request, and the sweep ends the record and the assignment at the same
//!   instant. The sweep is cleanup, not the control.

use std::sync::{Arc, Mutex};

use spot_bpm_engine::TimerPort;
use spot_break_glass::MemoryRecorder;
use spot_core::Clock;
use spot_iam_governance::{
    ApprovalDecision, ChangeActor, ChangeEvent, ChangeOp, GrantScope, OverrideSubmission,
    ProcessLauncher,
};
use spot_iam_questions::{EmergencyAuditor, EmergencyGrant, EmergencyRefusal, EmergencyRequest};
use spot_iam_roles::Source;
use time::OffsetDateTime;

use super::model::{
    Body, CODE_APPROVER_NOT_ALLOWED, CODE_AUDIT_UNAVAILABLE, CODE_CONFLICT, CODE_NOT_PERMITTED,
    CODE_NOT_SELF, CODE_OVERRIDE_REQUIRED, CODE_SESSION_FULL, CODE_UNAVAILABLE, EmergencyError,
    GrantView, MAX_ACTIONS, MAX_TEXT, NewRequest, Policy, Stored,
};
use super::seams::{Assignments, Ids, Recording};
use super::store::{GrantStore, Loaded};
use crate::services::access_questions_model::history::{ChangeJournal, record};
use crate::services::governance::GrantGuard;
use crate::services::roles::RoleChangeSink;

pub(super) const AUDIT: &str = "spot::audit::emergency_access";
pub(super) const RETRIES: usize = 6;

/// The refusals of requests and decisions the state machine reports, kept for the status and the
/// tests, and written to the audit target as they arrive (codes only).
#[derive(Default)]
pub struct RefusalLog {
    rows: Mutex<Vec<EmergencyRefusal>>,
}

impl RefusalLog {
    /// The refusals recorded so far, oldest first.
    #[must_use]
    pub fn rows(&self) -> Vec<EmergencyRefusal> {
        self.rows.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl EmergencyAuditor for RefusalLog {
    fn record(&self, refusal: &EmergencyRefusal) {
        tracing::info!(
            target: AUDIT,
            grant = %refusal.grant,
            by = %refusal.by,
            attempted = %refusal.attempted,
            code = %refusal.code,
            "emergency access refused"
        );
        if let Ok(mut rows) = self.rows.lock() {
            rows.push(refusal.clone());
        }
    }
}

/// What a sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Transitions made.
    pub advanced: usize,
    /// Grants that failed a step (the codes are in the audit target).
    pub failed: usize,
    /// Stored records that could not be read.
    pub unreadable: usize,
}

/// Everything an [`Emergency`] is built from.
pub struct Parts {
    /// The site.
    pub site: String,
    /// The injected clock: nothing here reads the wall clock.
    pub clock: Arc<dyn Clock>,
    /// The validated settings.
    pub policy: Policy,
    /// The grants.
    pub store: GrantStore,
    /// The assignment records.
    pub assignments: Arc<dyn Assignments>,
    /// The segregation-of-duties gate (the one every grant goes through). Without it nothing is
    /// activated.
    pub guard: Option<Arc<dyn GrantGuard>>,
    /// Starts the approval Case. Without it a request waits.
    pub launcher: Option<Arc<dyn ProcessLauncher + Send + Sync>>,
    /// The access-change journal. Without it nothing is changed.
    pub journal: Option<Arc<dyn ChangeJournal>>,
    /// The archive of ended sessions.
    pub recording: Arc<dyn Recording>,
    /// New grant ids.
    pub ids: Arc<dyn Ids>,
    /// The review escalation timers.
    pub timers: Arc<dyn TimerPort>,
    /// Told when access changed, so the role snapshot is rebuilt now.
    pub rights: Option<Arc<dyn RoleChangeSink>>,
    /// The refusals.
    pub refusals: Arc<RefusalLog>,
}

/// Emergency access of one site.
pub struct Emergency {
    pub(super) p: Parts,
}

pub(super) fn instant(at: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(at).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The stable code of a state-machine refusal. An approval refusal keeps the governance code
/// (`iam_governance.not_the_approver`, ...) the crate puts at the start of its detail, so a caller
/// can tell the grantee from a stranger.
pub(super) fn qcode(error: &spot_iam_questions::QuestionsError) -> EmergencyError {
    if let spot_iam_questions::QuestionsError::Approval { detail, .. } = error {
        for code in [
            spot_iam_governance::codes::APPROVER_IS_GRANTEE,
            spot_iam_governance::codes::NOT_THE_APPROVER,
            spot_iam_governance::codes::APPROVAL_NOT_PENDING,
            spot_iam_governance::codes::APPROVAL_PROCESS,
        ] {
            if detail.starts_with(code) {
                return EmergencyError::new(code);
            }
        }
    }
    EmergencyError::new(error.code())
}

fn trimmed(text: &str) -> Result<&str, EmergencyError> {
    let t = text.trim();
    if t.is_empty() || t.len() > MAX_TEXT || t.chars().any(|c| c.is_control() && c != '\n') {
        return Err(EmergencyError::new(
            spot_iam_questions::codes::REQUEST_INVALID,
        ));
    }
    Ok(t)
}

impl Emergency {
    /// Emergency access over `parts`.
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        Self { p: parts }
    }

    /// The refusals recorded so far.
    #[must_use]
    pub fn refusals(&self) -> Vec<EmergencyRefusal> {
        self.p.refusals.rows()
    }

    /// Which parts are wired: `(gate, launcher, journal)`.
    #[must_use]
    pub fn wired(&self) -> (bool, bool, bool) {
        (
            self.p.guard.is_some(),
            self.p.launcher.is_some(),
            self.p.journal.is_some(),
        )
    }

    /// Whether the store can be read now.
    ///
    /// # Errors
    ///
    /// `unavailable`.
    pub async fn probe(&self) -> Result<(), EmergencyError> {
        self.p.store.all().await.map(|_| ())
    }

    pub(super) fn now(&self) -> i64 {
        self.p.clock.now_utc().unix_timestamp()
    }

    /// Writes the audit line of a refusal and returns the error for the caller.
    pub(super) fn refuse(
        &self,
        op: &str,
        by: &str,
        grant: &str,
        error: EmergencyError,
    ) -> EmergencyError {
        tracing::info!(
            target: AUDIT,
            site = %self.p.site,
            op,
            by,
            grant,
            code = error.code,
            "emergency access refused"
        );
        error
    }

    pub(super) fn event(
        &self,
        body: &Body,
        op: ChangeOp,
        actor: ChangeActor,
        at: i64,
        reason: &str,
    ) -> ChangeEvent {
        ChangeEvent::grant(
            &format!("emergency:{}", body.id),
            &body.grant.request.user,
            body.grant.request.role.as_str(),
            Source::Emergency,
            op,
            actor,
            at,
        )
        .with_reason(reason)
    }

    pub(super) async fn emit(&self, event: &ChangeEvent) -> Result<(), EmergencyError> {
        let journal = self
            .p
            .journal
            .as_ref()
            .ok_or(EmergencyError::new(CODE_AUDIT_UNAVAILABLE))?;
        record(journal.as_ref(), event)
            .await
            .map(|_| ())
            .map_err(|_| EmergencyError::new(CODE_AUDIT_UNAVAILABLE))
    }

    pub(super) fn changed(&self) {
        if let Some(sink) = &self.p.rights {
            sink.changed();
        }
    }

    /// Writes `body` over `loaded`; `Ok(false)` when another writer got there first.
    pub(super) async fn commit(
        &self,
        loaded: &Loaded,
        body: &Body,
    ) -> Result<bool, EmergencyError> {
        self.p.store.put(Some(&loaded.raw), body).await
    }

    /// Runs `change` on the current record, writes its event, and commits, retrying on a lost
    /// race. `change` must be a pure function of the record (it runs again after a lost race).
    pub(super) async fn transition<F>(
        &self,
        id: &str,
        mut change: F,
    ) -> Result<Body, EmergencyError>
    where
        F: FnMut(&mut Body) -> Result<Option<ChangeEvent>, EmergencyError>,
    {
        for _ in 0..RETRIES {
            let loaded = self.p.store.load(id).await?;
            let mut body = loaded.body.clone();
            let event = change(&mut body)?;
            if let Some(event) = &event {
                self.emit(event).await?;
            }
            if self.commit(&loaded, &body).await? {
                return Ok(body);
            }
        }
        Err(EmergencyError::new(CODE_UNAVAILABLE))
    }

    // ------------------------------------------------------------------------------------------
    // The calls
    // ------------------------------------------------------------------------------------------

    /// Raises a request for the caller (`by`, a user id).
    ///
    /// # Errors
    ///
    /// The policy's refusals (duration, role, approver, reason) with their stable code; `audit_unavailable`
    /// or `unavailable` when nothing could be written. A refused request is not created.
    pub async fn request(&self, by: &str, new: NewRequest) -> Result<GrantView, EmergencyError> {
        let at = self.now();
        let id = self.p.ids.next();
        let result = self.request_inner(by, new, &id, at).await;
        match result {
            Ok(view) => {
                // A refused advance leaves the request waiting; the sweep tries again.
                let _ = self.advance(&id).await;
                match self.p.store.load(&id).await {
                    Ok(loaded) => Ok(GrantView::of(&loaded.body)),
                    Err(_) => Ok(view),
                }
            }
            Err(error) => Err(self.refuse("request", by, &id, error)),
        }
    }

    async fn request_inner(
        &self,
        by: &str,
        new: NewRequest,
        id: &str,
        at: i64,
    ) -> Result<GrantView, EmergencyError> {
        if by.trim().is_empty() {
            return Err(EmergencyError::new(CODE_NOT_SELF));
        }
        let reason = trimmed(&new.reason)?;
        if !self.p.policy.approvers.iter().any(|a| a == &new.approver) {
            return Err(EmergencyError::new(CODE_APPROVER_NOT_ALLOWED));
        }
        let minutes = new.minutes.min(u64::MAX / 60);
        let request = EmergencyRequest::open(
            id,
            by,
            &new.role,
            GrantScope::unscoped(),
            reason,
            &new.approver,
            &self.p.policy.escalate_to,
            minutes * 60,
            at,
            &*self.p.refusals,
        )
        .map_err(|e| qcode(&e))?;
        let grant = EmergencyGrant::request(request, self.p.policy.emergency.clone())
            .map_err(|e| qcode(&e))?;
        let body = Stored::new(&self.p.site, grant);
        self.emit(&self.event(
            &body,
            ChangeOp::Updated,
            ChangeActor::user(by),
            at,
            "emergency access requested",
        ))
        .await?;
        if !self.p.store.put(None, &body).await? {
            return Err(EmergencyError::new(CODE_UNAVAILABLE));
        }
        Ok(GrantView::of(&body))
    }

    /// The approver's decision to approve (idempotent: approving an approved grant again changes
    /// nothing). With an `override_` the approver tolerates a segregation-of-duties conflict, when
    /// the policy allows it.
    ///
    /// # Errors
    ///
    /// The approval's refusals (not the approver, the grantee, not pending, an SLA that lapsed),
    /// `conflict` for an override the policy does not allow, `override_required` for an incomplete
    /// one.
    pub async fn approve(
        &self,
        id: &str,
        by: &str,
        override_: Option<OverrideSubmission>,
    ) -> Result<GrantView, EmergencyError> {
        let result = self.approve_inner(id, by, override_).await;
        result.map_err(|e| self.refuse("approve", by, id, e))
    }

    async fn approve_inner(
        &self,
        id: &str,
        by: &str,
        override_: Option<OverrideSubmission>,
    ) -> Result<GrantView, EmergencyError> {
        if let Some(o) = &override_ {
            if !self.p.policy.allow_sod_override {
                return Err(EmergencyError::new(CODE_CONFLICT));
            }
            if !o.is_complete() {
                return Err(EmergencyError::new(CODE_OVERRIDE_REQUIRED));
            }
        }
        let at = self.now();
        let loaded = self.p.store.load(id).await?;
        let already = loaded.body.grant.approval.is_granted();
        if already && loaded.body.grant.request.approver != by {
            return Err(EmergencyError::new(CODE_NOT_PERMITTED));
        }
        if !already {
            // A `role:<name>` approver is matched through the roles the caller holds now.
            let roles = self
                .p
                .assignments
                .roles_of(by, at)
                .await
                .map_err(EmergencyError::new)?;
            let actor = by.to_owned();
            let holds =
                move |who: &str, role: &str| who == actor && roles.iter().any(|r| r == role);
            let refusals = Arc::clone(&self.p.refusals);
            let supplied = override_.clone();
            self.transition(id, |body| {
                body.grant
                    .decide(ApprovalDecision::Approve, by, at, &holds, &*refusals)
                    .map_err(|e| qcode(&e))?;
                body.override_submission = supplied.clone();
                body.blocked = None;
                Ok(Some(self.event(
                    body,
                    ChangeOp::Updated,
                    ChangeActor::user(by),
                    at,
                    "emergency access approved",
                )))
            })
            .await?;
        } else if let Some(supplied) = override_ {
            // An approver whose grant was blocked by a conflict may submit the override later.
            self.transition(id, |body| {
                body.override_submission = Some(supplied.clone());
                body.blocked = None;
                Ok(None)
            })
            .await?;
        }
        let _ = self.advance(id).await;
        Ok(GrantView::of(&self.p.store.load(id).await?.body))
    }

    /// The approver's decision to reject.
    ///
    /// # Errors
    ///
    /// As [`Emergency::approve`].
    pub async fn reject(
        &self,
        id: &str,
        by: &str,
        reason: &str,
    ) -> Result<GrantView, EmergencyError> {
        let result = async {
            let reason = trimmed(reason)?.to_owned();
            let at = self.now();
            let roles = self
                .p
                .assignments
                .roles_of(by, at)
                .await
                .map_err(EmergencyError::new)?;
            let actor = by.to_owned();
            let holds =
                move |who: &str, role: &str| who == actor && roles.iter().any(|r| r == role);
            let refusals = Arc::clone(&self.p.refusals);
            let body = self
                .transition(id, |body| {
                    body.grant
                        .decide(
                            ApprovalDecision::Reject {
                                reason: reason.clone(),
                            },
                            by,
                            at,
                            &holds,
                            &*refusals,
                        )
                        .map_err(|e| qcode(&e))?;
                    Ok(Some(self.event(
                        body,
                        ChangeOp::Updated,
                        ChangeActor::user(by),
                        at,
                        "emergency access rejected",
                    )))
                })
                .await?;
            Ok(GrantView::of(&body))
        }
        .await;
        result.map_err(|e| self.refuse("reject", by, id, e))
    }

    /// Ends an active session now. Allowed to the session's user, the named approver and a
    /// review controller.
    ///
    /// # Errors
    ///
    /// `not_permitted` for anyone else; the state machine's refusal for a grant that is not
    /// active.
    pub async fn end(&self, id: &str, by: &str) -> Result<GrantView, EmergencyError> {
        let result = async {
            let loaded = self.p.store.load(id).await?;
            let g = &loaded.body.grant;
            let allowed = by == g.request.user
                || by == g.request.approver
                || self.p.policy.controllers.iter().any(|c| c == by);
            if !allowed {
                return Err(EmergencyError::new(CODE_NOT_PERMITTED));
            }
            let at = self.now();
            self.finish(id, at, at, ChangeActor::user(by), "removed")
                .await?;
            let _ = self.advance(id).await;
            Ok(GrantView::of(&self.p.store.load(id).await?.body))
        }
        .await;
        result.map_err(|e| self.refuse("end", by, id, e))
    }

    /// A controller's check of an ended session: confirm it, or flag it.
    ///
    /// # Errors
    ///
    /// The review's refusals (the session's user, not the controller, not a distinct second
    /// person, not pending).
    pub async fn review(
        &self,
        id: &str,
        by: &str,
        confirm: bool,
        note: &str,
    ) -> Result<GrantView, EmergencyError> {
        let result = async {
            let note = if note.trim().is_empty() && confirm {
                String::new()
            } else {
                trimmed(note)?.to_owned()
            };
            // An ended session whose review has not been opened yet is opened first.
            let _ = self.advance(id).await;
            let at = self.now();
            let body = self
                .transition(id, |body| {
                    if confirm {
                        body.grant
                            .confirm(by, at, (!note.is_empty()).then_some(note.as_str()))
                            .map_err(|e| qcode(&e))?;
                    } else {
                        body.grant.flag(by, at, &note).map_err(|e| qcode(&e))?;
                    }
                    Ok(Some(self.event(
                        body,
                        ChangeOp::Updated,
                        ChangeActor::user(by),
                        at,
                        if confirm {
                            "emergency review confirmed"
                        } else {
                            "emergency review flagged"
                        },
                    )))
                })
                .await?;
            Ok(GrantView::of(&body))
        }
        .await;
        result.map_err(|e| self.refuse("review", by, id, e))
    }

    /// One grant.
    ///
    /// # Errors
    ///
    /// `not_found`, `unavailable`, or the record's `state_invalid` / `state_version`.
    pub async fn status(&self, id: &str) -> Result<GrantView, EmergencyError> {
        Ok(GrantView::of(&self.p.store.load(id).await?.body))
    }

    /// Every grant, in id order, and how many stored records could not be read.
    ///
    /// # Errors
    ///
    /// `unavailable`.
    pub async fn list(&self) -> Result<(Vec<GrantView>, usize), EmergencyError> {
        let all = self.p.store.all().await?;
        let mut views = Vec::new();
        let mut unreadable = 0;
        for item in all {
            match item {
                Ok(loaded) => views.push(GrantView::of(&loaded.body)),
                Err(_) => unreadable += 1,
            }
        }
        Ok((views, unreadable))
    }

    /// Records one action of the user's active session, tagged with the grant. Called by the
    /// request path for every action a user takes while holding an emergency assignment.
    ///
    /// # Errors
    ///
    /// `session_full` past the bound; the state machine's refusal when the session is not
    /// running or is not the caller's.
    pub async fn record_action(
        &self,
        id: &str,
        by: &str,
        action: &str,
        detail: Option<&str>,
    ) -> Result<(), EmergencyError> {
        let result = async {
            let at = self.now();
            if detail.is_some_and(|d| d.len() > MAX_TEXT) {
                return Err(EmergencyError::new(
                    spot_iam_questions::codes::REQUEST_INVALID,
                ));
            }
            self.transition(id, |body| {
                if body.grant.request.user != by {
                    return Err(EmergencyError::new(CODE_NOT_PERMITTED));
                }
                if body.grant.actions.len() >= MAX_ACTIONS {
                    return Err(EmergencyError::new(CODE_SESSION_FULL));
                }
                let context = body.grant.context();
                // The frame is validated by the shared recording component; the actions kept in
                // the record are the source the archive is rebuilt from.
                let mut sink = MemoryRecorder::new(&body.id, 1 << 20);
                body.grant
                    .record(&context, action, detail, at, &mut sink)
                    .map_err(|e| qcode(&e))?;
                Ok(None)
            })
            .await?;
            Ok(())
        }
        .await;
        result.map_err(|e| self.refuse("record_action", by, id, e))
    }
}
