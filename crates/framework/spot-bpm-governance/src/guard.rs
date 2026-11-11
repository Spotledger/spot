//! The pre-create guard: the one decision that keeps a governed record from being created
//! outside its request, and the audit of every exception and refusal.

use std::sync::Arc;

use spot_app_api::context::Actor;
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_permissions::Principal;

use crate::audit::{AuditSink, GovernanceAudit};
use crate::authority::{RequestAuthority, VerifiedRequest};
use crate::context::actor_text;
use crate::decision::{DeferredTo, GovernanceDecision, RefusalReason, Route};
use crate::error::GovernanceError;
use crate::mode::GovernanceMode;
use crate::permission::{OUTSIDE_PROCESS_PERMISSION, OutsideProcess};
use crate::policy::{PolicyChange, PolicyStore};

/// One attempt to create a record, as the guard sees it.
///
/// It carries the company and record type the policy is looked up for, the record being created,
/// who is acting and as which principal, the caller's reason (when it claims the outside-process
/// route), the request step the authority verified for this actor (when it runs inside one), and
/// the instant — time is an argument. The command id is carried for the audit only: it is never
/// parsed into a request context.
#[derive(Clone, Debug)]
pub struct CreateAttempt<'a> {
    /// The company (organisation unit) the record is created in.
    pub company: &'a str,
    /// The record type (DocType storage name).
    pub doctype: &'a str,
    /// The record being created.
    pub document_id: RecordId,
    /// Who is acting.
    pub actor: &'a Actor,
    /// The acting principal, as the permission model sees them.
    pub principal: &'a Principal,
    /// The caller's reason, when it claims the outside-process route.
    pub reason: Option<&'a str>,
    /// The request step the authority verified for this actor, when it runs inside a request.
    pub request: Option<&'a VerifiedRequest>,
    /// The command id, when there is one; for the audit, never for the decision.
    pub command_id: Option<&'a str>,
    /// The business instant of the operation.
    pub now: OffsetDateTime,
}

/// The governance guard: it decides one create attempt and audits every exception and refusal.
///
/// It decides nothing itself about permissions beyond the outside-process permission; it holds
/// four ports: the policy ([`PolicyStore`]), the request authority ([`RequestAuthority`]), the
/// outside-process permission ([`OutsideProcess`]) and the audit sink ([`AuditSink`]).
pub struct GovernanceGuard {
    policy: Arc<dyn PolicyStore>,
    requests: Arc<dyn RequestAuthority>,
    outside: Arc<dyn OutsideProcess>,
    audit: Arc<dyn AuditSink>,
}

impl GovernanceGuard {
    /// A guard over `policy`, `requests`, `outside` and `audit`.
    #[must_use]
    pub fn new(
        policy: Arc<dyn PolicyStore>,
        requests: Arc<dyn RequestAuthority>,
        outside: Arc<dyn OutsideProcess>,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            policy,
            requests,
            outside,
            audit,
        }
    }

    /// The policy store the guard reads through.
    #[must_use]
    pub fn policy(&self) -> &Arc<dyn PolicyStore> {
        &self.policy
    }

    /// The request step the authority verifies `command_id` as, for the acting person `actor`.
    ///
    /// A command id is an idempotency key the caller chooses, so this returns `Some` only for a
    /// grant the engine issued to exactly `actor` for the **current** attempt of its step:
    /// `None` for a forged key, a copied request id, a grant issued to another person or a
    /// replayed old attempt. The controller uses it; the guard never parses a command id.
    #[must_use]
    pub fn authenticate(&self, command_id: &str, actor: &Actor) -> Option<VerifiedRequest> {
        self.requests.authenticate(command_id, &actor_text(actor))
    }

    /// Decides one create attempt.
    ///
    /// - mode `free`: allowed, no audit;
    /// - mode `governed`: allowed inside a valid request, or through the outside-process
    ///   permission with an acceptable reason (audited); otherwise refused and audited;
    /// - mode `process_only`: deferred to the entry crate, with **no** permission or override
    ///   route offered here (ADR-080 section 2.3).
    ///
    /// # Errors
    /// [`GovernanceError::Settings`] or [`GovernanceError::PolicyInvalid`] when the policy cannot
    /// be read, and [`GovernanceError::Audit`] when the audit write fails, so the operation fails
    /// closed.
    pub fn before_create(
        &self,
        attempt: &CreateAttempt<'_>,
    ) -> Result<GovernanceDecision, GovernanceError> {
        let mode = self
            .policy
            .mode_at(attempt.company, attempt.doctype, attempt.now)?;
        match mode {
            GovernanceMode::Free => Ok(GovernanceDecision::allow(Route::Free)),
            GovernanceMode::ProcessOnly => Ok(GovernanceDecision::Defer {
                to: DeferredTo::Entry,
            }),
            GovernanceMode::Governed => self.governed(attempt),
        }
    }

    /// The governed branch: a valid request, the audited outside-process exception, or a refusal.
    fn governed(&self, attempt: &CreateAttempt<'_>) -> Result<GovernanceDecision, GovernanceError> {
        if attempt
            .request
            .is_some_and(|step| self.requests.may_create(step, attempt.doctype))
        {
            return Ok(GovernanceDecision::allow(Route::InCase));
        }

        let holds = self.outside.holds(
            attempt.principal,
            OUTSIDE_PROCESS_PERMISSION,
            attempt.company,
        );
        if holds {
            // A floor of zero would accept an empty reason: the floor is at least one character.
            let min_chars = self.policy.min_reason_chars(attempt.company)?.max(1);
            if let Some(reason) = attempt.reason.map(str::trim)
                && reason.chars().count() >= min_chars
            {
                let entry = GovernanceAudit::outside_process(
                    attempt.actor,
                    attempt.company,
                    attempt.doctype,
                    attempt.document_id,
                    reason,
                    attempt.command_id,
                    attempt.now,
                );
                self.audit.write(&entry)?;
                return Ok(GovernanceDecision::allow(Route::OutsideProcess));
            }
            return self.refuse(
                attempt,
                RefusalReason::ReasonRequired { min_chars },
                "the outside-process reason is blank or shorter than the configured floor",
            );
        }

        let reason = if attempt.request.is_some() {
            RefusalReason::NotInCase
        } else {
            RefusalReason::NoPermission
        };
        self.refuse(
            attempt,
            reason,
            "the record type is governed and the attempt is outside its request without the \
             outside-process permission",
        )
    }

    fn refuse(
        &self,
        attempt: &CreateAttempt<'_>,
        reason: RefusalReason,
        detail: &str,
    ) -> Result<GovernanceDecision, GovernanceError> {
        let entry = GovernanceAudit::refused(
            attempt.actor,
            attempt.company,
            attempt.doctype,
            attempt.document_id,
            GovernanceMode::Governed,
            detail,
            attempt.command_id,
            attempt.now,
        );
        self.audit.write(&entry)?;
        Ok(GovernanceDecision::Refuse { reason })
    }

    /// Records and audits a policy change.
    ///
    /// The change applies to records created at or after [`PolicyChange::effective_at`]. The
    /// audit entry is written **before** the change is recorded, so a change never exists without
    /// its audit; the caller wraps both in the management transaction.
    ///
    /// # Errors
    /// [`GovernanceError::Audit`] when the audit write fails, or
    /// [`GovernanceError::Settings`] when the change log cannot be written.
    pub fn change_policy(
        &self,
        change: PolicyChange,
        actor: &Actor,
    ) -> Result<(), GovernanceError> {
        let detail = format!(
            "{} to {} effective {}",
            change.from, change.to, change.effective_at
        );
        let entry = GovernanceAudit::policy_change(
            actor,
            &change.company,
            &change.doctype,
            change.to,
            &detail,
            change.changed_at,
        );
        self.audit.write(&entry)?;
        self.policy.record_change(change)
    }
}
