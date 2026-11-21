//! The customer-visible session log: one event per transition, written to the tenant's
//! tamper-evident audit chain.
//!
//! The chain is the source of truth for [`crate::customer::CustomerView`]: the vendor
//! cannot delete a session or its recording, and the customer reads who, why, when and
//! what was recorded from the same chain they already hold. Events carry no terminal
//! bytes and no secrets — the recording digest, not the content, travels here.

use std::collections::BTreeMap;
use std::future::Future;

use serde::{Deserialize, Serialize};
use spot_audit_chain::{AuditChain, AuditRecord, AuditStore, Checkpoint, PayloadValue, Subject};
use spot_deep_access::AuditExecutor;

use crate::error::BreakGlassError;
use crate::recorder::{Hash, hex};

/// The `requested` transition.
pub const REQUESTED: &str = "requested";
/// An `approved` transition (the two-person rule satisfied, or one approval recorded).
pub const APPROVED: &str = "approved";
/// The `consented` transition (the customer consented).
pub const CONSENTED: &str = "consented";
/// The `active` transition (a grant was composed and the session started).
pub const ACTIVE: &str = "active";
/// A `command` recorded inside a live session.
pub const COMMAND: &str = "command";
/// The `recording` transition (a recording digest was committed).
pub const RECORDING: &str = "recording";
/// The `ended` transition.
pub const ENDED: &str = "ended";
/// The `expired` transition.
pub const EXPIRED: &str = "expired";
/// The `revoked` transition.
pub const REVOKED: &str = "revoked";

/// One customer-visible break-glass transition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BreakGlassEvent {
    /// The stable transition kind (see the `*` constants).
    pub kind: String,
    /// The session id.
    pub session: String,
    /// The tenant the session is about.
    pub tenant: String,
    /// The actor's identity.
    pub who: String,
    /// A short plain-language note: the request reason, or what happened.
    pub reason: String,
    /// The stable decision code.
    pub code: String,
    /// When the transition happened (unix seconds).
    pub at: i64,
    /// The recording head digest, as hex, when this event commits one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording_digest: Option<String>,
    /// How many commands the session had recorded when this event was written.
    #[serde(default)]
    pub commands: u64,
}

impl BreakGlassEvent {
    /// A transition event.
    #[must_use]
    pub fn new(
        kind: &str,
        session: &str,
        tenant: &str,
        who: &str,
        reason: &str,
        code: &str,
        at: i64,
    ) -> BreakGlassEvent {
        BreakGlassEvent {
            kind: kind.to_owned(),
            session: session.to_owned(),
            tenant: tenant.to_owned(),
            who: who.to_owned(),
            reason: reason.to_owned(),
            code: code.to_owned(),
            at,
            recording_digest: None,
            commands: 0,
        }
    }

    /// The same event carrying a recording digest and a command count.
    #[must_use]
    pub fn with_recording(mut self, digest: Hash, commands: u64) -> BreakGlassEvent {
        self.recording_digest = Some(hex(&digest));
        self.commands = commands;
        self
    }

    /// The same event carrying a command count only.
    #[must_use]
    pub fn with_commands(mut self, commands: u64) -> BreakGlassEvent {
        self.commands = commands;
        self
    }
}

/// The injected synchronous boundary for the asynchronous audit-chain dependency.
///
/// This is deliberately the same boundary `spot-deep-access` defines, re-used here so the
/// platform supplies one executor; the error is mapped onto [`BreakGlassError`].
pub trait Executor {
    /// Complete the operation before returning; a failed execution must fail closed.
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, BreakGlassError>;
}

impl<E: AuditExecutor> Executor for E {
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, BreakGlassError> {
        AuditExecutor::run(self, future).map_err(|_| BreakGlassError::AuditUnavailable)
    }
}

/// The tenant-owned writer boundary. Implementations must durably append to the tenant's
/// `spot-audit-chain` before success; it is never passed to the operator.
pub trait SessionLog {
    /// The tenant owning the one chain this writer controls.
    fn tenant(&self) -> &str;
    /// Append exactly one transition.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::AuditUnavailable`] when the chain could not record it.
    fn append(&mut self, event: BreakGlassEvent) -> Result<(), BreakGlassError>;
}

/// A reference in-memory log, for hosts that only need the events (and for tests).
#[derive(Debug, Default)]
pub struct MemoryLog {
    tenant: String,
    events: Vec<BreakGlassEvent>,
}

impl MemoryLog {
    /// An empty log for `tenant`.
    #[must_use]
    pub fn new(tenant: &str) -> MemoryLog {
        MemoryLog {
            tenant: tenant.to_owned(),
            events: Vec::new(),
        }
    }

    /// The events appended so far, in order.
    #[must_use]
    pub fn events(&self) -> &[BreakGlassEvent] {
        &self.events
    }
}

impl SessionLog for MemoryLog {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn append(&mut self, event: BreakGlassEvent) -> Result<(), BreakGlassError> {
        self.events.push(event);
        Ok(())
    }
}

/// The audit-chain-backed log, composing the existing chain with the platform's executor.
pub struct ChainLog<S: AuditStore, E: Executor> {
    chain: AuditChain<S>,
    executor: E,
}

impl<S: AuditStore, E: Executor> ChainLog<S, E> {
    /// Wraps the tenant's chain; its signer and store stay platform-side.
    #[must_use]
    pub fn new(chain: AuditChain<S>, executor: E) -> Self {
        ChainLog { chain, executor }
    }

    /// The chain, for the customer view's verifier.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<S> {
        &self.chain
    }

    /// Obtains the current signed checkpoint for customer-held verification.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::AuditUnavailable`] when the chain has no head or the signer
    /// fails.
    pub fn checkpoint(&self, at: i64) -> Result<Checkpoint, BreakGlassError> {
        let micros = at
            .checked_mul(1_000_000)
            .ok_or(BreakGlassError::AuditUnavailable)?;
        self.executor
            .run(self.chain.checkpoint_now(micros))?
            .map_err(|_| BreakGlassError::AuditUnavailable)?
            .ok_or(BreakGlassError::AuditUnavailable)
    }
}

impl<S: AuditStore, E: Executor> SessionLog for ChainLog<S, E> {
    fn tenant(&self) -> &str {
        self.chain.site()
    }

    fn append(&mut self, event: BreakGlassEvent) -> Result<(), BreakGlassError> {
        let mut payload = BTreeMap::new();
        payload.insert(
            "event".to_owned(),
            PayloadValue::Text(
                serde_json::to_string(&event).map_err(|_| BreakGlassError::AuditUnavailable)?,
            ),
        );
        let record = AuditRecord {
            kind: format!("break_glass.{}", event.kind),
            actor: "system".to_owned(),
            on_behalf_of: Some(format!("user:{}", event.who)),
            subject: Some(Subject {
                doctype: "BreakGlassSession".to_owned(),
                id: event.session,
            }),
            at: event
                .at
                .checked_mul(1_000_000)
                .ok_or(BreakGlassError::AuditUnavailable)?,
            payload,
            masked: true,
        };
        self.executor
            .run(self.chain.append(record))?
            .map_err(|_| BreakGlassError::AuditUnavailable)?;
        Ok(())
    }
}

/// Appends `event` to `log`, failing closed if the log refuses.
///
/// # Errors
///
/// [`BreakGlassError::AuditUnavailable`] when the log refuses, or
/// [`BreakGlassError::GrantRefused`] when the event is about another tenant.
pub fn audit(log: &mut dyn SessionLog, event: &BreakGlassEvent) -> Result<(), BreakGlassError> {
    if log.tenant() != event.tenant {
        return Err(BreakGlassError::AuditUnavailable);
    }
    log.append(event.clone())
}
