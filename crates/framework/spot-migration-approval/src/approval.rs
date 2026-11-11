//! Approval records, the store they live in, and the two outside checks the rules call
//! through traits (separation of duties and the hardware signature).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::digest::PlanDigest;
use crate::error::ApprovalError;

/// An approver's decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// The approver accepts the plan.
    Approve,
    /// The approver rejects the plan; this blocks apply until they approve again.
    Reject,
}

/// One approver's decision on one exact plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    /// The digest of the plan that was approved; any change to the plan changes it.
    pub plan_digest: PlanDigest,
    /// Who decided (the identity the caller authenticated).
    pub approver: String,
    /// The role the caller verified the approver holds.
    pub role: String,
    /// Approve or reject.
    pub decision: Decision,
    /// The approver's free-text reason.
    pub comment: String,
    /// When, in unix microseconds, supplied by the caller.
    pub at: i64,
    /// A hardware signature over the digest, when the policy asks for one (hex).
    pub signature: Option<String>,
}

/// Where approvals are kept. Append-only: a reject followed by a new approve is two records,
/// and the latest per approver counts.
pub trait ApprovalStore: Send + Sync {
    /// Appends one record.
    ///
    /// # Errors
    /// [`ApprovalError::Backend`] when it cannot be stored.
    fn append(&self, approval: Approval) -> Result<(), ApprovalError>;

    /// Every record ever stored for this plan digest, oldest first.
    ///
    /// # Errors
    /// [`ApprovalError::Backend`] when it cannot be read.
    fn for_digest(&self, digest: &PlanDigest) -> Result<Vec<Approval>, ApprovalError>;

    /// Every record for any digest, oldest first (used to tell "never approved" from "approved
    /// before it was changed").
    ///
    /// # Errors
    /// [`ApprovalError::Backend`] when it cannot be read.
    fn all(&self) -> Result<Vec<Approval>, ApprovalError>;

    /// Records the author of the plan with this digest, once. The same author again is a no-op;
    /// a different author is [`ApprovalError::AuthorConflict`]. The author is persisted with the
    /// plan, so it survives a restart.
    ///
    /// The default refuses (a store that cannot persist authors cannot support approvals, and
    /// the service then fails closed).
    ///
    /// # Errors
    /// [`ApprovalError::AuthorConflict`], or [`ApprovalError::Backend`] when it cannot be stored.
    fn register_plan(&self, _digest: &PlanDigest, _author: &str) -> Result<(), ApprovalError> {
        Err(ApprovalError::Backend {
            message: "this approval store does not persist plan authors".to_owned(),
        })
    }

    /// The recorded author of the plan, or `None` when none is recorded. The default knows no
    /// author, which refuses every approval (fail closed).
    ///
    /// # Errors
    /// [`ApprovalError::Backend`] when it cannot be read.
    fn author_of(&self, _digest: &PlanDigest) -> Result<Option<String>, ApprovalError> {
        Ok(None)
    }
}

impl<T: ApprovalStore + ?Sized> ApprovalStore for Arc<T> {
    fn append(&self, approval: Approval) -> Result<(), ApprovalError> {
        (**self).append(approval)
    }

    fn for_digest(&self, digest: &PlanDigest) -> Result<Vec<Approval>, ApprovalError> {
        (**self).for_digest(digest)
    }

    fn all(&self) -> Result<Vec<Approval>, ApprovalError> {
        (**self).all()
    }

    fn register_plan(&self, digest: &PlanDigest, author: &str) -> Result<(), ApprovalError> {
        (**self).register_plan(digest, author)
    }

    fn author_of(&self, digest: &PlanDigest) -> Result<Option<String>, ApprovalError> {
        (**self).author_of(digest)
    }
}

/// The caller's identity as the host's authentication produced it: the verified subject and the
/// roles that identity holds. A role is never taken from what the caller types.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The authenticated subject.
    pub subject: String,
    /// The roles the subject holds.
    pub roles: Vec<String>,
}

impl Identity {
    /// An identity with these roles.
    #[must_use]
    pub fn new(subject: impl Into<String>, roles: &[&str]) -> Self {
        Self {
            subject: subject.into(),
            roles: roles.iter().map(|role| (*role).to_owned()).collect(),
        }
    }

    /// Whether the identity holds `role`.
    #[must_use]
    pub fn holds(&self, role: &str) -> bool {
        self.roles.iter().any(|held| held == role)
    }
}

/// An in-memory [`ApprovalStore`].
#[derive(Debug, Default)]
pub struct InMemoryApprovalStore {
    records: Mutex<Vec<Approval>>,
    authors: Mutex<BTreeMap<PlanDigest, String>>,
}

impl InMemoryApprovalStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

fn poisoned() -> ApprovalError {
    ApprovalError::Backend {
        message: "the approval store lock was poisoned".to_owned(),
    }
}

impl ApprovalStore for InMemoryApprovalStore {
    fn append(&self, approval: Approval) -> Result<(), ApprovalError> {
        self.records.lock().map_err(|_| poisoned())?.push(approval);
        Ok(())
    }

    fn for_digest(&self, digest: &PlanDigest) -> Result<Vec<Approval>, ApprovalError> {
        let records = self.records.lock().map_err(|_| poisoned())?;
        Ok(records
            .iter()
            .filter(|a| &a.plan_digest == digest)
            .cloned()
            .collect())
    }

    fn all(&self) -> Result<Vec<Approval>, ApprovalError> {
        Ok(self.records.lock().map_err(|_| poisoned())?.clone())
    }

    fn register_plan(&self, digest: &PlanDigest, author: &str) -> Result<(), ApprovalError> {
        let mut authors = self.authors.lock().map_err(|_| poisoned())?;
        match authors.get(digest) {
            Some(existing) if existing == author => Ok(()),
            Some(_) => Err(ApprovalError::AuthorConflict),
            None => {
                authors.insert(digest.clone(), author.to_owned());
                Ok(())
            }
        }
    }

    fn author_of(&self, digest: &PlanDigest) -> Result<Option<String>, ApprovalError> {
        Ok(self
            .authors
            .lock()
            .map_err(|_| poisoned())?
            .get(digest)
            .cloned())
    }
}

/// The separation-of-duties question (answered by `spot-sod` in production): did this person
/// author the plan?
pub trait AuthorshipCheck {
    /// Whether `who` authored the plan with this digest.
    fn is_author(&self, plan: &PlanDigest, who: &str) -> bool;
}

/// An [`AuthorshipCheck`] that knows no author (for tests and sites that do not track it).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoAuthors;

impl AuthorshipCheck for NoAuthors {
    fn is_author(&self, _plan: &PlanDigest, _who: &str) -> bool {
        false
    }
}

/// The hardware-signature hook (backed by `spot-signed-command` where present).
pub trait SignatureHook {
    /// Whether the approval's signature is valid for its plan digest and approver.
    fn verify(&self, digest: &PlanDigest, approval: &Approval) -> bool;
}

/// The outside checks, passed in rather than reached for.
#[derive(Clone, Copy)]
pub struct Checks<'a> {
    /// Separation of duties.
    pub authorship: &'a dyn AuthorshipCheck,
    /// The hardware hook, when the platform has one wired.
    pub signature: Option<&'a dyn SignatureHook>,
}

impl Checks<'_> {
    /// No author tracking and no hardware hook.
    #[must_use]
    pub fn none() -> Checks<'static> {
        Checks {
            authorship: &NoAuthors,
            signature: None,
        }
    }
}
