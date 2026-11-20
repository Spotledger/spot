//! The sweep: every step a grant can take on its own (WP-IAM-08.T14). One call to
//! [`Emergency::advance`] makes at most a few transitions of one grant, each a compare-and-swap of
//! its stored record; [`Emergency::sweep`] does it for every grant and is idempotent.
//!
//! The steps, in the order a grant meets them: launch the approval (needs the process host),
//! expire an approval whose SLA lapsed, activate an approved grant (the gate, the activation
//! instant, the assignment), end a session at its `valid_to`, archive its recording, open the
//! controller's review, escalate a review that is due. A step that cannot be taken yet (no host,
//! a conflict, no eligible controller) answers "nothing changed" and is retried quietly.

use std::sync::Arc;

use spot_bpm_engine::{CaseId, TimerRegistration};
use spot_break_glass::{FrameKind, MemoryRecorder, Recorder};
use spot_dag::NodeId;
use spot_iam_governance::{ChangeActor, ChangeOp, GrantRequest, GrantRequestId, Grantor};
use spot_iam_questions::{EMERGENCY_REVIEW_NODE, EmergencyGrant, EmergencyPhase, ReviewState};
use spot_iam_roles::Source;

use super::flow::{Emergency, SweepReport, instant, qcode};
use super::model::{
    Body, CODE_CONFLICT, CODE_OVERRIDE_REQUIRED, CODE_REVIEW_UNASSIGNED, CODE_UNAVAILABLE,
    EmergencyError, RecordingStatus,
};
use super::seams::{Activation, run_blocking};
use super::store::Loaded;
use crate::services::governance::GrantOutcome;

const STEPS: usize = 8;

impl Emergency {
    // ------------------------------------------------------------------------------------------
    // The sweep: every step a grant can take on its own
    // ------------------------------------------------------------------------------------------

    /// Advances one grant as far as it can go now.
    ///
    /// # Errors
    ///
    /// The code of the step that failed; what was done before it stays done.
    pub async fn advance(&self, id: &str) -> Result<usize, EmergencyError> {
        let mut done = 0;
        for _ in 0..STEPS {
            if !self.step(id).await? {
                break;
            }
            done += 1;
        }
        Ok(done)
    }

    /// Advances every grant. Idempotent: a second run right after the first does nothing.
    pub async fn sweep(&self) -> SweepReport {
        let mut report = SweepReport::default();
        let all = match self.p.store.all().await {
            Ok(all) => all,
            Err(_) => {
                report.failed += 1;
                return report;
            }
        };
        for item in all {
            match item {
                Ok(loaded) => match self.advance(&loaded.body.id).await {
                    Ok(n) => report.advanced += n,
                    Err(error) => {
                        report.failed += 1;
                        self.refuse("sweep", "system", &loaded.body.id, error);
                    }
                },
                Err(_) => report.unreadable += 1,
            }
        }
        report
    }

    /// One transition of grant `id`; `Ok(true)` when something changed.
    async fn step(&self, id: &str) -> Result<bool, EmergencyError> {
        let loaded = self.p.store.load(id).await?;
        let now = self.now();
        let phase = loaded.body.grant.phase.clone();
        match &phase {
            EmergencyPhase::Requested => self.begin_approval(id, now).await,
            EmergencyPhase::AwaitingApproval { .. } => {
                if loaded.body.grant.approval.is_granted() {
                    self.activate(loaded, now).await
                } else if loaded
                    .body
                    .grant
                    .approval
                    .expires_at()
                    .is_some_and(|expires| now > expires)
                {
                    self.transition(id, |b| {
                        b.grant.expire_approval(now).map_err(|e| qcode(&e))?;
                        Ok(Some(self.event(
                            b,
                            ChangeOp::Updated,
                            ChangeActor::system("emergency_access"),
                            now,
                            "emergency approval expired",
                        )))
                    })
                    .await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            EmergencyPhase::Active { ends_at, .. } => {
                if now >= *ends_at {
                    self.finish(
                        id,
                        *ends_at,
                        now,
                        ChangeActor::system("emergency_access"),
                        "expired",
                    )
                    .await?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            EmergencyPhase::Ended { at } => {
                let at = *at;
                if loaded.body.recording == RecordingStatus::Open {
                    self.archive(loaded, at).await
                } else if loaded.body.grant.review.is_none() {
                    self.open_review(loaded, now).await
                } else {
                    self.escalate(loaded, now).await
                }
            }
            EmergencyPhase::Rejected { .. } | EmergencyPhase::Expired { .. } => Ok(false),
        }
    }

    async fn begin_approval(&self, id: &str, now: i64) -> Result<bool, EmergencyError> {
        let Some(launcher) = &self.p.launcher else {
            // No process host: the request waits, and readiness says so.
            return Ok(false);
        };
        let launcher = Arc::clone(launcher);
        self.transition(id, |body| {
            run_blocking(|| body.grant.begin(&*launcher, now)).map_err(|e| qcode(&e))?;
            Ok(Some(self.event(
                body,
                ChangeOp::Updated,
                ChangeActor::system("emergency_access"),
                now,
                "emergency approval started",
            )))
        })
        .await?;
        Ok(true)
    }

    async fn activate(&self, loaded: Loaded, now: i64) -> Result<bool, EmergencyError> {
        let id = loaded.body.id.clone();
        let body = loaded.body.clone();
        let user = body.grant.request.user.clone();
        let duration = i64::try_from(body.grant.request.duration_secs).unwrap_or(i64::MAX);
        let (at, record) = match self
            .p
            .assignments
            .find(&id)
            .await
            .map_err(EmergencyError::new)?
        {
            // The assignment was written by an earlier attempt that did not commit: the instant it
            // was written at is in the record, and the gate already said yes.
            Some(record) => (
                body.activation_at
                    .ok_or(EmergencyError::new(CODE_UNAVAILABLE))?,
                record,
            ),
            None => {
                if let Some(code) = self.gate(&body).await? {
                    return self.block(&id, code, now).await;
                }
                // The instant is fixed in the record before the assignment is written, so a retry
                // after a crash writes the same window.
                let fixed = self
                    .transition(&id, |b| {
                        if b.activation_at.is_none() {
                            b.activation_at = Some(now);
                        }
                        Ok(None)
                    })
                    .await?;
                let at = fixed.activation_at.unwrap_or(now);
                let event = self.event(
                    &body,
                    ChangeOp::Created,
                    ChangeActor::user(&body.grant.request.approver),
                    at,
                    &body.grant.request.reason,
                );
                self.emit(&event).await?;
                let record = self
                    .p
                    .assignments
                    .grant(&Activation {
                        grant: id.clone(),
                        user,
                        role: body.grant.request.role.as_str().to_owned(),
                        valid_from: at,
                        valid_to: at.saturating_add(duration),
                        reason: body.grant.request.reason.clone(),
                        event,
                    })
                    .await
                    .map_err(EmergencyError::new)?;
                self.changed();
                (at, record)
            }
        };
        self.transition(&id, |b| {
            if matches!(b.grant.phase, EmergencyPhase::Active { .. }) {
                return Ok(None);
            }
            b.grant.activate(at).map_err(|e| qcode(&e))?;
            b.assignment = Some(record.clone());
            b.blocked = None;
            Ok(None)
        })
        .await?;
        Ok(true)
    }

    /// The segregation-of-duties gate: `None` when the grant may be made, else the code that
    /// blocks it (and records it).
    async fn gate(&self, body: &Body) -> Result<Option<&'static str>, EmergencyError> {
        let Some(guard) = &self.p.guard else {
            return Err(EmergencyError::new(CODE_UNAVAILABLE));
        };
        let request_id = GrantRequestId::new(&body.id).ok_or(EmergencyError::new(
            spot_iam_questions::codes::REQUEST_INVALID,
        ))?;
        let approver = &body.grant.request.approver;
        let mut request = GrantRequest::new(
            &request_id,
            &body.grant.request.user,
            body.grant.request.role.as_str(),
            Grantor::plain(approver),
        )
        .with_source(Source::Emergency)
        .with_reason(&body.grant.request.reason);
        if let Some(over) = body
            .override_submission
            .clone()
            .filter(|_| self.p.policy.allow_sod_override)
        {
            request.override_submission = Some(over);
            request.grantor = Grantor::new(approver, ["override_sod"]);
        }
        match guard.recheck_approved(request).await {
            GrantOutcome::Allowed => Ok(None),
            GrantOutcome::Blocked(_) => Ok(Some(CODE_CONFLICT)),
            GrantOutcome::NeedsOverride(_) => Ok(Some(CODE_OVERRIDE_REQUIRED)),
            GrantOutcome::Unavailable(_) | GrantOutcome::Pending { .. } => {
                Err(EmergencyError::new(CODE_UNAVAILABLE))
            }
        }
    }

    /// Records why a grant waits, once, and answers "nothing changed" until it is cleared.
    async fn block(&self, id: &str, code: &'static str, now: i64) -> Result<bool, EmergencyError> {
        let mut first = false;
        self.transition(id, |b| {
            first = b.blocked.as_deref() != Some(code);
            b.blocked = Some(code.to_owned());
            Ok(first.then(|| {
                self.event(
                    b,
                    ChangeOp::Updated,
                    ChangeActor::system("emergency_access"),
                    now,
                    "emergency grant blocked by a segregation-of-duties conflict",
                )
            }))
        })
        .await?;
        if first {
            self.refuse("activate", "system", id, EmergencyError::new(code));
        }
        Ok(false)
    }

    /// Ends the session at `end_at` (the assignment window closes then) and records it at `now`.
    pub(super) async fn finish(
        &self,
        id: &str,
        end_at: i64,
        now: i64,
        actor: ChangeActor,
        cause: &str,
    ) -> Result<(), EmergencyError> {
        let loaded = self.p.store.load(id).await?;
        let body = &loaded.body;
        if !matches!(body.grant.phase, EmergencyPhase::Active { .. }) {
            return Err(self.state_error(&body.grant));
        }
        let event = self
            .event(
                body,
                ChangeOp::Ended,
                actor.clone(),
                end_at,
                "emergency access ended",
            )
            .with_cause(cause);
        self.emit(&event).await?;
        self.p
            .assignments
            .end(id, end_at.min(now), &event)
            .await
            .map_err(EmergencyError::new)?;
        self.changed();
        self.transition(id, |b| {
            if matches!(b.grant.phase, EmergencyPhase::Active { .. }) {
                b.grant.end(end_at.min(now)).map_err(|e| qcode(&e))?;
            }
            Ok(None)
        })
        .await?;
        Ok(())
    }

    fn state_error(&self, _grant: &EmergencyGrant) -> EmergencyError {
        EmergencyError::new(spot_iam_questions::codes::EMERGENCY_STATE)
    }

    /// Archives the recording of an ended session, or says it cannot.
    async fn archive(&self, loaded: Loaded, _now: i64) -> Result<bool, EmergencyError> {
        let body = &loaded.body;
        let (started, ended) = match (&body.activation_at, &body.grant.phase) {
            (Some(s), EmergencyPhase::Ended { at }) => (*s, *at),
            _ => return Ok(false),
        };
        let mut recorder = MemoryRecorder::new(&body.id, 8 << 20);
        for action in &body.grant.actions {
            let bytes =
                serde_json::to_vec(action).map_err(|_| EmergencyError::new(CODE_UNAVAILABLE))?;
            // A full recorder keeps its truncation marker; the actions stay in the record.
            let _ = recorder.append(FrameKind::Input, action.at, &bytes, true);
        }
        let mut bytes = Vec::new();
        for frame in recorder.frames() {
            bytes.extend_from_slice(&frame.to_bytes());
        }
        let status = match self
            .p
            .recording
            .archive(&body.id, &body.grant.request.user, started, ended, bytes)
            .await
        {
            Ok(digest) => RecordingStatus::Archived { digest },
            Err(_) => RecordingStatus::Unavailable,
        };
        let label = status.label();
        self.transition(&body.id, |b| {
            b.recording = status.clone();
            Ok(Some(self.event(
                b,
                ChangeOp::Updated,
                ChangeActor::system("emergency_access"),
                ended,
                if label == "archived" {
                    "emergency session recording archived"
                } else {
                    "emergency session recording unavailable"
                },
            )))
        })
        .await?;
        Ok(true)
    }

    async fn open_review(&self, loaded: Loaded, now: i64) -> Result<bool, EmergencyError> {
        let id = loaded.body.id.clone();
        let user = loaded.body.grant.request.user.clone();
        let Some(controller) = self
            .p
            .policy
            .controllers
            .iter()
            .find(|c| **c != user)
            .cloned()
        else {
            return self.block_review(&id, now).await;
        };
        let timers = Arc::clone(&self.p.timers);
        self.transition(&id, |b| {
            b.grant
                .open_review(&controller, now, &*timers)
                .map_err(|e| qcode(&e))?;
            b.blocked = None;
            Ok(Some(self.event(
                b,
                ChangeOp::Updated,
                ChangeActor::system("emergency_access"),
                now,
                "emergency review opened",
            )))
        })
        .await?;
        Ok(true)
    }

    async fn block_review(&self, id: &str, now: i64) -> Result<bool, EmergencyError> {
        let mut first = false;
        self.transition(id, |b| {
            first = b.blocked.as_deref() != Some(CODE_REVIEW_UNASSIGNED);
            b.blocked = Some(CODE_REVIEW_UNASSIGNED.to_owned());
            Ok(first.then(|| {
                self.event(
                    b,
                    ChangeOp::Updated,
                    ChangeActor::system("emergency_access"),
                    now,
                    "emergency review has no eligible controller",
                )
            }))
        })
        .await?;
        if first {
            self.refuse(
                "open_review",
                "system",
                id,
                EmergencyError::new(CODE_REVIEW_UNASSIGNED),
            );
        }
        Ok(false)
    }

    /// Arms the review's timer (it lives in memory, so every step arms it again after a restart)
    /// and escalates a review that is due.
    async fn escalate(&self, loaded: Loaded, now: i64) -> Result<bool, EmergencyError> {
        let body = &loaded.body;
        let Some(review) = &body.grant.review else {
            return Ok(false);
        };
        if !matches!(review.state, ReviewState::Pending) {
            return Ok(false);
        }
        self.arm(body);
        if now < review.due_at {
            return Ok(false);
        }
        let timers = Arc::clone(&self.p.timers);
        let mut fired = false;
        self.transition(&body.id, |b| {
            let escalation = b.grant.escalate_due(&*timers, now);
            fired = escalation.is_some();
            Ok(escalation.map(|e| {
                self.event(
                    b,
                    ChangeOp::Updated,
                    ChangeActor::system("emergency_access"),
                    now,
                    &format!("emergency review escalated ({})", e.count),
                )
            }))
        })
        .await?;
        Ok(fired)
    }

    /// Registers the escalation timer of an open review.
    fn arm(&self, body: &Body) {
        let Some(review) = &body.grant.review else {
            return;
        };
        let mut raw = String::from("emergency_");
        for ch in review.grant.chars() {
            let c = ch.to_ascii_lowercase();
            raw.push(
                if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                    c
                } else {
                    '_'
                },
            );
        }
        raw.truncate(64);
        if let (Ok(case), Ok(node)) = (CaseId::new(&raw), NodeId::new(EMERGENCY_REVIEW_NODE)) {
            self.p.timers.register(TimerRegistration {
                case,
                node,
                attempt: 0,
                due: Some(instant(review.due_at)),
                event: Some(body.grant.review_event()),
                human: None,
            });
        }
    }
}
