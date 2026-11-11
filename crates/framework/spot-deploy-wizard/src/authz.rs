//! Who may run a plan.
//!
//! `Prepared::run` used to execute for whoever held a `Prepared`. It now takes an
//! [`Authorised`] token, which [`authorise`] builds only from four pieces of evidence the
//! management layer supplies:
//!
//! 1. an authenticated administrator identity ([`AdminIdentity`]) whose session is live and
//!    who holds [`DEPLOY_PERMISSION`];
//! 2. a completed step-up for that same administrator, no older than
//!    [`STEP_UP_WINDOW_SECS`] ([`StepUp`]);
//! 3. an approval ([`Approval`]) that names the plan id **and** the plan hash [`Prepared`]
//!    produced, given by someone other than the administrator running it, no older than
//!    [`APPROVAL_WINDOW_SECS`];
//! 4. a plan that still matches: a plan that changed has a different hash, so an old
//!    approval does not run it.
//!
//! Each missing or mismatched piece has its own stable code and nothing runs. The token
//! fixes the plan it was built for and the instant it lapses (the earliest of the step-up
//! window, the session end and the approval window).
//!
//! # What the types are
//!
//! This crate does not depend on the identity crates (`spot-iam-access`, `spot-auth`), so
//! the evidence types here are small value types the management adapter fills in from its
//! own session, step-up and approval records. The adapter is the only caller of
//! [`authorise`] outside tests; [`Authorised::for_testing`] exists only under the `testing`
//! feature. The window constants mirror `spot-iam-access`'s default step-up age.
//!
//! Time is an argument. Nothing here reads a clock, and no value here is a secret, so
//! `Debug` is derived.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{
    APPROVAL_EXPIRED, APPROVAL_MISMATCH, APPROVAL_MISSING, APPROVAL_SELF, FORBIDDEN,
    STEP_UP_REQUIRED, UNAUTHENTICATED, WizardError,
};
use crate::plan::PlanId;

/// The permission an administrator needs to run a deployment plan.
pub const DEPLOY_PERMISSION: &str = concat!("deploy", ".run");

/// How recent a completed step-up must be, in seconds (the same default as
/// `spot-iam-access`'s `DEFAULT_STEP_UP_AGE_SECONDS`).
pub const STEP_UP_WINDOW_SECS: i64 = 600;

/// How long an approval of a plan stays usable, in seconds.
pub const APPROVAL_WINDOW_SECS: i64 = 3_600;

/// The hash of a prepared plan: its id and the stamps of the facts it was planned from.
///
/// Two plans for the same topology planned from different facts have different hashes, so an
/// approval of one does not run the other.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlanHash(String);

impl PlanHash {
    /// The hash as hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_hex(hex: String) -> Self {
        Self(hex)
    }
}

impl From<&str> for PlanHash {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl fmt::Display for PlanHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An authenticated administrator, as the management layer's session knows them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminIdentity {
    subject: String,
    permissions: BTreeSet<String>,
    session_expires_at: i64,
}

impl AdminIdentity {
    /// An administrator `subject` whose authenticated session ends at `session_expires_at`
    /// and who holds `permissions`. The management adapter builds this only from a live,
    /// authenticated session.
    #[must_use]
    pub fn new(subject: &str, permissions: &[&str], session_expires_at: i64) -> Self {
        Self {
            subject: subject.to_owned(),
            permissions: permissions.iter().map(|p| (*p).to_owned()).collect(),
            session_expires_at,
        }
    }

    /// The administrator's stable subject.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }
}

/// A completed step-up (a fresh strong re-authentication) for one administrator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepUp {
    subject: String,
    completed_at: i64,
}

impl StepUp {
    /// A step-up `subject` completed at `completed_at`.
    #[must_use]
    pub fn new(subject: &str, completed_at: i64) -> Self {
        Self {
            subject: subject.to_owned(),
            completed_at,
        }
    }
}

/// An approval of one exact plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    plan_id: PlanId,
    plan_hash: PlanHash,
    approver: String,
    approved_at: i64,
}

impl Approval {
    /// `approver` approved the plan `plan_id` with hash `plan_hash` at `approved_at`.
    #[must_use]
    pub fn new(plan_id: PlanId, plan_hash: PlanHash, approver: &str, approved_at: i64) -> Self {
        Self {
            plan_id,
            plan_hash,
            approver: approver.to_owned(),
            approved_at,
        }
    }
}

/// Proof that an administrator may run exactly one prepared plan, until `expires_at`.
///
/// Built only by [`authorise`] (the management adapter) or, in tests,
/// [`Authorised::for_testing`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authorised {
    actor: String,
    plan_id: PlanId,
    plan_hash: PlanHash,
    expires_at: i64,
}

impl Authorised {
    /// The administrator who runs the plan; the actor of every audit entry.
    #[must_use]
    pub fn actor(&self) -> &str {
        &self.actor
    }

    /// The plan the token is for.
    #[must_use]
    pub fn plan_id(&self) -> &PlanId {
        &self.plan_id
    }

    /// The plan hash the token is for.
    #[must_use]
    pub fn plan_hash(&self) -> &PlanHash {
        &self.plan_hash
    }

    /// The instant (exclusive) the token lapses.
    #[must_use]
    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }

    /// A token that skips every check, for tests only.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn for_testing(actor: &str, prepared: &crate::Prepared, expires_at: i64) -> Self {
        Self {
            actor: actor.to_owned(),
            plan_id: prepared.plan().id().clone(),
            plan_hash: prepared.plan_hash().clone(),
            expires_at,
        }
    }
}

/// Builds the [`Authorised`] token for `prepared`, or says which piece is missing or wrong.
///
/// This is the management adapter's entry point: it passes what its session, step-up and
/// approval stores hold (`None` for a piece that does not exist) and the instant `now`.
///
/// # Errors
/// In the order checked: [`UNAUTHENTICATED`] (no identity, empty subject, or session over),
/// [`FORBIDDEN`] (no [`DEPLOY_PERMISSION`]), [`STEP_UP_REQUIRED`] (none, someone else's,
/// older than [`STEP_UP_WINDOW_SECS`], or from the future), [`APPROVAL_MISSING`],
/// [`APPROVAL_MISMATCH`] (another plan id or hash), [`APPROVAL_SELF`] (the approver is the
/// requester, or has no name), [`APPROVAL_EXPIRED`].
pub fn authorise(
    prepared: &crate::Prepared,
    identity: Option<&AdminIdentity>,
    step_up: Option<&StepUp>,
    approval: Option<&Approval>,
    now: i64,
) -> Result<Authorised, WizardError> {
    let Some(identity) = identity.filter(|i| !i.subject.trim().is_empty()) else {
        return Err(WizardError::refused(
            UNAUTHENTICATED,
            "running a deployment needs an authenticated administrator",
        ));
    };
    if now >= identity.session_expires_at {
        return Err(WizardError::refused(
            UNAUTHENTICATED,
            "the administrator's session has ended",
        ));
    }
    if !identity.permissions.contains(DEPLOY_PERMISSION) {
        return Err(WizardError::refused(
            FORBIDDEN,
            "the administrator may not run deployments",
        ));
    }
    let step_up_required = || {
        WizardError::refused(
            STEP_UP_REQUIRED,
            "a recent step-up by this administrator is required",
        )
    };
    let step_up = step_up
        .filter(|s| s.subject == identity.subject)
        .ok_or_else(step_up_required)?;
    if step_up.completed_at > now || now.saturating_sub(step_up.completed_at) > STEP_UP_WINDOW_SECS
    {
        return Err(step_up_required());
    }
    let approval = approval
        .ok_or_else(|| WizardError::refused(APPROVAL_MISSING, "this plan has not been approved"))?;
    if &approval.plan_id != prepared.plan().id() || &approval.plan_hash != prepared.plan_hash() {
        return Err(WizardError::refused(
            APPROVAL_MISMATCH,
            "the approval is for a different plan, or the plan changed since it was approved",
        ));
    }
    let approver = approval.approver.trim();
    if approver.is_empty() || approver.eq_ignore_ascii_case(identity.subject.trim()) {
        return Err(WizardError::refused(
            APPROVAL_SELF,
            "a plan must be approved by someone other than the administrator who runs it",
        ));
    }
    if approval.approved_at > now || now.saturating_sub(approval.approved_at) > APPROVAL_WINDOW_SECS
    {
        return Err(WizardError::refused(
            APPROVAL_EXPIRED,
            "the approval is too old, or dated in the future",
        ));
    }
    let expires_at = identity
        .session_expires_at
        .min(step_up.completed_at.saturating_add(STEP_UP_WINDOW_SECS))
        .min(approval.approved_at.saturating_add(APPROVAL_WINDOW_SECS));
    Ok(Authorised {
        actor: identity.subject.clone(),
        plan_id: approval.plan_id.clone(),
        plan_hash: approval.plan_hash.clone(),
        expires_at,
    })
}
