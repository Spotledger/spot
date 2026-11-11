//! Enable and disable: the state machine, and the leaver actions as data (ADR-070 section 6.3).
//!
//! A transition never performs anything. It returns the new user and the list of things the
//! caller must now do, in a fixed order, so the persistence, session and key tasks cannot get
//! the leaver rules wrong by choosing their own order or forgetting a step.
//!
//! Disabling twice is not an error: the second call returns the user unchanged with no
//! actions, so a SCIM retry or a double click is safe.

use spot_events_contract::Actor;
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use crate::anonymise::is_anonymised;
use crate::error::{self as e, IamError, IamErrors, Result};
use crate::model::User;

/// The longest reason, in characters.
pub const MAX_REASON: usize = 500;

/// One thing that must happen because a user left. Data only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LeaverAction {
    /// End every session of the user.
    EndSessions,
    /// Revoke every refresh token of the user.
    RevokeRefreshTokens,
    /// Disable every API key the user owns.
    DisableApiKeys,
    /// Cancel the invitations that are still pending.
    CancelInvitations,
    /// Reassign or flag the open tasks and approvals: to the manager, or a nominated delegate.
    /// `None` means there is nobody to give them to, and they are flagged for an administrator.
    ReassignOpenTasks {
        /// Who receives them.
        to: Option<RecordId>,
    },
    /// Write one audit event for the change.
    WriteAudit,
}

impl LeaverAction {
    /// The position of the action in the fixed order.
    fn rank(&self) -> u8 {
        match self {
            Self::EndSessions => 0,
            Self::RevokeRefreshTokens => 1,
            Self::DisableApiKeys => 2,
            Self::CancelInvitations => 3,
            Self::ReassignOpenTasks { .. } => 4,
            Self::WriteAudit => 5,
        }
    }
}

/// Puts actions in the fixed order and drops repeats of the same kind (the first wins), so the
/// result does not depend on the order the input came in.
pub fn normalise(actions: impl IntoIterator<Item = LeaverAction>) -> Vec<LeaverAction> {
    let mut actions: Vec<LeaverAction> = actions.into_iter().collect();
    // A stable sort keeps the first of two equal ranks first, which `dedup_by_key` then keeps.
    actions.sort_by_key(LeaverAction::rank);
    actions.dedup_by_key(|a| a.rank());
    actions
}

/// What happened to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleChange {
    /// The user was disabled.
    Disabled,
    /// The user was enabled.
    Enabled,
}

/// The facts of the audit event a transition asks for (`WriteAudit`). No personal data beyond
/// the ids: names stay in the directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditNote {
    /// The user the change is about.
    pub subject: RecordId,
    /// Who made the change.
    pub by: Actor,
    /// The stated reason.
    pub reason: String,
    /// When.
    pub at: OffsetDateTime,
    /// What.
    pub change: LifecycleChange,
}

/// The result of a lifecycle change: the user as it must now be stored, and what must happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    /// The user after the change (equal to the input when nothing changed).
    pub new_state: User,
    /// The actions, in the fixed order. Empty when nothing changed.
    pub actions: Vec<LeaverAction>,
    /// The audit event's facts; `Some` exactly when `actions` contains `WriteAudit`.
    pub audit: Option<AuditNote>,
}

impl Transition {
    fn unchanged(user: &User) -> Self {
        Self {
            new_state: user.clone(),
            actions: Vec::new(),
            audit: None,
        }
    }

    /// Whether the transition changed anything.
    pub fn is_noop(&self) -> bool {
        self.actions.is_empty()
    }
}

fn check_reason(reason: &str) -> Result<String> {
    let reason = reason.trim();
    if reason.is_empty()
        || reason.chars().count() > MAX_REASON
        || reason.chars().any(char::is_control)
    {
        return Err(IamErrors::one(IamError::in_field(
            e::REASON_REQUIRED,
            "reason",
            format!("a reason is required, as text of at most {MAX_REASON} characters"),
        )));
    }
    Ok(reason.to_owned())
}

impl User {
    /// Disables the user. Open tasks go to the user's manager.
    pub fn disable(&self, by: &Actor, reason: &str, now: OffsetDateTime) -> Result<Transition> {
        self.disable_to(by, reason, now, None)
    }

    /// Disables the user; open tasks go to `delegate` when one is nominated, else to the manager.
    pub fn disable_to(
        &self,
        by: &Actor,
        reason: &str,
        now: OffsetDateTime,
        delegate: Option<RecordId>,
    ) -> Result<Transition> {
        let reason = check_reason(reason)?;
        if !self.enabled {
            return Ok(Transition::unchanged(self));
        }
        let mut problems = Vec::new();
        if now < self.created_at {
            problems.push(IamError::in_field(
                e::INVALID_STATE,
                "deactivated_at",
                "a user cannot be deactivated before they were created",
            ));
        }
        if delegate == Some(self.id) {
            problems.push(IamError::in_field(
                e::MANAGER_SELF,
                "delegate",
                "open tasks cannot go to the user who is leaving",
            ));
        }
        if let Some(errors) = IamErrors::from_list(problems) {
            return Err(errors);
        }
        let new_state = User {
            enabled: false,
            deactivated_at: Some(now),
            ..self.clone()
        };
        let actions = normalise([
            LeaverAction::WriteAudit,
            LeaverAction::ReassignOpenTasks {
                to: delegate.or(self.manager),
            },
            LeaverAction::CancelInvitations,
            LeaverAction::DisableApiKeys,
            LeaverAction::RevokeRefreshTokens,
            LeaverAction::EndSessions,
        ]);
        Ok(Transition {
            audit: Some(AuditNote {
                subject: self.id,
                by: by.clone(),
                reason,
                at: now,
                change: LifecycleChange::Disabled,
            }),
            new_state,
            actions,
        })
    }

    /// Enables the user again. Nothing that the leaver actions ended comes back: sessions,
    /// tokens and keys are issued afresh. An anonymised user cannot be enabled.
    pub fn enable(&self, by: &Actor, reason: &str, now: OffsetDateTime) -> Result<Transition> {
        let reason = check_reason(reason)?;
        if self.enabled {
            return Ok(Transition::unchanged(self));
        }
        if is_anonymised(self) {
            return Err(IamErrors::one(IamError::new(
                e::ANONYMISED,
                "an anonymised user cannot be enabled again; create a new user",
            )));
        }
        Ok(Transition {
            new_state: User {
                enabled: true,
                deactivated_at: None,
                ..self.clone()
            },
            actions: vec![LeaverAction::WriteAudit],
            audit: Some(AuditNote {
                subject: self.id,
                by: by.clone(),
                reason,
                at: now,
                change: LifecycleChange::Enabled,
            }),
        })
    }
}
