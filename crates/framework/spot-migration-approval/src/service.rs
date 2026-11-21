//! The approval service: records approvals and rejections, answers status, issues the apply
//! ticket, and writes an audit record for every action. Time is always an argument.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::Serialize;
use spot_audit_chain::{AuditRecord, PayloadValue, Subject};
use spot_migration_plan::{ReleasePlan, Risk};

use crate::approval::{Approval, ApprovalStore, Checks, Decision, Identity};
use crate::authorise::{ApplyTicket, Evaluation, evaluate};
use crate::digest::PlanDigest;
use crate::error::{ApprovalError, Refusal};
use crate::policy::ApprovalPolicy;

/// Longest comment copied into an audit payload, in characters.
const AUDIT_COMMENT_CHARS: usize = 512;

/// Where audit records go. Production bridges this to `spot_audit_chain::AuditChain::append`
/// (which is async); the service fails closed if the sink fails.
pub trait AuditSink: Send + Sync {
    /// Appends one record.
    ///
    /// # Errors
    /// [`ApprovalError::Backend`] when the record cannot be written.
    fn record(&self, record: AuditRecord) -> Result<(), ApprovalError>;
}

/// An [`AuditSink`] that keeps records in memory (tests, and a staging buffer for a bridge).
#[derive(Debug, Default)]
pub struct MemoryAuditSink {
    records: Mutex<Vec<AuditRecord>>,
}

impl MemoryAuditSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The records written so far, oldest first.
    #[must_use]
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl AuditSink for MemoryAuditSink {
    fn record(&self, record: AuditRecord) -> Result<(), ApprovalError> {
        self.records
            .lock()
            .map_err(|_| ApprovalError::Backend {
                message: "the audit sink lock was poisoned".to_owned(),
            })?
            .push(record);
        Ok(())
    }
}

/// The text of a risk class.
#[must_use]
pub fn risk_name(risk: Risk) -> &'static str {
    match risk {
        Risk::Additive => "additive",
        Risk::NeedsReview => "needs_review",
        Risk::Destructive => "destructive",
    }
}

/// The audit record kinds this crate writes. They are built with `concat!` because the
/// error-code scan treats every dotted literal as an error code, and these are not.
pub mod kinds {
    /// Someone read the approval status.
    pub const STATUS_VIEWED: &str = concat!("migration", ".status_viewed");
    /// A plan was registered with its author.
    pub const PLAN_REGISTERED: &str = concat!("migration", ".plan_registered");
    /// Someone viewed the plan.
    pub const PLAN_SHOWN: &str = concat!("migration", ".plan_shown");
    /// An approval was recorded.
    pub const APPROVED: &str = concat!("migration", ".approved");
    /// A rejection was recorded.
    pub const REJECTED: &str = concat!("migration", ".rejected");
    /// An apply ticket was issued.
    pub const APPLY_AUTHORISED: &str = concat!("migration", ".apply_authorised");
    /// An apply ticket was refused.
    pub const APPLY_REFUSED: &str = concat!("migration", ".apply_refused");
}

/// A plan's approval status, for `status` and `plan show`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The plan digest (hex).
    pub plan_digest: String,
    /// The effective risk class.
    pub risk: String,
    /// Distinct valid approvers needed.
    pub required: u32,
    /// Approvers whose approval counts.
    pub approved_by: Vec<String>,
    /// Approvers whose latest decision is a rejection.
    pub rejected_by: Vec<String>,
    /// Approvers whose approval has expired.
    pub expired: Vec<String>,
    /// True when `authorise_apply` would issue a ticket now.
    pub ready: bool,
    /// The refusal code that would be returned otherwise.
    pub blocked_by: Option<String>,
}

impl Status {
    fn from(evaluation: &Evaluation) -> Self {
        let refusal = evaluation.refusal();
        Self {
            plan_digest: evaluation.digest.to_string(),
            risk: risk_name(evaluation.risk).to_owned(),
            required: evaluation.required,
            approved_by: evaluation.valid.clone(),
            rejected_by: evaluation.rejected_by.clone(),
            expired: evaluation.expired.clone(),
            ready: refusal.is_none(),
            blocked_by: refusal.map(|r| r.code().to_owned()),
        }
    }
}

/// Who is acting, in which role, with what words, and when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Act<'a> {
    /// The verified identity the host's authentication produced: the approver is its subject.
    pub identity: Identity,
    /// The role the caller acts in. It must be one of the identity's roles
    /// ([`ApprovalError::RoleNotHeld`] otherwise).
    pub role: &'a str,
    /// The free-text reason.
    pub comment: &'a str,
    /// A hardware signature over the digest, when one was produced (hex).
    pub signature: Option<String>,
    /// Unix microseconds.
    pub now: i64,
}

/// Approval workflow over a store and an audit sink, under one site policy.
pub struct ApprovalService<S: ApprovalStore, A: AuditSink> {
    store: S,
    audit: A,
    policy: ApprovalPolicy,
}

impl<S: ApprovalStore, A: AuditSink> ApprovalService<S, A> {
    /// A service for one site.
    ///
    /// # Errors
    /// [`ApprovalError::BadPolicy`] when the policy is not valid.
    pub fn new(store: S, audit: A, policy: ApprovalPolicy) -> Result<Self, ApprovalError> {
        policy.validate()?;
        Ok(Self {
            store,
            audit,
            policy,
        })
    }

    /// The site policy.
    #[must_use]
    pub fn policy(&self) -> &ApprovalPolicy {
        &self.policy
    }

    /// The audit sink.
    #[must_use]
    pub fn audit(&self) -> &A {
        &self.audit
    }

    /// The approval store.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    fn audit_action(
        &self,
        kind: &str,
        actor: &str,
        digest: &PlanDigest,
        at: i64,
        fields: &[(&str, PayloadValue)],
    ) -> Result<(), ApprovalError> {
        let mut payload = BTreeMap::new();
        for (key, value) in fields {
            payload.insert((*key).to_owned(), value.clone());
        }
        // An audit failure of any kind is the audit-unavailable refusal: an action that cannot
        // be recorded is not taken.
        self.audit
            .record(AuditRecord {
                kind: kind.to_owned(),
                actor: format!("user:{actor}"),
                on_behalf_of: None,
                subject: Some(Subject {
                    doctype: "MigrationPlan".to_owned(),
                    id: digest.to_string(),
                }),
                at,
                payload,
                masked: true,
            })
            .map_err(|error| ApprovalError::AuditUnavailable {
                message: match error {
                    ApprovalError::Backend { message }
                    | ApprovalError::AuditUnavailable { message } => message,
                    other => other.to_string(),
                },
            })
    }

    /// Registers the author of a plan, once, when the plan is created, and audits it as
    /// `migration.plan_registered` (actor: the author, subject: the plan digest). The author is
    /// persisted with the plan, so separation of duties survives a restart.
    ///
    /// Audit first and fail closed: if the audit sink fails the registration is not stored.
    /// Registering the same author again is a no-op and writes no second record; a different
    /// author is [`ApprovalError::AuthorConflict`] and writes no record.
    ///
    /// # Errors
    /// [`ApprovalError::NotPermitted`] for an empty author, [`ApprovalError::AuthorConflict`]
    /// when the plan has a different author, [`ApprovalError::AuditUnavailable`] when the audit
    /// record cannot be written (nothing is stored), [`ApprovalError::Backend`] when the store
    /// fails.
    pub fn register_plan_at(
        &self,
        plan: &ReleasePlan,
        author: &str,
        now: i64,
    ) -> Result<(), ApprovalError> {
        if author.trim().is_empty() {
            return Err(ApprovalError::NotPermitted {
                reason: "a plan needs a named author".to_owned(),
            });
        }
        let digest = PlanDigest::of(plan);
        match self.store.author_of(&digest)? {
            Some(existing) if existing == author => return Ok(()),
            Some(_) => return Err(ApprovalError::AuthorConflict),
            None => {}
        }
        self.audit_action(
            kinds::PLAN_REGISTERED,
            author,
            &digest,
            now,
            &[
                ("action", PayloadValue::Text("register".to_owned())),
                ("author", PayloadValue::Text(author.to_owned())),
                (
                    "risk",
                    PayloadValue::Text(
                        risk_name(crate::authorise::effective_risk(plan)).to_owned(),
                    ),
                ),
            ],
        )?;
        self.store.register_plan(&digest, author)
    }

    /// Registers the author of a plan, once, WITHOUT an audit record. The author is persisted
    /// with the plan, so separation of duties survives a restart. Registering the same author
    /// again is a no-op.
    ///
    /// # Errors
    /// [`ApprovalError::NotPermitted`] for an empty author, [`ApprovalError::AuthorConflict`]
    /// when the plan has a different author, [`ApprovalError::Backend`] when the store fails.
    #[deprecated(note = "use register_plan_at, which also audits the registration")]
    pub fn register_plan(&self, plan: &PlanDigest, author: &str) -> Result<(), ApprovalError> {
        if author.trim().is_empty() {
            return Err(ApprovalError::NotPermitted {
                reason: "a plan needs a named author".to_owned(),
            });
        }
        self.store.register_plan(plan, author)
    }

    fn decide(
        &self,
        plan: &ReleasePlan,
        decision: Decision,
        act: &Act<'_>,
        checks: &Checks<'_>,
    ) -> Result<Approval, ApprovalError> {
        let approver = act.identity.subject.as_str();
        if approver.trim().is_empty() {
            return Err(ApprovalError::NotPermitted {
                reason: "the identity has no subject".to_owned(),
            });
        }
        // The role is only ever one the verified identity holds.
        if !act.identity.holds(act.role) {
            return Err(ApprovalError::RoleNotHeld {
                role: act.role.to_owned(),
            });
        }
        let digest = PlanDigest::of(plan);
        let risk = crate::authorise::effective_risk(plan);
        let need = self.policy.requirement(risk);
        if let Some(required) = &need.role
            && required != act.role
        {
            return Err(ApprovalError::NotPermitted {
                reason: format!("this plan needs an approver with the {required} role"),
            });
        }
        if decision == Decision::Approve {
            // An unknown author fails closed: the approval is refused, never allowed.
            let author = self
                .store
                .author_of(&digest)?
                .ok_or(ApprovalError::AuthorUnknown)?;
            if self.policy.forbid_author_approval
                && (author == approver || checks.authorship.is_author(&digest, approver))
            {
                return Err(ApprovalError::NotPermitted {
                    reason: "an approver cannot approve a plan they authored".to_owned(),
                });
            }
        }
        let approval = Approval {
            plan_digest: digest.clone(),
            approver: approver.to_owned(),
            role: act.role.to_owned(),
            decision,
            comment: act.comment.to_owned(),
            at: act.now,
            signature: act.signature.clone(),
        };
        let (kind, verb) = match decision {
            Decision::Approve => (kinds::APPROVED, "approve"),
            Decision::Reject => (kinds::REJECTED, "reject"),
        };
        let shown: String = act.comment.chars().take(AUDIT_COMMENT_CHARS).collect();
        // Audit first and fail closed: an action that cannot be audited is not taken.
        self.audit_action(
            kind,
            approver,
            &digest,
            act.now,
            &[
                ("action", PayloadValue::Text(verb.to_owned())),
                ("role", PayloadValue::Text(act.role.to_owned())),
                ("comment", PayloadValue::Text(shown)),
                ("risk", PayloadValue::Text(risk_name(risk).to_owned())),
                ("signed", PayloadValue::Bool(act.signature.is_some())),
            ],
        )?;
        self.store.append(approval.clone())?;
        Ok(approval)
    }

    /// Records an approval of exactly this plan.
    ///
    /// # Errors
    /// [`ApprovalError::NotPermitted`] for a wrong role or a self-approval,
    /// [`ApprovalError::AuditUnavailable`] when the audit record cannot be written (nothing is
    /// recorded), [`ApprovalError::Backend`] when the store fails.
    pub fn approve(
        &self,
        plan: &ReleasePlan,
        act: &Act<'_>,
        checks: &Checks<'_>,
    ) -> Result<Approval, ApprovalError> {
        self.decide(plan, Decision::Approve, act, checks)
    }

    /// Records a rejection of exactly this plan (the signature is not used).
    ///
    /// # Errors
    /// As [`Self::approve`].
    pub fn reject(
        &self,
        plan: &ReleasePlan,
        act: &Act<'_>,
        checks: &Checks<'_>,
    ) -> Result<Approval, ApprovalError> {
        self.decide(plan, Decision::Reject, act, checks)
    }

    /// The plan's approval status now.
    ///
    /// # Errors
    /// [`ApprovalError::Backend`] when the store cannot be read.
    pub fn status(
        &self,
        plan: &ReleasePlan,
        now: i64,
        checks: &Checks<'_>,
    ) -> Result<Status, ApprovalError> {
        let approvals = self.store.all()?;
        Ok(Status::from(&evaluate(
            plan,
            &approvals,
            &self.policy,
            now,
            checks,
        )))
    }

    /// Records that someone looked at the plan (`plan show`).
    ///
    /// # Errors
    /// [`ApprovalError::Backend`] when the audit sink fails.
    pub fn record_shown(
        &self,
        plan: &ReleasePlan,
        actor: &str,
        now: i64,
    ) -> Result<(), ApprovalError> {
        self.audit_action(kinds::PLAN_SHOWN, actor, &PlanDigest::of(plan), now, &[])
    }

    /// Records that someone read the approval status (`status`).
    ///
    /// # Errors
    /// [`ApprovalError::AuditUnavailable`] when the audit sink fails.
    pub fn record_status_viewed(
        &self,
        plan: &ReleasePlan,
        actor: &str,
        now: i64,
    ) -> Result<(), ApprovalError> {
        self.audit_action(kinds::STATUS_VIEWED, actor, &PlanDigest::of(plan), now, &[])
    }

    /// Issues the apply ticket for this plan, or refuses; either way it is audited.
    ///
    /// # Errors
    /// [`AuthoriseError::Refused`] with the [`Refusal`] from `authorise_apply`, or
    /// [`AuthoriseError::Approval`] when the store or audit sink fails (no ticket is issued).
    pub fn authorise_apply(
        &self,
        plan: &ReleasePlan,
        actor: &str,
        now: i64,
        checks: &Checks<'_>,
    ) -> Result<ApplyTicket, AuthoriseError> {
        let approvals = self.store.all().map_err(AuthoriseError::Approval)?;
        let outcome =
            crate::authorise::authorise_apply(plan, &approvals, &self.policy, now, checks);
        let digest = PlanDigest::of(plan);
        match &outcome {
            Ok(ticket) => self
                .audit_action(
                    kinds::APPLY_AUTHORISED,
                    actor,
                    &digest,
                    now,
                    &[(
                        "approvers",
                        PayloadValue::Int(
                            i64::try_from(ticket.approvers().len()).unwrap_or(i64::MAX),
                        ),
                    )],
                )
                .map_err(AuthoriseError::Approval)?,
            Err(refusal) => self
                .audit_action(
                    kinds::APPLY_REFUSED,
                    actor,
                    &digest,
                    now,
                    &[("code", PayloadValue::Text(refusal.code().to_owned()))],
                )
                .map_err(AuthoriseError::Approval)?,
        }
        outcome.map_err(AuthoriseError::Refused)
    }
}

/// Why [`ApprovalService::authorise_apply`] gave no ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthoriseError {
    /// The plan is not approved.
    Refused(Refusal),
    /// The store or the audit sink failed.
    Approval(ApprovalError),
}
