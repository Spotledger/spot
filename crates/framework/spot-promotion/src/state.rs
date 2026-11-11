//! The promotion record and its state machine.
//!
//! A promotion is `Requested -> Planned -> Approved -> Rehearsed -> Applied -> Verified`, with
//! `Failed` and `Rejected` as terminal outcomes. Every transition is appended to the promotion's
//! journal — the persisted record a console lists — and the steps that move it forward also
//! record the matching migration-audit event. No transition ever goes backwards: a terminal
//! promotion is never reopened, and a second attempt starts with a new id.

use spot_custom_bundle::{Bundle, Digest};
use spot_environments::EnvRef;
use spot_migration_audit::RunId;

use crate::error::PromotionError;
use crate::plan::PromotionPlan;
use crate::ports::{ApplyEvidence, FailureEvidence, RehearsalEvidence};

/// Where a promotion is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PromotionState {
    /// The promotion is requested; a bundle, a source and a target are named.
    Requested,
    /// The bundle has been planned against the target and nothing blocks it.
    Planned,
    /// The target environment's approval policy is satisfied.
    Approved,
    /// A rehearsal on a clone of the target passed.
    Rehearsed,
    /// The bundle has been written to the target; verification has not run yet.
    Applied,
    /// Verification passed; the promotion is complete.
    Verified,
    /// A rehearsal, apply, or verification failed. The target is at its pre-state where possible.
    Failed,
    /// An approver rejected the plan, or the plan was refused before apply.
    Rejected,
}

impl PromotionState {
    /// The state's stable lower-case name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Planned => "planned",
            Self::Approved => "approved",
            Self::Rehearsed => "rehearsed",
            Self::Applied => "applied",
            Self::Verified => "verified",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
        }
    }

    /// Whether the promotion is finished and never moves again.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Verified | Self::Failed | Self::Rejected)
    }

    /// Whether `next` is a forward edge from this state.
    #[must_use]
    pub fn allows(self, next: PromotionState) -> bool {
        match self {
            Self::Requested => next == Self::Planned,
            Self::Planned => matches!(next, Self::Approved | Self::Rejected),
            Self::Approved => matches!(next, Self::Rehearsed | Self::Applied | Self::Failed),
            Self::Rehearsed => matches!(next, Self::Applied | Self::Failed),
            Self::Applied => matches!(next, Self::Verified | Self::Failed),
            Self::Verified | Self::Failed | Self::Rejected => false,
        }
    }
}

impl std::fmt::Display for PromotionState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One journaled transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transition {
    /// The state the promotion left, or `None` for its first record.
    pub from: Option<PromotionState>,
    /// The state it entered.
    pub to: PromotionState,
    /// When the transition happened, in unix microseconds (an argument, never read here).
    pub at: i64,
    /// The identity that caused it.
    pub actor: String,
    /// One plain-language sentence for a human.
    pub note: String,
}

/// One promotion, as the store keeps it.
///
/// A promotion names its bundle by digest, never by content: the bytes travel separately, and
/// every step refuses a bundle whose digest is not the one the promotion was requested for.
#[derive(Clone, Debug)]
pub struct Promotion {
    id: RunId,
    bundle_digest: Digest,
    from_env: EnvRef,
    to_env: EnvRef,
    requester: String,
    state: PromotionState,
    plan: Option<PromotionPlan>,
    rehearsal: Option<RehearsalEvidence>,
    pre_image: Option<Bundle>,
    apply: Option<ApplyEvidence>,
    failure: Option<FailureEvidence>,
    journal: Vec<Transition>,
    executions: u32,
}

impl Promotion {
    /// A new promotion in [`PromotionState::Requested`].
    ///
    /// The route must advance the environment ladder by exactly one role and stay in one
    /// environment set; skipping an environment is a separate policy decision the ALM-02 model
    /// owns, and this pipeline never assumes it.
    ///
    /// # Errors
    /// [`PromotionError::BadRoute`] when the target is not the next role after the source or the
    /// two addresses name different environment sets, and
    /// [`PromotionError::PolicyInvalid`] when the requester or the bundle name is empty.
    pub(crate) fn request(
        id: RunId,
        bundle: &Bundle,
        from_env: EnvRef,
        to_env: EnvRef,
        requester: String,
        at: i64,
    ) -> Result<Promotion, PromotionError> {
        if from_env.set != to_env.set || to_env.role.rank() != from_env.role.rank() + 1 {
            return Err(PromotionError::BadRoute {
                from: from_env.role.name().to_owned(),
                to: to_env.role.name().to_owned(),
            });
        }
        if requester.is_empty() {
            return Err(PromotionError::PolicyInvalid {
                message: "a promotion names the requester".to_owned(),
            });
        }
        let note = format!(
            "promotion requested for bundle {} from {} to {}",
            bundle.digest(),
            from_env.role.name(),
            to_env.role.name()
        );
        Ok(Self {
            journal: vec![Transition {
                from: None,
                to: PromotionState::Requested,
                at,
                actor: requester.clone(),
                note,
            }],
            id,
            bundle_digest: bundle.digest(),
            from_env,
            to_env,
            requester,
            state: PromotionState::Requested,
            plan: None,
            rehearsal: None,
            pre_image: None,
            apply: None,
            failure: None,
            executions: 0,
        })
    }

    /// The promotion id.
    #[must_use]
    pub fn id(&self) -> &RunId {
        &self.id
    }

    /// The digest of the bundle this promotion is for.
    #[must_use]
    pub fn bundle_digest(&self) -> Digest {
        self.bundle_digest
    }

    /// The environment the bundle comes from.
    #[must_use]
    pub fn from_env(&self) -> &EnvRef {
        &self.from_env
    }

    /// The environment the bundle is promoted to.
    #[must_use]
    pub fn to_env(&self) -> &EnvRef {
        &self.to_env
    }

    /// Who requested the promotion. Never an approver on a `prod` promotion.
    #[must_use]
    pub fn requester(&self) -> &str {
        &self.requester
    }

    /// The current state.
    #[must_use]
    pub fn state(&self) -> PromotionState {
        self.state
    }

    /// The plan, once the promotion has been planned.
    #[must_use]
    pub fn plan(&self) -> Option<&PromotionPlan> {
        self.plan.as_ref()
    }

    /// The rehearsal evidence, once a rehearsal ran.
    #[must_use]
    pub fn rehearsal(&self) -> Option<&RehearsalEvidence> {
        self.rehearsal.as_ref()
    }

    /// The apply evidence, once the bundle was written.
    #[must_use]
    pub fn apply(&self) -> Option<&ApplyEvidence> {
        self.apply.as_ref()
    }

    /// The failure evidence, when the promotion failed.
    #[must_use]
    pub fn failure(&self) -> Option<&FailureEvidence> {
        self.failure.as_ref()
    }

    /// The captured pre-image of the target, kept so a failed verification can restore it.
    #[must_use]
    pub fn pre_image(&self) -> Option<&Bundle> {
        self.pre_image.as_ref()
    }

    /// Every journaled transition, oldest first.
    #[must_use]
    pub fn journal(&self) -> &[Transition] {
        &self.journal
    }

    /// How many execution attempts this promotion has had (resumes included).
    #[must_use]
    pub fn executions(&self) -> u32 {
        self.executions
    }

    /// The run id of the current execution. The first execution is the promotion id; every
    /// resume takes a new one, so a resumed run never collides in the migration audit store.
    ///
    /// # Errors
    /// [`PromotionError::BadId`] when the derived text is not a valid run id.
    pub(crate) fn run_id(&self) -> Result<RunId, PromotionError> {
        if self.executions == 0 {
            return Ok(self.id.clone());
        }
        let text = format!("{}:resume-{}", self.id, self.executions);
        RunId::new(text.clone()).map_err(|_| PromotionError::BadId { message: text })
    }

    /// The run id for the next execution, after counting it.
    pub(crate) fn next_run_id(&mut self) -> Result<RunId, PromotionError> {
        self.executions = self.executions.saturating_add(1);
        self.run_id()
    }

    /// Moves the promotion to `to`, journaling the edge.
    ///
    /// # Errors
    /// [`PromotionError::BadState`] when the edge is not a forward transition.
    pub(crate) fn advance(
        &mut self,
        to: PromotionState,
        at: i64,
        actor: &str,
        note: impl Into<String>,
    ) -> Result<(), PromotionError> {
        let from = self.state;
        if !from.allows(to) {
            return Err(PromotionError::BadState {
                id: self.id.to_string(),
                state: from.to_string(),
                required: format!("a state that may move to {to}"),
            });
        }
        self.journal.push(Transition {
            from: Some(from),
            to,
            at,
            actor: actor.to_owned(),
            note: note.into(),
        });
        self.state = to;
        Ok(())
    }

    /// Records the plan. Callers advance the state separately, so a store write and an audit
    /// event can be ordered around it.
    pub(crate) fn set_plan(&mut self, plan: PromotionPlan) {
        self.plan = Some(plan);
    }

    /// Records the rehearsal evidence.
    pub(crate) fn set_rehearsal(&mut self, evidence: RehearsalEvidence) {
        self.rehearsal = Some(evidence);
    }

    /// Records the captured pre-image.
    pub(crate) fn set_pre_image(&mut self, pre_image: Bundle) {
        self.pre_image = Some(pre_image);
    }

    /// Records the apply evidence.
    pub(crate) fn set_apply(&mut self, evidence: ApplyEvidence) {
        self.apply = Some(evidence);
    }

    /// Records why the promotion failed.
    pub(crate) fn set_failure(&mut self, evidence: FailureEvidence) {
        self.failure = Some(evidence);
    }

    /// Checks that the plan in hand is exactly the approved plan and that the bundle is the one
    /// this promotion names.
    ///
    /// # Errors
    /// [`PromotionError::BundleMismatch`], [`PromotionError::BadState`] when no plan exists, and
    /// [`PromotionError::PlanChanged`] when the stored plan no longer hashes to its digest.
    pub(crate) fn check_plan(&self, bundle: &Bundle) -> Result<&PromotionPlan, PromotionError> {
        let found = bundle.digest();
        if found != self.bundle_digest {
            return Err(PromotionError::BundleMismatch {
                id: self.id.to_string(),
                expected: self.bundle_digest.to_hex(),
                found: found.to_hex(),
            });
        }
        let plan = self.plan.as_ref().ok_or_else(|| PromotionError::BadState {
            id: self.id.to_string(),
            state: self.state.to_string(),
            required: "a planned promotion".to_owned(),
        })?;
        plan.check_unchanged()?;
        Ok(plan)
    }
}
