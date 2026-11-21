//! Emergency access with controller review: the firefighter pattern of ADR-070 section 9.
//!
//! An emergency grant is a pre-defined, time-boxed assignment of an elevated role with source
//! `emergency`. It moves through
//! `Requested -> AwaitingApproval -> Active -> Ended -> Under review -> Confirmed | Flagged`,
//! with `Rejected` and `Expired` terminals on the approval side:
//!
//! 1. the user raises a **request with a reason**, bounded by the tenant's maximum duration;
//! 2. the **role owner approves** it on the BPM engine — the same [`ProcessLauncher`] trait T1
//!    uses, so an approval is a Process, never a private workflow;
//! 3. an approved grant **activates** as a time-boxed assignment (`valid_from`/`valid_to`);
//! 4. every action in the session is **recorded**, tagged with the grant id through the
//!    session's [`SessionContext`], into the shared `spot-break-glass` recording component;
//! 5. on the end, a [`ReviewTask`] goes to a **controller who is not the user**; when the
//!    controller is also the approver and the policy asks for it, a **second distinct check**
//!    is required before the review confirms;
//! 6. an unconfirmed review **escalates on schedule** through the engine's
//!    [`TimerPort`](spot_bpm_engine::TimerPort).
//!
//! Nothing here reads a clock: every instant is an argument.

use serde::{Deserialize, Serialize};
use spot_bpm_engine::{CaseId, TimerPort, TimerRegistration};
use spot_dag::NodeId;
use spot_iam_governance::{
    ApprovalDecision, ApprovalSpec, EscalationTarget, GovError, GrantApproval, GrantRequestId,
    GrantScope, MAX_TARGET_BYTES, ProcessLauncher,
};
use spot_iam_roles::{Assignment, RoleName, Source};
use time::OffsetDateTime;

use crate::audit::{EmergencyAuditor, EmergencyRefusal};
use crate::error::{EMERGENCY_STATE, QuestionsError};
use crate::recording::SessionRecorder;

/// The node id an emergency review's escalation timer is registered under.
pub const EMERGENCY_REVIEW_NODE: &str = "controller_review";

/// The tenant's emergency-access policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct EmergencyPolicy {
    /// The longest session the tenant allows, in whole seconds.
    pub max_duration_secs: u64,
    /// How long the role owner has to approve before the approval expires.
    pub approval_sla_secs: u64,
    /// How long the controller has to review the session before it escalates.
    pub review_due_secs: u64,
    /// How long after it falls due an unconfirmed review waits before escalating again.
    pub escalate_after_secs: u64,
    /// How many times an unconfirmed review may escalate before it stays escalated.
    pub max_escalations: u8,
    /// Whether a controller who is also the approver needs a second, distinct check before
    /// the review confirms.
    pub require_second_check_when_approver_reviews: bool,
    /// The elevated roles a tenant allows through emergency access. Empty means any role the
    /// registry knows; a non-empty list is the pre-defined set ADR-070 section 9 describes.
    #[serde(default)]
    pub emergency_roles: Vec<String>,
}

impl Default for EmergencyPolicy {
    fn default() -> Self {
        EmergencyPolicy {
            max_duration_secs: 3600,
            approval_sla_secs: 3600,
            review_due_secs: 86_400,
            escalate_after_secs: 86_400,
            max_escalations: 3,
            require_second_check_when_approver_reviews: true,
            emergency_roles: Vec::new(),
        }
    }
}

/// What a user asks for: an elevated role, for a reason, for a bounded time.
///
/// The escalation target is a validated [`EscalationTarget`]: a request cannot hold
/// an empty, blank, control-character or over-long target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct EmergencyRequest {
    /// The idempotent grant request id.
    pub id: GrantRequestId,
    /// The user the session is for.
    pub user: String,
    /// The elevated role.
    pub role: RoleName,
    /// Where the role applies.
    #[serde(default)]
    pub scope: GrantScope,
    /// Why emergency access is needed; required, non-blank.
    pub reason: String,
    /// The role owner who approves it.
    pub approver: String,
    /// Who the approval escalates to: a validated actor id or `role:<name>` reference.
    pub escalate_to: EscalationTarget,
    /// How long the session may run, in whole seconds.
    pub duration_secs: u64,
}

impl EmergencyRequest {
    /// A request, checked on its own.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::RequestInvalid`] for a blank id, user, role, reason or approver, or a
    /// zero duration;
    /// [`QuestionsError::EscalationTargetInvalid`] when the escalation target is empty, blank,
    /// has a control character or is longer than [`MAX_TARGET_BYTES`]. No error ever
    /// contains the offered target.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        user: &str,
        role: &str,
        scope: GrantScope,
        reason: &str,
        approver: &str,
        escalate_to: &str,
        duration_secs: u64,
    ) -> Result<EmergencyRequest, QuestionsError> {
        let Some(id) = GrantRequestId::new(id) else {
            return Err(invalid("(request)", "the request id must be non-blank"));
        };
        for (field, value) in [
            ("user", user),
            ("role", role),
            ("reason", reason),
            ("approver", approver),
        ] {
            if value.trim().is_empty() {
                return Err(invalid(
                    id.as_str(),
                    &format!("`{field}` must be non-blank"),
                ));
            }
        }
        let escalate_to = escalation_target(id.as_str(), escalate_to)?;
        if user == approver {
            return Err(invalid(
                id.as_str(),
                "the approver must not be the user the session is for",
            ));
        }
        if duration_secs == 0 {
            return Err(QuestionsError::EmergencyDuration {
                grant: id.as_str().to_owned(),
                detail: "the requested duration must be positive".to_owned(),
            });
        }
        Ok(EmergencyRequest {
            id,
            user: user.to_owned(),
            role: RoleName::from(role),
            scope,
            reason: reason.to_owned(),
            approver: approver.to_owned(),
            escalate_to,
            duration_secs,
        })
    }

    /// A request, checked on its own, with a refusal written to `audit` before it returns.
    ///
    /// A refused request is not created and leaves exactly one [`EmergencyRefusal`] row: the
    /// request id it would have had, who asked, the instant and the stable code — never the
    /// target the requester offered. A successful request writes no row.
    ///
    /// # Errors
    ///
    /// As [`EmergencyRequest::new`].
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        id: &str,
        user: &str,
        role: &str,
        scope: GrantScope,
        reason: &str,
        approver: &str,
        escalate_to: &str,
        duration_secs: u64,
        at: i64,
        audit: &dyn EmergencyAuditor,
    ) -> Result<EmergencyRequest, QuestionsError> {
        Self::new(
            id,
            user,
            role,
            scope,
            reason,
            approver,
            escalate_to,
            duration_secs,
        )
        .inspect_err(|error| {
            audit.record(&EmergencyRefusal {
                grant: if GrantRequestId::new(id).is_some() {
                    id.to_owned()
                } else {
                    "(request)".to_owned()
                },
                by: user.to_owned(),
                attempted: "request".to_owned(),
                at,
                code: error.code().to_owned(),
            });
        })
    }
}

/// The validated escalation target, refused with this crate's stable code and without the
/// offered value.
fn escalation_target(subject: &str, escalate_to: &str) -> Result<EscalationTarget, QuestionsError> {
    EscalationTarget::new(escalate_to).map_err(|_| QuestionsError::EscalationTargetInvalid {
        subject: subject.to_owned(),
        detail: format!(
            "`escalate_to` needs an actor id or `role:<name>` with text, no control characters \
             and at most {MAX_TARGET_BYTES} bytes"
        ),
    })
}

fn invalid(subject: &str, detail: &str) -> QuestionsError {
    QuestionsError::RequestInvalid {
        subject: subject.to_owned(),
        detail: detail.to_owned(),
    }
}

/// Where an emergency grant is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum EmergencyPhase {
    /// The request is made; the approval has not been launched.
    Requested,
    /// The approval is running on the BPM engine.
    AwaitingApproval {
        /// When it was launched, in integer epoch seconds.
        launched_at: i64,
    },
    /// The role owner refused it; nothing is granted.
    Rejected {
        /// Who refused it.
        by: String,
        /// When, in integer epoch seconds.
        at: i64,
        /// Why.
        reason: String,
    },
    /// The approval's SLA lapsed; nothing is granted.
    Expired {
        /// When it expired, in integer epoch seconds.
        at: i64,
    },
    /// The session is running: the time-boxed assignment is in force.
    Active {
        /// The assignment the grant became.
        assignment: Assignment,
        /// When the session started, in integer epoch seconds.
        started_at: i64,
        /// When it ends, in integer epoch seconds.
        ends_at: i64,
    },
    /// The session has ended and awaits (or is under) review.
    Ended {
        /// When it ended, in integer epoch seconds.
        at: i64,
    },
}

impl EmergencyPhase {
    /// A stable label, for error details.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            EmergencyPhase::Requested => "requested",
            EmergencyPhase::AwaitingApproval { .. } => "awaiting_approval",
            EmergencyPhase::Rejected { .. } => "rejected",
            EmergencyPhase::Expired { .. } => "expired",
            EmergencyPhase::Active { .. } => "active",
            EmergencyPhase::Ended { .. } => "ended",
        }
    }
}

/// The identifying context of an active emergency session: the grant every action is tagged
/// with, and the user it is for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SessionContext {
    /// The grant id.
    pub grant: String,
    /// The user the session is for.
    pub user: String,
}

/// One action the session took. Never carries a secret: the action's name and an optional
/// non-secret detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SessionAction {
    /// The grant the action belongs to.
    pub grant: String,
    /// The user who acted.
    pub user: String,
    /// When, in integer epoch seconds.
    pub at: i64,
    /// What the action was (`read`, `post`, `export`, ...).
    pub action: String,
    /// An optional non-secret detail (a record name, a count).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One check a reviewer recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ReviewCheck {
    /// Who checked.
    pub by: String,
    /// When, in integer epoch seconds.
    pub at: i64,
    /// Whether the check confirmed the session.
    pub confirmed: bool,
    /// The reviewer's note, when they gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Where one review is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum ReviewState {
    /// Waiting for the controller (and, when required, the second check).
    Pending,
    /// The controller confirmed; the session is clean.
    Confirmed {
        /// When the last required check was recorded, in integer epoch seconds.
        at: i64,
    },
    /// The controller flagged the session; it needs a person.
    Flagged {
        /// When, in integer epoch seconds.
        at: i64,
        /// Why.
        reason: String,
    },
    /// The review fell due unconfirmed and escalated.
    Escalated {
        /// How many escalations have happened.
        count: u8,
        /// When the last escalation happened, in integer epoch seconds.
        at: i64,
    },
}

/// The review of one ended session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ReviewTask {
    /// The review's id.
    pub id: String,
    /// The grant it reviews.
    pub grant: String,
    /// The session's user: never a reviewer.
    pub user: String,
    /// The controller who must review it.
    pub controller: String,
    /// The approver, so the second-check rule can be decided.
    pub approver: String,
    /// How many distinct checks the review needs (1, or 2 when the controller is the
    /// approver and the policy asks for it).
    pub checks_required: u8,
    /// The checks recorded so far, in order.
    #[serde(default)]
    pub checks: Vec<ReviewCheck>,
    /// When the review falls due, in integer epoch seconds.
    pub due_at: i64,
    /// Where an unconfirmed review escalates.
    pub escalate_to: String,
    /// How many escalations have happened.
    pub escalations: u8,
    /// Where the review is.
    pub state: ReviewState,
}

impl ReviewTask {
    /// Whether the review still needs a decision.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self.state, ReviewState::Pending)
    }

    /// The people who may still record a check: for the first check the controller, and for a
    /// second check anyone distinct from the user and the controller.
    #[must_use]
    pub fn may_decide(&self, by: &str) -> bool {
        if by == self.user {
            return false;
        }
        if self.checks.is_empty() {
            by == self.controller
        } else if self.checks_required >= 2 {
            self.checks.iter().all(|c| c.by != by)
        } else {
            false
        }
    }
}

/// One escalation of an unconfirmed review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Escalation {
    /// The grant being reviewed.
    pub grant: String,
    /// The review.
    pub review: String,
    /// Who the review escalated to.
    pub to: String,
    /// Which escalation this is, starting at 1.
    pub count: u8,
    /// When it escalated, in integer epoch seconds.
    pub at: i64,
}

/// One emergency grant and its whole life.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct EmergencyGrant {
    /// What was asked for.
    pub request: EmergencyRequest,
    /// The tenant policy in force when it was requested.
    pub policy: EmergencyPolicy,
    /// The role owner's approval, on the BPM engine.
    pub approval: GrantApproval,
    /// Where the grant is.
    pub phase: EmergencyPhase,
    /// Every action the session recorded, in order.
    #[serde(default)]
    pub actions: Vec<SessionAction>,
    /// The controller review, once the session ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewTask>,
}

impl EmergencyGrant {
    /// Records a request and checks it against the policy: the reason is non-blank (checked by
    /// [`EmergencyRequest::new`]) and the duration is within the tenant's maximum.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::EmergencyDuration`] when the requested duration exceeds the policy.
    pub fn request(
        request: EmergencyRequest,
        policy: EmergencyPolicy,
    ) -> Result<EmergencyGrant, QuestionsError> {
        if request.duration_secs > policy.max_duration_secs {
            return Err(QuestionsError::EmergencyDuration {
                grant: request.id.as_str().to_owned(),
                detail: format!(
                    "the policy allows at most {} seconds, not {}",
                    policy.max_duration_secs, request.duration_secs
                ),
            });
        }
        if !policy.emergency_roles.is_empty()
            && !policy
                .emergency_roles
                .iter()
                .any(|r| r == request.role.as_str())
        {
            return Err(QuestionsError::RequestInvalid {
                subject: request.id.as_str().to_owned(),
                detail: format!(
                    "role `{}` is not one the tenant allows through emergency access",
                    request.role.as_str()
                ),
            });
        }
        let spec = ApprovalSpec {
            request: request.id.clone(),
            user: request.user.clone(),
            role: request.role.clone(),
            scope: request.scope.clone(),
            reason: request.reason.clone(),
            approver: request.approver.clone(),
            escalate_to: request.escalate_to.clone(),
            sla_secs: policy.approval_sla_secs,
        };
        Ok(EmergencyGrant {
            request,
            policy,
            approval: GrantApproval::new(spec),
            phase: EmergencyPhase::Requested,
            actions: Vec::new(),
            review: None,
        })
    }

    /// The grant's id.
    #[must_use]
    pub fn id(&self) -> &str {
        self.request.id.as_str()
    }

    /// The session context every action is tagged with.
    #[must_use]
    pub fn context(&self) -> SessionContext {
        SessionContext {
            grant: self.id().to_owned(),
            user: self.request.user.clone(),
        }
    }

    /// Launches the role owner's approval on the BPM engine, at most once.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::EmergencyState`] when the request is not fresh;
    /// [`QuestionsError::Approval`] when the process cannot be built or the launcher refuses.
    pub fn begin(&mut self, launcher: &dyn ProcessLauncher, at: i64) -> Result<(), QuestionsError> {
        if !matches!(self.phase, EmergencyPhase::Requested) {
            return Err(self.wrong_phase("begin the approval of"));
        }
        let id = self.id().to_owned();
        self.approval
            .begin(launcher, at)
            .map_err(|e| QuestionsError::Approval {
                request: id,
                detail: e.to_string(),
            })?;
        self.phase = EmergencyPhase::AwaitingApproval { launched_at: at };
        Ok(())
    }

    /// Records the role owner's decision.
    ///
    /// Who may decide is the governance crate's rule, not this one's:
    /// [`GrantApproval::decide_with_roles`] refuses the grantee first, then anyone who is not the
    /// named approver (a `role:<name>` approver is matched through `holds_role`), and lets
    /// `escalate_to` decide only after [`EmergencyGrant::escalate_approval`]. `holds_role` is
    /// called as (actor, role); it does no I/O and is not kept.
    ///
    /// Every refusal writes exactly one [`EmergencyRefusal`] row to `audit` before it returns
    /// and leaves the grant as it was, except that a decision after the SLA lapsed expires the
    /// approval, as it always did. A successful decision writes no row.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::EmergencyState`] when the approval is not running;
    /// [`QuestionsError::Approval`] when the approval refuses the decision (an SLA that lapsed, a
    /// non-pending approval, the grantee, or someone who is not the approver); the governance
    /// code is kept at the start of the detail.
    pub fn decide(
        &mut self,
        decision: ApprovalDecision,
        by: &str,
        at: i64,
        holds_role: &dyn Fn(&str, &str) -> bool,
        audit: &dyn EmergencyAuditor,
    ) -> Result<(), QuestionsError> {
        let attempted = match decision {
            ApprovalDecision::Approve => "approve",
            ApprovalDecision::Reject { .. } => "reject",
        };
        match self.decide_checked(decision, by, at, holds_role) {
            Ok(()) => Ok(()),
            Err((error, code)) => {
                audit.record(&EmergencyRefusal {
                    grant: self.id().to_owned(),
                    by: by.to_owned(),
                    attempted: attempted.to_owned(),
                    at,
                    code: code.to_owned(),
                });
                Err(error)
            }
        }
    }

    /// [`EmergencyGrant::decide`] without the audit: the refusal and its stable code.
    fn decide_checked(
        &mut self,
        decision: ApprovalDecision,
        by: &str,
        at: i64,
        holds_role: &dyn Fn(&str, &str) -> bool,
    ) -> Result<(), (QuestionsError, &'static str)> {
        if !matches!(self.phase, EmergencyPhase::AwaitingApproval { .. }) {
            return Err((self.wrong_phase("decide"), EMERGENCY_STATE));
        }
        if self
            .approval
            .expires_at()
            .is_some_and(|expires| at > expires)
        {
            self.approval
                .expire(at)
                .map_err(|e| (self.approval_error(&e), e.code()))?;
            self.phase = EmergencyPhase::Expired { at };
            return Err((
                QuestionsError::Approval {
                    request: self.id().to_owned(),
                    detail: format!(
                        "{}: the approval's SLA had lapsed; it expired",
                        spot_iam_governance::codes::APPROVAL_NOT_PENDING
                    ),
                },
                spot_iam_governance::codes::APPROVAL_NOT_PENDING,
            ));
        }
        self.approval
            .decide_with_roles(decision, by, at, holds_role)
            .map_err(|e| (self.approval_error(&e), e.code()))?;
        match &self.approval.state {
            spot_iam_governance::ApprovalState::Rejected { by, at, reason } => {
                self.phase = EmergencyPhase::Rejected {
                    by: by.clone(),
                    at: *at,
                    reason: reason.clone(),
                };
            }
            spot_iam_governance::ApprovalState::Expired { at } => {
                self.phase = EmergencyPhase::Expired { at: *at };
            }
            _ => {}
        }
        Ok(())
    }

    /// Records that the approval's SLA timer fired, so the escalation target may decide as well
    /// as the named approver. The host calls it when the timer fires. It decides nothing and
    /// does not expire the approval. Idempotent: a second call changes nothing.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::EmergencyState`] when the approval is not running;
    /// [`QuestionsError::Approval`] when the approval is not pending.
    pub fn escalate_approval(&mut self, at: i64) -> Result<(), QuestionsError> {
        if !matches!(self.phase, EmergencyPhase::AwaitingApproval { .. }) {
            return Err(self.wrong_phase("escalate the approval of"));
        }
        self.approval
            .escalate(at)
            .map_err(|e| self.approval_error(&e))
    }

    /// Expires an approval whose SLA lapsed. Idempotent once expired.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::EmergencyState`] when the approval is not running;
    /// [`QuestionsError::Approval`] when the approval refuses it.
    pub fn expire_approval(&mut self, at: i64) -> Result<(), QuestionsError> {
        if !matches!(self.phase, EmergencyPhase::AwaitingApproval { .. }) {
            return Err(self.wrong_phase("expire the approval of"));
        }
        self.approval
            .expire(at)
            .map_err(|e| QuestionsError::Approval {
                request: self.id().to_owned(),
                detail: e.to_string(),
            })?;
        self.phase = EmergencyPhase::Expired { at };
        Ok(())
    }

    /// Activates an approved grant as a time-boxed assignment with source `emergency`.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::EmergencyState`] when the approval is not approved or the grant is
    /// not awaiting activation.
    pub fn activate(&mut self, at: i64) -> Result<&Assignment, QuestionsError> {
        if !self.approval.is_granted() {
            return Err(QuestionsError::EmergencyState {
                grant: self.id().to_owned(),
                detail: format!(
                    "the approval is not granted; the grant is {}",
                    self.phase.label()
                ),
            });
        }
        if !matches!(self.phase, EmergencyPhase::AwaitingApproval { .. }) {
            return Err(self.wrong_phase("activate"));
        }
        let ends_at =
            at.saturating_add(i64::try_from(self.request.duration_secs).unwrap_or(i64::MAX));
        let mut assignment = Assignment::new(
            &format!("emergency:{}", self.id()),
            &self.request.user,
            self.request.role.as_str(),
        );
        assignment.scope = self.request.scope.to_scope();
        assignment.source = Source::Emergency;
        assignment.valid_from = instant(at);
        assignment.valid_to = instant(ends_at);
        assignment.reason = Some(self.request.reason.clone());
        self.phase = EmergencyPhase::Active {
            assignment,
            started_at: at,
            ends_at,
        };
        self.assignment()
            .ok_or_else(|| QuestionsError::EmergencyState {
                grant: self.id().to_owned(),
                detail: "the assignment vanished after activation".to_owned(),
            })
    }

    /// The time-boxed assignment, once the grant is active.
    #[must_use]
    pub fn assignment(&self) -> Option<&Assignment> {
        match &self.phase {
            EmergencyPhase::Active { assignment, .. } => Some(assignment),
            _ => None,
        }
    }

    /// When the active session ends.
    #[must_use]
    pub fn ends_at(&self) -> Option<i64> {
        match &self.phase {
            EmergencyPhase::Active { ends_at, .. } => Some(*ends_at),
            _ => None,
        }
    }

    /// Records one action of the session, tagged with the grant id through `context`, into the
    /// shared recording component.
    ///
    /// # Errors
    ///
    /// - [`QuestionsError::EmergencyState`] when no session is running, or `at` is past its
    ///   end;
    /// - [`QuestionsError::Recording`] when `context` is not this grant's context, or the
    ///   recorder refuses the frame.
    pub fn record(
        &mut self,
        context: &SessionContext,
        action: &str,
        detail: Option<&str>,
        at: i64,
        recorder: &mut dyn SessionRecorder,
    ) -> Result<(), QuestionsError> {
        if action.trim().is_empty() {
            return Err(invalid(self.id(), "`action` must be non-blank"));
        }
        let ends_at = match &self.phase {
            EmergencyPhase::Active { ends_at, .. } => *ends_at,
            _ => return Err(self.wrong_phase("record an action in")),
        };
        if at > ends_at {
            return Err(QuestionsError::EmergencyState {
                grant: self.id().to_owned(),
                detail: format!("the session ended at {ends_at}"),
            });
        }
        if context.grant != self.id() || context.user != self.request.user {
            return Err(QuestionsError::Recording {
                subject: self.id().to_owned(),
                detail: "the action is not tagged with this grant's context".to_owned(),
            });
        }
        let entry = SessionAction {
            grant: self.id().to_owned(),
            user: self.request.user.clone(),
            at,
            action: action.to_owned(),
            detail: detail.map(str::to_owned),
        };
        let bytes = serde_json::to_vec(&entry).map_err(|e| QuestionsError::Recording {
            subject: self.id().to_owned(),
            detail: format!("the action did not serialise: {e}"),
        })?;
        recorder
            .append(spot_break_glass::FrameKind::Input, at, &bytes, true)
            .map_err(|e| QuestionsError::Recording {
                subject: self.id().to_owned(),
                detail: e.to_string(),
            })?;
        self.actions.push(entry);
        Ok(())
    }

    /// Ends the active session and falls due for review. Idempotent once ended.
    ///
    /// # Errors
    ///
    /// [`QuestionsError::EmergencyState`] when no session is (or was) running.
    pub fn end(&mut self, at: i64) -> Result<(), QuestionsError> {
        match self.phase {
            EmergencyPhase::Active { .. } => {
                self.phase = EmergencyPhase::Ended { at };
                Ok(())
            }
            EmergencyPhase::Ended { .. } => Ok(()),
            _ => Err(self.wrong_phase("end")),
        }
    }

    /// Opens the controller review of an ended session.
    ///
    /// The controller must not be the session's user. When the controller is the approver and
    /// the policy asks for it, the review needs a second, distinct check before it confirms.
    ///
    /// # Errors
    ///
    /// - [`QuestionsError::EmergencyState`] when no session has ended, or a review is already
    ///   open;
    /// - [`QuestionsError::ReviewSelf`] when the controller is the user;
    /// - [`QuestionsError::RequestInvalid`] when the controller is blank.
    pub fn open_review(
        &mut self,
        controller: &str,
        at: i64,
        timers: &dyn TimerPort,
    ) -> Result<&ReviewTask, QuestionsError> {
        if !matches!(self.phase, EmergencyPhase::Ended { .. }) {
            return Err(self.wrong_phase("review"));
        }
        if self.review.is_some() {
            return Err(QuestionsError::EmergencyState {
                grant: self.id().to_owned(),
                detail: "a review is already open".to_owned(),
            });
        }
        if controller == self.request.user {
            return Err(QuestionsError::ReviewSelf {
                review: self.id().to_owned(),
                detail: controller.to_owned(),
            });
        }
        if controller.trim().is_empty() {
            return Err(invalid(self.id(), "the controller must be non-blank"));
        }
        let checks_required = u8::from(
            self.policy.require_second_check_when_approver_reviews
                && controller == self.request.approver,
        ) + 1;
        let due_at =
            at.saturating_add(i64::try_from(self.policy.review_due_secs).unwrap_or(i64::MAX));
        let task = ReviewTask {
            id: format!("{}:review", self.id()),
            grant: self.id().to_owned(),
            user: self.request.user.clone(),
            controller: controller.to_owned(),
            approver: self.request.approver.clone(),
            checks_required,
            checks: Vec::new(),
            due_at,
            escalate_to: self.request.escalate_to.as_str().to_owned(),
            escalations: 0,
            state: ReviewState::Pending,
        };
        self.review = Some(task);
        self.schedule_review(timers);
        self.review
            .as_ref()
            .ok_or_else(|| QuestionsError::EmergencyState {
                grant: self.id().to_owned(),
                detail: "the review vanished".to_owned(),
            })
    }

    /// Records a controller's confirmation.
    ///
    /// # Errors
    ///
    /// - [`QuestionsError::EmergencyState`] when the review is not pending;
    /// - [`QuestionsError::ReviewSelf`] when the decider is the session's user;
    /// - [`QuestionsError::ReviewWrongPerson`] when the decider is not the controller (first
    ///   check) or not a distinct person (second check).
    pub fn confirm(
        &mut self,
        by: &str,
        at: i64,
        note: Option<&str>,
    ) -> Result<&ReviewTask, QuestionsError> {
        self.record_check(by, at, true, note)
    }

    /// Records a controller's flag: the session needs a person.
    ///
    /// # Errors
    ///
    /// As [`EmergencyGrant::confirm`].
    pub fn flag(&mut self, by: &str, at: i64, reason: &str) -> Result<&ReviewTask, QuestionsError> {
        self.record_check(by, at, false, Some(reason))
    }

    fn record_check(
        &mut self,
        by: &str,
        at: i64,
        confirmed: bool,
        note: Option<&str>,
    ) -> Result<&ReviewTask, QuestionsError> {
        let grant = self.id().to_owned();
        let Some(review) = &mut self.review else {
            return Err(QuestionsError::EmergencyState {
                grant,
                detail: "no review is open".to_owned(),
            });
        };
        if !review.is_pending() {
            return Err(QuestionsError::EmergencyState {
                grant,
                detail: format!("the review is {:?}", review.state),
            });
        }
        if by == review.user {
            return Err(QuestionsError::ReviewSelf {
                review: review.id.clone(),
                detail: by.to_owned(),
            });
        }
        if !review.may_decide(by) {
            return Err(QuestionsError::ReviewWrongPerson {
                review: review.id.clone(),
                detail: by.to_owned(),
            });
        }
        review.checks.push(ReviewCheck {
            by: by.to_owned(),
            at,
            confirmed,
            note: note.map(str::to_owned),
        });
        if !confirmed {
            review.state = ReviewState::Flagged {
                at,
                reason: note.unwrap_or_default().to_owned(),
            };
        } else if u8::try_from(review.checks.len()).unwrap_or(u8::MAX) >= review.checks_required {
            review.state = ReviewState::Confirmed { at };
        }
        self.review
            .as_ref()
            .ok_or_else(|| QuestionsError::EmergencyState {
                grant: self.id().to_owned(),
                detail: "the review vanished".to_owned(),
            })
    }

    /// An unconfirmed review whose deadline passed escalates, through the engine's timer port.
    ///
    /// Returns the escalation when one happened. A review that reaches the policy's maximum
    /// escalations stays escalated rather than re-arming.
    pub fn escalate_due(&mut self, timers: &dyn TimerPort, now: i64) -> Option<Escalation> {
        let review = self.review.as_ref()?;
        if !review.is_pending() || now < review.due_at {
            return None;
        }
        let fired = timers.due(offset(now));
        if !fired
            .iter()
            .any(|t| matches!(&t.event, Some(event) if event == &self.review_event()))
        {
            return None;
        }
        let review = self.review.as_mut()?;
        review.escalations = review.escalations.saturating_add(1);
        review.state = ReviewState::Escalated {
            count: review.escalations,
            at: now,
        };
        let grant = review.grant.clone();
        let escalation = Escalation {
            grant,
            review: review.id.clone(),
            to: review.escalate_to.clone(),
            count: review.escalations,
            at: now,
        };
        if review.escalations < self.policy.max_escalations {
            let review = self.review.as_mut()?;
            review.state = ReviewState::Pending;
            review.due_at = now
                .saturating_add(i64::try_from(self.policy.escalate_after_secs).unwrap_or(i64::MAX));
            self.schedule_review(timers);
        }
        Some(escalation)
    }

    /// The event key an unconfirmed review's timer fires under.
    #[must_use]
    pub fn review_event(&self) -> String {
        format!("emergency.review:{}", self.id())
    }

    /// Registers (or re-registers) the review's escalation timer with the engine's timer port.
    fn schedule_review(&self, timers: &dyn TimerPort) {
        let Some(review) = &self.review else {
            return;
        };
        let (Some(case), Some(node)) = (review_case(review), review_node()) else {
            return;
        };
        timers.register(TimerRegistration {
            case,
            node,
            attempt: 0,
            due: Some(offset(review.due_at)),
            event: Some(self.review_event()),
            human: None,
        });
    }

    /// A governance refusal as this crate's error, keeping the governance code in the detail.
    fn approval_error(&self, error: &GovError) -> QuestionsError {
        QuestionsError::Approval {
            request: self.id().to_owned(),
            detail: format!("{}: {error}", error.code()),
        }
    }

    fn wrong_phase(&self, action: &str) -> QuestionsError {
        QuestionsError::EmergencyState {
            grant: self.id().to_owned(),
            detail: format!("cannot {action} a grant that is {}", self.phase.label()),
        }
    }
}

/// The `OffsetDateTime` for an integer epoch second, saturating for values outside the range.
fn offset(at: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(at).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// The optional instant for a validity bound.
fn instant(at: i64) -> Option<OffsetDateTime> {
    Some(offset(at))
}

/// The case id a review's timer is registered under: derived from the review id, made valid
/// for the engine's id grammar.
fn review_case(review: &ReviewTask) -> Option<CaseId> {
    let mut raw = String::with_capacity(review.id.len() + 8);
    raw.push_str("emergency_");
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
    CaseId::new(&raw).ok()
}

/// The review node id.
fn review_node() -> Option<NodeId> {
    NodeId::new(EMERGENCY_REVIEW_NODE).ok()
}
