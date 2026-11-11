//! The customer-side record: one event per grant transition, approval, session start and
//! stop, action and revocation, appended to the tenant's own audit chain.

use std::collections::BTreeMap;
use std::future::Future;

use spot_audit_chain::{AuditChain, AuditRecord, AuditStore, Payload, PayloadValue, Subject};
use spot_support_session::{SubjectKind, Tier};

use crate::error::GrantError;
use crate::model::{actor_text, subject_kind_text};

/// What a customer-side record entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// The support side asked for the grant.
    Requested,
    /// A customer administrator approved the grant.
    Approved,
    /// The session started acting.
    Started,
    /// One action was attempted (allowed or denied).
    Action,
    /// The session stopped.
    Stopped,
    /// The customer (or the support side) revoked the grant.
    Revoked,
    /// A customer administrator extended the grant (a new approval).
    Extended,
}

impl EventKind {
    /// The stable text form used in the chain and the view: `requested`, `approved`,
    /// `started`, `action`, `stopped`, `revoked`, `extended`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Requested => "requested",
            EventKind::Approved => "approved",
            EventKind::Started => "started",
            EventKind::Action => "action",
            EventKind::Stopped => "stopped",
            EventKind::Revoked => "revoked",
            EventKind::Extended => "extended",
        }
    }

    pub(crate) fn parse(text: &str) -> Option<EventKind> {
        match text {
            "requested" => Some(EventKind::Requested),
            "approved" => Some(EventKind::Approved),
            "started" => Some(EventKind::Started),
            "action" => Some(EventKind::Action),
            "stopped" => Some(EventKind::Stopped),
            "revoked" => Some(EventKind::Revoked),
            "extended" => Some(EventKind::Extended),
            _ => None,
        }
    }
}

/// One customer-visible record entry.
///
/// The event carries the who, the scope, the tier and the outcome in plain words; it never
/// carries action arguments, credentials or tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantEvent {
    /// Which transition this is.
    pub kind: EventKind,
    /// The tenant whose chain carries the entry.
    pub tenant: String,
    /// The grant/session id.
    pub session: String,
    /// Who did this: the requester, the approving administrator, the revoker, or the
    /// subject acting.
    pub who: String,
    /// Whether `who` is an agent or a person.
    pub who_kind: SubjectKind,
    /// The grant's subject: who the access was given to, whatever this event's actor did.
    pub subject: String,
    /// Whether the subject is an agent or a person.
    pub subject_kind: SubjectKind,
    /// The support organisation the grant is made to.
    pub support_org: String,
    /// The tier ceiling of the grant; for an action, the tier the action needed.
    pub tier: Tier,
    /// The scope in plain words.
    pub scope: String,
    /// The action name (action events only).
    pub action: String,
    /// The DocType the action reached, if any.
    pub doctype: Option<String>,
    /// `allowed`, `denied`, or the transition word.
    pub outcome: String,
    /// The stable code of the decision or transition.
    pub code: String,
    /// What the action changed, in plain words; empty for read-only actions and lifecycle
    /// transitions.
    pub change: String,
    /// A human reason: the denial sentence, the revocation reason, the extension summary.
    pub reason: String,
    /// For an extension, the new end instant (unix seconds).
    pub extended_to: Option<i64>,
    /// When it happened (unix seconds).
    pub at: i64,
}

impl GrantEvent {
    fn payload(&self) -> Payload {
        let mut payload = BTreeMap::new();
        let mut text = |key: &str, value: &str| {
            payload.insert(key.to_owned(), PayloadValue::Text(value.to_owned()));
        };
        text("event", self.kind.as_str());
        text("session", &self.session);
        text("who", &self.who);
        text("who_kind", subject_kind_text(self.who_kind));
        text("subject", &self.subject);
        text("subject_kind", subject_kind_text(self.subject_kind));
        text("org", &self.support_org);
        text("tier", self.tier.as_str());
        text("scope", &self.scope);
        text("action", &self.action);
        text("outcome", &self.outcome);
        text("code", &self.code);
        text("change", &self.change);
        text("reason", &self.reason);
        text("at", &self.at.to_string());
        if let Some(doctype) = &self.doctype {
            text("doctype", doctype);
        }
        if let Some(extended_to) = self.extended_to {
            payload.insert("extended_to".to_owned(), PayloadValue::Int(extended_to));
        }
        payload
    }
}

/// The tenant's own log, as the control plane sees it.
///
/// The only operation is append: there is no update, no delete and no truncate, so a
/// vendor-side caller holding this trait cannot suppress an entry. Implementations must
/// durably append to the tenant's `spot-audit-chain` before returning `Ok`.
pub trait CustomerLog {
    /// The tenant whose chain this writer controls.
    fn tenant(&self) -> &str;

    /// Appends exactly one entry to the tenant's chain.
    ///
    /// # Errors
    ///
    /// [`GrantError::AuditUnavailable`] when the chain cannot record it, or
    /// [`GrantError::WrongTenant`] when the event is about another tenant.
    fn append(&mut self, event: GrantEvent) -> Result<(), GrantError>;
}

/// A synchronous boundary for the asynchronous audit-chain dependency.
///
/// The platform supplies its executor; this crate reads no clock and starts no task.
/// Implementations must drive the whole future to completion and must never detach it.
pub trait AuditExecutor {
    /// Completes the operation before returning; a failure must fail closed.
    ///
    /// # Errors
    ///
    /// [`GrantError::AuditUnavailable`] when the future cannot complete.
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, GrantError>;
}

/// The customer's chain as a [`CustomerLog`]: every event becomes one `support.*` record
/// on the tenant's audit chain.
pub struct ChainLog<S: AuditStore, E: AuditExecutor> {
    chain: AuditChain<S>,
    executor: E,
}

impl<S: AuditStore, E: AuditExecutor> ChainLog<S, E> {
    /// Wraps the tenant's chain (its site is the tenant id) and the platform's executor.
    #[must_use]
    pub fn new(chain: AuditChain<S>, executor: E) -> ChainLog<S, E> {
        ChainLog { chain, executor }
    }

    /// The chain, for the customer view.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<S> {
        &self.chain
    }

    /// The executor, for the customer view.
    #[must_use]
    pub fn executor(&self) -> &E {
        &self.executor
    }

    /// The tenant this writer appends to.
    #[must_use]
    pub fn tenant(&self) -> &str {
        self.chain.site()
    }
}

impl<S: AuditStore, E: AuditExecutor> CustomerLog for ChainLog<S, E> {
    fn tenant(&self) -> &str {
        self.chain.site()
    }

    fn append(&mut self, event: GrantEvent) -> Result<(), GrantError> {
        if event.tenant != self.chain.site() {
            return Err(GrantError::WrongTenant {
                asked: event.tenant,
                grant: self.chain.site().to_owned(),
            });
        }
        let subject = Subject {
            doctype: "SupportSession".to_owned(),
            id: event.session.clone(),
        };
        let record = AuditRecord {
            kind: format!("support.{}", event.kind.as_str()),
            actor: "system".to_owned(),
            on_behalf_of: Some(actor_text(event.who_kind, &event.who)),
            subject: Some(subject),
            at: event
                .at
                .checked_mul(1_000_000)
                .ok_or(GrantError::AuditUnavailable)?,
            payload: event.payload(),
            masked: true,
        };
        self.executor
            .run(self.chain.append(record))?
            .map_err(|_| GrantError::AuditUnavailable)?;
        Ok(())
    }
}
