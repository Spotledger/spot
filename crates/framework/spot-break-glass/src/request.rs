//! The break-glass request, its approvers and the composition helpers that turn it into
//! the grant models of `spot-deep-access` and `spot-support-session`.
//!
//! A [`Request`] is data and conveys no authority: it names who is asking, why, for which
//! ticket, over which window and scope. Nothing here reads a clock or a store.

use spot_deep_access::AccessRequest;
use spot_support_session::{Scope, SessionSpec, Subject, SubjectKind, Tier};

use crate::error::BreakGlassError;
use crate::policy::Policy;

/// Which side of the boundary a principal stands on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrincipalKind {
    /// The vendor (platform support): the only side that may approve or request.
    Vendor,
    /// The customer (tenant): consents through a customer-signed receipt.
    Customer,
}

/// A named principal, on one side of the boundary.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Principal {
    /// Which side the principal is on.
    pub kind: PrincipalKind,
    /// The principal's identity, as the control plane knows it.
    pub id: String,
}

impl Principal {
    /// A principal from a side and an id.
    #[must_use]
    pub fn new(kind: PrincipalKind, id: &str) -> Principal {
        Principal {
            kind,
            id: id.to_owned(),
        }
    }

    /// A vendor-side principal.
    #[must_use]
    pub fn vendor(id: &str) -> Principal {
        Principal::new(PrincipalKind::Vendor, id)
    }

    /// A customer-side principal.
    #[must_use]
    pub fn customer(id: &str) -> Principal {
        Principal::new(PrincipalKind::Customer, id)
    }
}

/// One vendor-side approval of a request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    /// The approving principal (always vendor-side).
    pub approver: Principal,
    /// When the approval was made (unix seconds).
    pub at: i64,
}

/// A recorded customer consent: who consented, when, and a commitment to the exact
/// receipt the customer signed (the deep-access receipt hash, when the customer-signed
/// path is used).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomerConsent {
    /// The consenting customer principal.
    pub approver: Principal,
    /// When the consent was recorded (unix seconds).
    pub at: i64,
    /// The deep-access consent receipt commitment, when a signed receipt backs it.
    pub receipt_hash: Option<[u8; 32]>,
}

/// A just-in-time break-glass request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The caller-assigned request id.
    pub id: String,
    /// The tenant the session is about.
    pub tenant: String,
    /// The vendor-side requester's identity.
    pub requester: String,
    /// Whether the requester acts as a person or an agent (for the composed grant).
    pub requester_kind: SubjectKind,
    /// Why the session is needed; never blank.
    pub reason: String,
    /// The support ticket the session is against; never blank.
    pub ticket_id: String,
    /// What the session is scoped to (the `spot-support-session` scope).
    pub scope: Scope,
    /// The tier ceiling nothing inside the session may rise above.
    pub tier_ceiling: Tier,
    /// The instant the session may start acting, inclusive (unix seconds).
    pub starts_at: i64,
    /// The instant the session ends, exclusive (unix seconds).
    pub expires_at: i64,
    /// When the request was made (unix seconds).
    pub requested_at: i64,
}

impl Request {
    /// Validates the request against the policy and the current instant: opt-in, a
    /// non-blank requester, reason and ticket, a well-formed half-open window that has
    /// not been asked to start before it was requested, and a duration within the
    /// policy's effective cap.
    ///
    /// # Errors
    ///
    /// The first failing [`BreakGlassError`].
    pub fn validate(&self, policy: &Policy, now: i64) -> Result<(), BreakGlassError> {
        policy.validate()?;
        if self.requester.trim().is_empty() {
            return Err(BreakGlassError::RequesterBlank);
        }
        if self.reason.trim().is_empty() {
            return Err(BreakGlassError::ReasonRequired);
        }
        if self.ticket_id.trim().is_empty() {
            return Err(BreakGlassError::TicketRequired);
        }
        if self.requested_at > now {
            return Err(BreakGlassError::WindowInvalid);
        }
        if self.starts_at < self.requested_at || self.expires_at <= self.starts_at {
            return Err(BreakGlassError::WindowInvalid);
        }
        let asked = self.expires_at - self.starts_at;
        let max = policy.max_duration();
        if asked > max {
            return Err(BreakGlassError::DurationOverPolicy { asked, max });
        }
        Ok(())
    }

    /// The session subject for the composed grant models: the requester, as the given
    /// kind of principal.
    #[must_use]
    pub fn subject(&self) -> Subject {
        Subject::new(self.requester_kind, &self.requester)
    }

    /// The `spot-support-session` spec this request composes to: the scoped, time-boxed
    /// grant model, with the requester as subject and the vendor requester as the
    /// `requested_by` identity.
    #[must_use]
    pub fn session_spec(&self) -> SessionSpec {
        SessionSpec {
            id: self.id.clone(),
            tenant: self.tenant.clone(),
            subject: self.subject(),
            tier_ceiling: self.tier_ceiling,
            scope: self.scope.clone(),
            reason: self.reason.clone(),
            requested_by: self.requester.clone(),
            starts_at: self.starts_at,
            expires_at: self.expires_at,
        }
    }

    /// The `spot-deep-access` request this request composes to: the customer-signed,
    /// hardware-bound grant model, whose subject is the requester and whose spec is
    /// [`Request::session_spec`].
    #[must_use]
    pub fn access_request(&self) -> AccessRequest {
        AccessRequest {
            id: self.id.clone(),
            requester: self.subject(),
            reason: self.reason.clone(),
            ticket_ref: self.ticket_id.clone(),
            spec: self.session_spec(),
            requested_at: self.requested_at,
        }
    }
}
