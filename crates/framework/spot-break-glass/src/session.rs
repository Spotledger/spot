//! The break-glass session state machine: `Request -> Approved -> Active -> Ended |
//! Expired | Revoked`.
//!
//! The machine is pure: it reads no clock (every instant is an argument), touches no
//! store, and composes the two existing grant models rather than re-implementing them —
//! [`spot_deep_access::DeepCredential`] for the customer-signed, hardware-bound grant and
//! [`spot_support_session::SessionGrant`] for the scoped, time-boxed one. Each transition
//! returns the customer-visible [`BreakGlassEvent`] the host appends to the audit chain.
//!
//! Expiry is enforced by the agent's monotonic clock: [`BreakGlassSession::enforce_expiry`]
//! observes the instant, refuses a clock that went backwards, and calls a [`SessionKiller`]
//! at the exact instant the window closes.

use spot_deep_access::DeepCredential;
use spot_support_session::SessionGrant;

use crate::audit::{
    ACTIVE, APPROVED, BreakGlassEvent, COMMAND, CONSENTED, ENDED, EXPIRED, RECORDING, REQUESTED,
    REVOKED,
};
use crate::error::{ALLOW, BreakGlassError};
use crate::policy::Policy;
use crate::recorder::Hash;
use crate::request::{Approval, CustomerConsent, Principal, PrincipalKind, Request};

/// A composed grant: the authority a live session runs under.
///
/// Either the customer-signed, hardware-bound credential from `spot-deep-access`, or the
/// scoped, time-boxed grant from `spot-support-session`. The crate never builds a third
/// model; a caller obtains one through those crates and hands it in.
#[derive(Debug)]
pub enum Grant {
    /// A customer-signed, hardware-bound grant (deep-access).
    Deep(Box<DeepCredential>),
    /// A scoped, time-boxed grant (support-session).
    Scoped(Box<SessionGrant>),
}

impl Grant {
    /// The tenant the grant covers.
    #[must_use]
    pub fn tenant(&self) -> &str {
        match self {
            Grant::Deep(credential) => credential.tenant(),
            Grant::Scoped(grant) => &grant.tenant,
        }
    }

    /// The inclusive start instant.
    #[must_use]
    pub fn starts_at(&self) -> i64 {
        match self {
            Grant::Deep(credential) => credential.not_before(),
            Grant::Scoped(grant) => grant.starts_at,
        }
    }

    /// The exclusive end instant.
    #[must_use]
    pub fn expires_at(&self) -> i64 {
        match self {
            Grant::Deep(credential) => credential.not_after(),
            Grant::Scoped(grant) => grant.expires_at,
        }
    }

    /// Whether this is the customer-signed deep grant.
    #[must_use]
    pub fn is_deep(&self) -> bool {
        matches!(self, Grant::Deep(_))
    }

    /// The deep credential, when this is the deep grant.
    #[must_use]
    pub fn deep(&self) -> Option<&DeepCredential> {
        match self {
            Grant::Deep(credential) => Some(credential.as_ref()),
            Grant::Scoped(_) => None,
        }
    }

    /// The scoped grant, when this is the scoped grant.
    #[must_use]
    pub fn scoped(&self) -> Option<&SessionGrant> {
        match self {
            Grant::Scoped(grant) => Some(grant.as_ref()),
            Grant::Deep(_) => None,
        }
    }
}

/// The session's stored lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    /// Requested, awaiting two distinct vendor-side approvals and consent.
    Requested,
    /// Approved by the required people (and consented, when required).
    Approved,
    /// Active: a grant was composed and the window is running.
    Active,
    /// Ended by its operator or the customer.
    Ended,
    /// Past its window end; killed at the exact instant.
    Expired,
    /// Revoked, final and immediate.
    Revoked,
}

impl State {
    /// The stable lower-case name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            State::Requested => "requested",
            State::Approved => "approved",
            State::Active => "active",
            State::Ended => "ended",
            State::Expired => "expired",
            State::Revoked => "revoked",
        }
    }

    /// Whether the session is over and can never change again.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Ended | State::Expired | State::Revoked)
    }
}

/// What [`BreakGlassSession::enforce_expiry`] did.
#[derive(Clone, Debug)]
pub enum ExpiryOutcome {
    /// The session is still inside its window.
    Live,
    /// The window closed at this instant: the killer was called and the session expired.
    Killed(Box<BreakGlassEvent>),
    /// The session was already over.
    AlreadyOver,
}

/// The boundary that stops a live interactive session. The platform supplies the real one
/// (closing the PTY); [`RecordingKiller`] is the reference used by tests.
pub trait SessionKiller {
    /// Kill the session named `session` at `at`.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError`] when the terminal cannot be stopped; a failure must leave the
    /// caller failing closed.
    fn kill(&mut self, session: &str, at: i64) -> Result<(), BreakGlassError>;
}

/// A killer that records what it was asked to kill, for tests and for the reference host.
#[derive(Debug, Default)]
pub struct RecordingKiller {
    kills: Vec<(String, i64)>,
}

impl RecordingKiller {
    /// The kills recorded so far, in order.
    #[must_use]
    pub fn kills(&self) -> &[(String, i64)] {
        &self.kills
    }
}

impl SessionKiller for RecordingKiller {
    fn kill(&mut self, session: &str, at: i64) -> Result<(), BreakGlassError> {
        self.kills.push((session.to_owned(), at));
        Ok(())
    }
}

/// A just-in-time break-glass session.
#[derive(Debug)]
pub struct BreakGlassSession {
    request: Request,
    policy: Policy,
    state: State,
    approvals: Vec<Approval>,
    consent: Option<CustomerConsent>,
    grant: Option<Grant>,
    activated_at: Option<i64>,
    ended_at: Option<i64>,
    revocation: Option<(String, String, i64)>,
    commands: u64,
    recording_digest: Option<Hash>,
    last_seen: i64,
}

impl BreakGlassSession {
    /// Opens a session in the `Requested` state, validating the request against the
    /// policy, and returns it with the `requested` event to log.
    ///
    /// # Errors
    ///
    /// The first failing [`BreakGlassError`] from [`Request::validate`].
    pub fn new(
        request: Request,
        policy: Policy,
        now: i64,
    ) -> Result<(BreakGlassSession, BreakGlassEvent), BreakGlassError> {
        request.validate(&policy, now)?;
        let event = BreakGlassEvent::new(
            REQUESTED,
            &request.id,
            &request.tenant,
            &request.requester,
            &request.reason,
            ALLOW,
            now,
        );
        let session = BreakGlassSession {
            request,
            policy,
            state: State::Requested,
            approvals: Vec::new(),
            consent: None,
            grant: None,
            activated_at: None,
            ended_at: None,
            revocation: None,
            commands: 0,
            recording_digest: None,
            last_seen: now,
        };
        Ok((session, event))
    }

    /// The request.
    #[must_use]
    pub fn request(&self) -> &Request {
        &self.request
    }

    /// The policy.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The session id.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.request.id
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.request.tenant
    }

    /// The stored lifecycle state.
    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// The vendor-side approvals recorded so far.
    #[must_use]
    pub fn approvals(&self) -> &[Approval] {
        &self.approvals
    }

    /// The recorded customer consent, if any.
    #[must_use]
    pub fn consent(&self) -> Option<&CustomerConsent> {
        self.consent.as_ref()
    }

    /// The composed grant, once the session is active.
    #[must_use]
    pub fn grant(&self) -> Option<&Grant> {
        self.grant.as_ref()
    }

    /// When the session was activated (unix seconds).
    #[must_use]
    pub fn activated_at(&self) -> Option<i64> {
        self.activated_at
    }

    /// When the session ended, expired or was revoked (unix seconds).
    #[must_use]
    pub fn ended_at(&self) -> Option<i64> {
        self.ended_at
    }

    /// The revocation (who, why, when), when the session was revoked.
    #[must_use]
    pub fn revocation(&self) -> Option<(&str, &str, i64)> {
        self.revocation
            .as_ref()
            .map(|(by, reason, at)| (by.as_str(), reason.as_str(), *at))
    }

    /// How many commands the session has recorded.
    #[must_use]
    pub fn commands(&self) -> u64 {
        self.commands
    }

    /// The committed recording head digest, when one was recorded.
    #[must_use]
    pub fn recording_digest(&self) -> Option<Hash> {
        self.recording_digest
    }

    /// The inclusive start instant: the grant's when active, else the request's.
    #[must_use]
    pub fn starts_at(&self) -> i64 {
        self.grant
            .as_ref()
            .map_or(self.request.starts_at, Grant::starts_at)
    }

    /// The exclusive end instant: the grant's when active, else the request's.
    #[must_use]
    pub fn expires_at(&self) -> i64 {
        self.grant
            .as_ref()
            .map_or(self.request.expires_at, Grant::expires_at)
    }

    /// Whether the session is active and inside `[starts_at, expires_at)` at `now`.
    #[must_use]
    pub fn is_live_at(&self, now: i64) -> bool {
        self.state == State::Active && now >= self.starts_at() && now < self.expires_at()
    }

    fn observe(&mut self, now: i64) -> Result<(), BreakGlassError> {
        if now < self.last_seen {
            return Err(BreakGlassError::ClockWentBackwards);
        }
        self.last_seen = now;
        Ok(())
    }

    fn consent_satisfied(&self) -> bool {
        !self.policy.require_customer_consent || self.consent.is_some()
    }

    fn approvals_satisfied(&self) -> bool {
        self.approvals.len() >= self.policy.required_approvers
    }

    /// Records one vendor-side approval.
    ///
    /// The approver must be a vendor-side principal, must differ from the requester, and
    /// may approve only once. When the required number of distinct approvals (and, if the
    /// policy requires it, customer consent) is present, the session moves to `Approved`.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::WrongState`], [`BreakGlassError::ApproverBlank`],
    /// [`BreakGlassError::ApproverNotVendor`],
    /// [`BreakGlassError::RequesterCannotApprove`], [`BreakGlassError::DuplicateApprover`]
    /// or [`BreakGlassError::ClockWentBackwards`].
    pub fn approve(
        &mut self,
        approver: &Principal,
        now: i64,
    ) -> Result<BreakGlassEvent, BreakGlassError> {
        self.observe(now)?;
        if self.state != State::Requested {
            return Err(BreakGlassError::WrongState(
                "approvals are recorded only before the session is approved",
            ));
        }
        if approver.id.trim().is_empty() {
            return Err(BreakGlassError::ApproverBlank);
        }
        if approver.kind != PrincipalKind::Vendor {
            return Err(BreakGlassError::ApproverNotVendor);
        }
        // Compare identities trimmed and case-folded, so `alice` and `Alice ` are one person.
        let same = |a: &str, b: &str| a.trim().eq_ignore_ascii_case(b.trim());
        if same(&approver.id, &self.request.requester) {
            return Err(BreakGlassError::RequesterCannotApprove);
        }
        if self
            .approvals
            .iter()
            .any(|approval| same(&approval.approver.id, &approver.id))
        {
            return Err(BreakGlassError::DuplicateApprover);
        }
        self.approvals.push(Approval {
            approver: approver.clone(),
            at: now,
        });
        if self.approvals_satisfied() && self.consent_satisfied() {
            self.state = State::Approved;
        }
        let reason = format!(
            "{} of {} vendor approvals",
            self.approvals.len(),
            self.policy.required_approvers
        );
        Ok(BreakGlassEvent::new(
            APPROVED,
            &self.request.id,
            &self.request.tenant,
            &approver.id,
            &reason,
            ALLOW,
            now,
        ))
    }

    /// Records the customer's consent.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::WrongState`], [`BreakGlassError::ConsentMismatch`],
    /// [`BreakGlassError::ApproverBlank`] or [`BreakGlassError::ClockWentBackwards`].
    pub fn record_consent(
        &mut self,
        consent: &CustomerConsent,
        now: i64,
    ) -> Result<BreakGlassEvent, BreakGlassError> {
        self.observe(now)?;
        if self.state != State::Requested {
            return Err(BreakGlassError::WrongState(
                "consent is recorded only before the session is approved",
            ));
        }
        if consent.approver.kind != PrincipalKind::Customer {
            return Err(BreakGlassError::ConsentMismatch);
        }
        if consent.approver.id.trim().is_empty() {
            return Err(BreakGlassError::ApproverBlank);
        }
        self.consent = Some(consent.clone());
        if self.approvals_satisfied() && self.consent_satisfied() {
            self.state = State::Approved;
        }
        Ok(BreakGlassEvent::new(
            CONSENTED,
            &self.request.id,
            &self.request.tenant,
            &consent.approver.id,
            "customer consent recorded",
            ALLOW,
            now,
        ))
    }

    /// Activates an approved session under a composed grant.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::NotEnoughApprovers`] while the session is still `Requested`,
    /// [`BreakGlassError::WrongState`] when it is not `Approved`,
    /// [`BreakGlassError::GrantMismatch`] when the grant covers another tenant or a wider
    /// window, or [`BreakGlassError::ClockWentBackwards`].
    pub fn activate(&mut self, grant: Grant, now: i64) -> Result<BreakGlassEvent, BreakGlassError> {
        self.observe(now)?;
        if self.state == State::Requested {
            return Err(BreakGlassError::NotEnoughApprovers {
                have: self.approvals.len(),
                need: self.policy.required_approvers,
            });
        }
        if self.state != State::Approved {
            return Err(BreakGlassError::WrongState(
                "only an approved session can be activated",
            ));
        }
        if grant.tenant() != self.request.tenant
            || grant.starts_at() < self.request.starts_at
            || grant.expires_at() > self.request.expires_at
        {
            return Err(BreakGlassError::GrantMismatch);
        }
        // The grant must be this session's own and no wider than what was approved: a scoped
        // grant for another session or subject, or one above the requested tier, is refused.
        let matches_request = match &grant {
            Grant::Scoped(scoped) => {
                scoped.id == self.request.id
                    && scoped.subject == self.request.subject()
                    && scoped.tier_ceiling <= self.request.tier_ceiling
            }
            Grant::Deep(credential) => credential.tier() <= self.request.tier_ceiling,
        };
        if !matches_request {
            return Err(BreakGlassError::GrantMismatch);
        }
        self.grant = Some(grant);
        self.state = State::Active;
        self.activated_at = Some(now);
        Ok(BreakGlassEvent::new(
            ACTIVE,
            &self.request.id,
            &self.request.tenant,
            &self.request.requester,
            "session active",
            ALLOW,
            now,
        ))
    }

    /// Records one command executed inside a live session. The command's text is never
    /// carried here — only a count — so a command that names a secret cannot leak into
    /// the audit chain.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::NotStarted`], [`BreakGlassError::Expired`] or
    /// [`BreakGlassError::WrongState`] when the session is not live;
    /// [`BreakGlassError::ClockWentBackwards`].
    pub fn command(&mut self, now: i64) -> Result<BreakGlassEvent, BreakGlassError> {
        self.observe(now)?;
        if !self.is_live_at(now) {
            return Err(self.liveness_error(now));
        }
        self.commands += 1;
        Ok(BreakGlassEvent::new(
            COMMAND,
            &self.request.id,
            &self.request.tenant,
            &self.request.requester,
            "command executed",
            ALLOW,
            now,
        )
        .with_commands(self.commands))
    }

    /// Commits the recording head digest for the customer view.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::ClockWentBackwards`].
    pub fn record_recording(
        &mut self,
        digest: Hash,
        now: i64,
    ) -> Result<BreakGlassEvent, BreakGlassError> {
        self.observe(now)?;
        self.recording_digest = Some(digest);
        Ok(BreakGlassEvent::new(
            RECORDING,
            &self.request.id,
            &self.request.tenant,
            &self.request.requester,
            "recording committed",
            ALLOW,
            now,
        )
        .with_recording(digest, self.commands))
    }

    /// Ends a live session.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::WrongState`] when the session is not active;
    /// [`BreakGlassError::ClockWentBackwards`].
    pub fn end(&mut self, reason: &str, now: i64) -> Result<BreakGlassEvent, BreakGlassError> {
        self.observe(now)?;
        if self.state != State::Active {
            return Err(BreakGlassError::WrongState(
                "only an active session can be ended",
            ));
        }
        self.state = State::Ended;
        self.ended_at = Some(now);
        Ok(BreakGlassEvent::new(
            ENDED,
            &self.request.id,
            &self.request.tenant,
            &self.request.requester,
            reason,
            ALLOW,
            now,
        ))
    }

    /// Revokes a session: final and immediate, whatever its state before it is over.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::WrongState`] when the session is already over;
    /// [`BreakGlassError::ClockWentBackwards`].
    pub fn revoke(
        &mut self,
        by: &Principal,
        reason: &str,
        now: i64,
    ) -> Result<BreakGlassEvent, BreakGlassError> {
        self.observe(now)?;
        if self.state.is_terminal() {
            return Err(BreakGlassError::WrongState(
                "the session is already over; revocation is final",
            ));
        }
        self.state = State::Revoked;
        self.ended_at = Some(now);
        self.revocation = Some((by.id.clone(), reason.to_owned(), now));
        Ok(BreakGlassEvent::new(
            REVOKED,
            &self.request.id,
            &self.request.tenant,
            &by.id,
            reason,
            BreakGlassError::Revoked.code(),
            now,
        ))
    }

    /// Enforces expiry against the agent's monotonic clock: while the window is open the
    /// session is `Live`; at the exact instant it closes, the [`SessionKiller`] is called
    /// and the session becomes `Expired`; a session already over is `AlreadyOver`.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::ClockWentBackwards`] when the clock regressed (fail closed), or
    /// the killer's error.
    pub fn enforce_expiry(
        &mut self,
        killer: &mut dyn SessionKiller,
        now: i64,
    ) -> Result<ExpiryOutcome, BreakGlassError> {
        if self.state.is_terminal() {
            return Ok(ExpiryOutcome::AlreadyOver);
        }
        self.observe(now)?;
        if now < self.expires_at() {
            return Ok(ExpiryOutcome::Live);
        }
        killer.kill(&self.request.id, now)?;
        self.state = State::Expired;
        self.ended_at = Some(now);
        Ok(ExpiryOutcome::Killed(Box::new(BreakGlassEvent::new(
            EXPIRED,
            &self.request.id,
            &self.request.tenant,
            "system",
            "window closed",
            BreakGlassError::Expired.code(),
            now,
        ))))
    }

    fn liveness_error(&self, now: i64) -> BreakGlassError {
        if now < self.starts_at() {
            BreakGlassError::NotStarted
        } else if now >= self.expires_at() {
            BreakGlassError::Expired
        } else {
            BreakGlassError::WrongState("the session is not active")
        }
    }
}
