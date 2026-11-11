//! Grant approval on the BPM engine (ADR-070 section 8, rule 2).
//!
//! A tenant may require approval for chosen roles or duties. The approval is not a private
//! workflow: [`approval_process`] turns an [`ApprovalSpec`] into a small [`ProcessVersion`]
//! built from the `spot-bpm-model` approval-with-escalation template (a Decision node with an
//! SLA timeout and an escalation), and a [`ProcessLauncher`] the host implements starts it on
//! the engine.
//!
//! [`GrantApproval`] is the pure state machine around one request. The request id is the
//! idempotency key: [`GrantApproval::begin`] launches at most one Case however often it is
//! retried, and [`GrantApproval::assignment`] derives the assignment id from the same request
//! id, so a crash between approval and grant is retried without writing a second assignment.
//! A rejected or expired approval leaves nothing granted.

use serde::{Deserialize, Serialize};
use spot_bpm_model::{NodeKind, ProcessVersion, from_text, templates};
use spot_iam_roles::{Assignment, RoleName, Source};

use crate::error::GovError;
use crate::request::GrantRequestId;
use crate::scope::GrantScope;
use crate::target::{EscalationTarget, target_matches};

/// The process name the grant-approval Process runs under.
pub const APPROVAL_PROCESS_NAME: &str = "grant_approval";
/// The app that owns the grant-approval Process.
pub const APPROVAL_PROCESS_APP: &str = "identity";

/// What an approval is about: the request, who is asked, and the SLA.
///
/// Both who is asked and who the SLA escalates to are validated at the type level:
/// a spec cannot be built, or loaded, with an empty or otherwise unusable escalation target.
/// Loading a spec whose `escalate_to` is not a valid target fails the whole load with the
/// request id named and the value never shown.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    deny_unknown_fields,
    rename_all = "snake_case",
    try_from = "ApprovalSpecWire"
)]
pub struct ApprovalSpec {
    /// The idempotent grant request id.
    pub request: GrantRequestId,
    /// Who the grant is for.
    pub user: String,
    /// The role being granted.
    pub role: RoleName,
    /// Where the role applies.
    #[serde(default)]
    pub scope: GrantScope,
    /// Why approval is required.
    pub reason: String,
    /// Who is asked first: a user name or a `role:` reference the BPM task layer resolves.
    pub approver: String,
    /// Who the SLA escalates to: a validated actor id or `role:<name>` reference.
    pub escalate_to: EscalationTarget,
    /// How long the approver has before the approval escalates, in whole seconds.
    pub sla_secs: u64,
}

/// The stored shape of an [`ApprovalSpec`], validated on the way in.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct ApprovalSpecWire {
    request: GrantRequestId,
    user: String,
    role: RoleName,
    #[serde(default)]
    scope: GrantScope,
    reason: String,
    approver: String,
    escalate_to: String,
    sla_secs: u64,
}

impl TryFrom<ApprovalSpecWire> for ApprovalSpec {
    type Error = String;

    fn try_from(wire: ApprovalSpecWire) -> Result<ApprovalSpec, String> {
        // A stored spec is the untrusted path: the whole load fails, the request id is named
        // and the offered value is never echoed.
        let escalate_to = EscalationTarget::new(&wire.escalate_to).map_err(|_| {
            format!(
                "{}: {}: the escalation target is not a valid actor id",
                crate::error::REQUEST_INVALID,
                wire.request.as_str()
            )
        })?;
        Ok(ApprovalSpec {
            request: wire.request,
            user: wire.user,
            role: wire.role,
            scope: wire.scope,
            reason: wire.reason,
            approver: wire.approver,
            escalate_to,
            sla_secs: wire.sla_secs,
        })
    }
}

/// What a launcher returns: the started Case and the content hash of the process it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct LaunchReceipt {
    /// The started Case's id.
    pub case_id: String,
    /// The process version's content hash, so a consumer knows exactly what runs.
    pub process_hash: String,
}

/// Starts a grant-approval Process on the BPM engine.
///
/// The host implements this over `spot-bpm-engine`. It must key the started Case by the
/// request id so that a launch retried after a crash resumes the same Case; this crate's
/// [`GrantApproval::begin`] already avoids relaunching an approval it has recorded as pending.
pub trait ProcessLauncher {
    /// Launches `process` for `request`, or resumes the Case already started for it.
    ///
    /// # Errors
    ///
    /// [`GovError::ApprovalProcess`] when the engine refuses the launch.
    fn launch(
        &self,
        process: &ProcessVersion,
        request: &GrantRequestId,
    ) -> Result<LaunchReceipt, GovError>;
}

/// Builds the grant-approval Process for `spec` from the built-in approval-with-escalation
/// template: a trigger, one Decision node (with the SLA timeout) and an escalation Decision.
///
/// A spec always carries a valid escalation target ([`EscalationTarget`]), so no
/// process can be built that escalates to nobody in particular.
///
/// # Errors
///
/// [`GovError::ApprovalProcess`] when the built-in template does not parse.
pub fn approval_process(spec: &ApprovalSpec) -> Result<ProcessVersion, GovError> {
    let mut version =
        from_text(templates::APPROVAL_ESCALATION).map_err(|e| GovError::ApprovalProcess {
            detail: format!("the approval template did not parse: {e}"),
        })?;
    version.name = APPROVAL_PROCESS_NAME.to_owned();
    version.app = APPROVAL_PROCESS_APP.to_owned();
    version.title = Some("Grant approval".to_owned());
    for node in &mut version.nodes {
        match node.id.as_str() {
            "first" => {
                if let NodeKind::Decision(config) = &mut node.kind {
                    config.title = format!(
                        "Approve granting `{}` to `{}`",
                        spec.role.as_str(),
                        spec.user
                    );
                    config.assignee = Some(spec.approver.clone());
                }
                node.timeout_secs = Some(spec.sla_secs);
            }
            "escalate" => {
                if let NodeKind::Decision(config) = &mut node.kind {
                    config.assignee = Some(spec.escalate_to.as_str().to_owned());
                }
            }
            _ => {}
        }
    }
    Ok(version)
}

/// Where one grant approval is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// The approval has not been launched.
    #[default]
    NotStarted,
    /// The approval is running; a decision is awaited. `launched_at` is when it started.
    Pending {
        /// When the approval started, in integer epoch seconds.
        launched_at: i64,
    },
    /// The approval was granted; the grant may take effect.
    Approved {
        /// Who approved it.
        by: String,
        /// When, in integer epoch seconds.
        at: i64,
    },
    /// The approval was refused; nothing is granted.
    Rejected {
        /// Who rejected it.
        by: String,
        /// When, in integer epoch seconds.
        at: i64,
        /// Why.
        reason: String,
    },
    /// The approval's SLA lapsed with no decision; nothing is granted.
    Expired {
        /// When it expired, in integer epoch seconds.
        at: i64,
    },
}

impl ApprovalState {
    /// A stable text label for this state, used in error details.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            ApprovalState::NotStarted => "not_started",
            ApprovalState::Pending { .. } => "pending",
            ApprovalState::Approved { .. } => "approved",
            ApprovalState::Rejected { .. } => "rejected",
            ApprovalState::Expired { .. } => "expired",
        }
    }

    /// Whether the state is pending.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, ApprovalState::Pending { .. })
    }
}

/// The decision a person made on a pending approval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Approve the grant.
    Approve,
    /// Reject the grant, with a reason.
    Reject {
        /// Why the grant was rejected.
        reason: String,
    },
}

/// The pure state machine around one grant approval, keyed by its request id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct GrantApproval {
    /// What the approval is about.
    pub spec: ApprovalSpec,
    /// Where it is.
    #[serde(default)]
    pub state: ApprovalState,
    /// The started Case, once launched.
    #[serde(default)]
    pub receipt: Option<LaunchReceipt>,
    /// Whether the SLA escalation has fired. Until it has, only the named approver may decide;
    /// afterwards `escalate_to` may too. Absent in records written before it existed (false).
    #[serde(default)]
    pub escalated: bool,
    /// When the escalation fired, in integer epoch seconds, once it has.
    #[serde(default)]
    pub escalated_at: Option<i64>,
}

impl GrantApproval {
    /// A not-yet-launched approval for `spec`.
    #[must_use]
    pub fn new(spec: ApprovalSpec) -> GrantApproval {
        GrantApproval {
            spec,
            state: ApprovalState::NotStarted,
            receipt: None,
            escalated: false,
            escalated_at: None,
        }
    }

    /// Records that the SLA escalation fired on a pending approval, so `escalate_to` may now
    /// decide. Idempotent: a second call keeps the first escalation time.
    ///
    /// # Errors
    ///
    /// [`GovError::ApprovalNotPending`] when the approval is not pending.
    pub fn escalate(&mut self, at: i64) -> Result<(), GovError> {
        if !self.state.is_pending() {
            return Err(self.not_pending());
        }
        if !self.escalated {
            self.escalated = true;
            self.escalated_at = Some(at);
        }
        Ok(())
    }

    /// Launches the approval on `launcher`, at most once.
    ///
    /// The first call builds the Process and launches it; every later call is a no-op that
    /// returns the recorded receipt, so a retried request never starts a second Case.
    ///
    /// # Errors
    ///
    /// [`GovError::ApprovalProcess`] when the Process cannot be built or the launcher refuses.
    pub fn begin(
        &mut self,
        launcher: &dyn ProcessLauncher,
        at: i64,
    ) -> Result<&LaunchReceipt, GovError> {
        if self.receipt.is_none() {
            let process = approval_process(&self.spec)?;
            let receipt = launcher.launch(&process, &self.spec.request)?;
            self.receipt = Some(receipt);
            self.state = ApprovalState::Pending { launched_at: at };
        }
        self.receipt
            .as_ref()
            .ok_or_else(|| GovError::ApprovalProcess {
                detail: "the launcher returned no receipt".to_owned(),
            })
    }

    /// Records a decision on a pending approval, checking that `by` is the named approver.
    ///
    /// The checks run in this order, and a refused decision changes nothing (the approval
    /// stays pending; the caller records the refusal on its audit path):
    ///
    /// 1. the approval must be pending;
    /// 2. `by` may not be the grantee, whatever role they hold;
    /// 3. `by` must be the spec's approver: the same user name, or, when the approver is
    ///    `role:<name>`, a user for whom `holds_role(by, name)` is true. Once
    ///    [`GrantApproval::escalate`] has been called, `escalate_to` is matched the same way
    ///    and also counts; before that it does not.
    ///
    /// `holds_role` is supplied by the caller (actor, role); this crate does no I/O.
    ///
    /// # Errors
    ///
    /// - [`GovError::ApprovalNotPending`] when the approval is not pending;
    /// - [`GovError::ApproverIsGrantee`] when `by` is the grant's user;
    /// - [`GovError::NotTheApprover`] when `by` is not the named approver.
    pub fn decide_with_roles(
        &mut self,
        decision: ApprovalDecision,
        by: &str,
        at: i64,
        holds_role: &dyn Fn(&str, &str) -> bool,
    ) -> Result<(), GovError> {
        if !self.state.is_pending() {
            return Err(self.not_pending());
        }
        if by == self.spec.user {
            return Err(GovError::ApproverIsGrantee {
                request: self.spec.request.as_str().to_owned(),
                detail: format!("`{by}` is the grantee and may not approve their own grant"),
            });
        }
        // An empty or malformed actor id matches nothing, and a target is always well formed.
        let matches = |target: &str| target_matches(target, by, holds_role);
        let is_approver = matches(&self.spec.approver)
            || (self.escalated && matches(self.spec.escalate_to.as_str()));
        if !is_approver {
            return Err(GovError::NotTheApprover {
                request: self.spec.request.as_str().to_owned(),
                detail: format!("`{by}` is not the approver `{}`", self.spec.approver),
            });
        }
        self.state = match decision {
            ApprovalDecision::Approve => ApprovalState::Approved {
                by: by.to_owned(),
                at,
            },
            ApprovalDecision::Reject { reason } => ApprovalState::Rejected {
                by: by.to_owned(),
                at,
                reason,
            },
        };
        Ok(())
    }

    /// Expires a pending approval whose SLA has lapsed. Idempotent once expired.
    ///
    /// # Errors
    ///
    /// [`GovError::ApprovalNotPending`] when the approval is already decided.
    pub fn expire(&mut self, at: i64) -> Result<(), GovError> {
        match self.state {
            ApprovalState::Pending { .. } => {
                self.state = ApprovalState::Expired { at };
                Ok(())
            }
            ApprovalState::Expired { .. } => Ok(()),
            _ => Err(self.not_pending()),
        }
    }

    /// When the approval's SLA lapses, once it has started.
    #[must_use]
    pub fn expires_at(&self) -> Option<i64> {
        match self.state {
            ApprovalState::Pending { launched_at } => Some(
                launched_at.saturating_add(i64::try_from(self.spec.sla_secs).unwrap_or(i64::MAX)),
            ),
            _ => None,
        }
    }

    /// Whether the grant has been approved and may take effect.
    #[must_use]
    pub fn is_granted(&self) -> bool {
        matches!(self.state, ApprovalState::Approved { .. })
    }

    /// The assignment the approved grant becomes.
    ///
    /// The assignment id is derived from the request id, so calling this twice — a retry after
    /// a crash between approval and grant — yields the same assignment.
    ///
    /// # Errors
    ///
    /// [`GovError::NotApproved`] when the approval is not approved: a rejected, expired or
    /// still-pending approval leaves nothing granted.
    pub fn assignment(&self) -> Result<Assignment, GovError> {
        if !self.is_granted() {
            return Err(GovError::NotApproved {
                request: self.spec.request.as_str().to_owned(),
                detail: format!("the approval is {}; nothing is granted", self.state.label()),
            });
        }
        let mut assignment = Assignment::new(
            &self.spec.request.assignment_id(),
            &self.spec.user,
            self.spec.role.as_str(),
        );
        assignment.scope = self.spec.scope.to_scope();
        assignment.source = Source::Manual;
        assignment.reason = Some(self.spec.reason.clone());
        Ok(assignment)
    }

    fn not_pending(&self) -> GovError {
        GovError::ApprovalNotPending {
            request: self.spec.request.as_str().to_owned(),
            detail: format!("the approval is {}", self.state.label()),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn spec() -> ApprovalSpec {
        ApprovalSpec {
            request: GrantRequestId::new("req-1").expect("id"),
            user: "ben".to_owned(),
            role: RoleName::from("Finance Approver"),
            scope: GrantScope::value("Company", "A"),
            reason: "the role needs approval".to_owned(),
            approver: "role:cfo".to_owned(),
            escalate_to: EscalationTarget::new("role:ceo").expect("target"),
            sla_secs: 3600,
        }
    }

    #[test]
    fn the_process_is_the_template_with_the_spec_folded_in() {
        let process = approval_process(&spec()).expect("process");
        assert_eq!(process.name, APPROVAL_PROCESS_NAME);
        let first = process
            .node(&spot_bpm_model::NodeId::new("first").expect("id"))
            .expect("first node");
        assert_eq!(first.timeout_secs, Some(3600));
        match &first.kind {
            NodeKind::Decision(config) => assert_eq!(config.assignee.as_deref(), Some("role:cfo")),
            other => panic!("first is not a decision: {other:?}"),
        }
    }
}
