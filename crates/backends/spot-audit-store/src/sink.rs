//! The agent decision sink: the bridge from the (synchronous) authoriser to the (asynchronous)
//! audit chain, and the audited wrapper that can refuse an action whose audit cannot be recorded.
//!
//! [`AgentDecisionAudit`] is a [`DecisionSink`]: [`DecisionSink::record`] never blocks — it pushes
//! to a bounded channel that a [`DecisionReceiver`] drains into the chain. When the channel is
//! full the record is dropped and an `audit.dropped` counter is raised through the injected
//! [`Telemetry`]; the bare sink cannot refuse an action (its `record` returns `()`), so
//! [`AuditedAuthoriser`] is the fail-closed path: it reserves a channel permit *before* deciding
//! and, when there is none, refuses the action without running it.

use std::sync::Arc;

use spot_agent_tiers::{
    AgentIdentity, Authoriser, Context, Decision, DecisionRecord, DecisionSink,
};
use spot_audit_chain::{AuditChain, AuditRecord, AuditStore, Payload, PayloadValue};
use tokio::sync::mpsc;

/// The counter raised when a decision record is dropped (its name is built with
/// `concat!`, like the signal kinds: it is a metric name, not an error code).
pub const DROPPED_COUNTER: &str = concat!("audit.", "dropped");

/// The record kind every decision audit entry carries (built with `concat!`: a record kind, not
/// an error code).
pub const DECISION_KIND: &str = concat!("agent.", "decision");

/// Counters the sink raises. A bridge to `spot-telemetry` is later work.
pub trait Telemetry: Send + Sync {
    /// Adds `by` to the counter named `name`.
    fn counter(&self, name: &str, by: u64);
}

/// A [`Telemetry`] that counts nothing, for hosts and tests without a metrics handle.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTelemetry;

impl Telemetry for NoTelemetry {
    fn counter(&self, _name: &str, _by: u64) {}
}

/// What to do when the audit channel is full. The default refuses the action.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// An action that cannot be audited does not run (the default).
    #[default]
    FailClosed,
    /// The action runs, unaudited, and the drop is counted.
    FailOpen,
}

/// The audited action could not be recorded, so (under the fail-closed policy) it was not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditUnavailable {
    /// What was refused, and why.
    pub message: String,
}

impl AuditUnavailable {
    /// A refusal with `message`.
    pub fn new(message: impl Into<String>) -> AuditUnavailable {
        AuditUnavailable {
            message: message.into(),
        }
    }

    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        crate::error::UNAVAILABLE
    }
}

impl std::fmt::Display for AuditUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message)
    }
}

impl std::error::Error for AuditUnavailable {}

/// The synchronous [`DecisionSink`] over a bounded channel.
pub struct AgentDecisionAudit {
    tx: mpsc::Sender<DecisionRecord>,
    telemetry: Arc<dyn Telemetry>,
}

/// The reading end of the channel: what a drain pulls decision records from.
pub struct DecisionReceiver {
    rx: mpsc::Receiver<DecisionRecord>,
}

impl AgentDecisionAudit {
    /// A sink with a channel holding up to `capacity` pending records.
    #[must_use]
    pub fn new(
        capacity: usize,
        telemetry: Arc<dyn Telemetry>,
    ) -> (AgentDecisionAudit, DecisionReceiver) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (
            AgentDecisionAudit { tx, telemetry },
            DecisionReceiver { rx },
        )
    }

    /// How many records the channel can still take.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.tx.capacity()
    }

    /// Reserves room for one record, or `None` when the channel is full.
    ///
    /// The [`AuditedAuthoriser`] uses this before deciding; the returned permit guarantees the
    /// `send` cannot fail.
    pub fn try_reserve(&self) -> Option<mpsc::Permit<'_, DecisionRecord>> {
        self.tx.try_reserve().ok()
    }
}

impl DecisionSink for AgentDecisionAudit {
    fn record(&self, entry: DecisionRecord) {
        if self.tx.try_send(entry).is_err() {
            self.telemetry.counter(DROPPED_COUNTER, 1);
        }
    }
}

impl DecisionReceiver {
    /// The next pending record, or `None` when every sender is gone and the channel is empty.
    pub async fn recv(&mut self) -> Option<DecisionRecord> {
        self.rx.recv().await
    }
}

/// The [`DecisionRecord`] the audited wrapper stores, whose action arguments never appear.
fn record_of(
    authoriser: &Authoriser,
    agent: &AgentIdentity,
    action: &str,
    ctx: &Context,
    decision: &Decision,
    now: i64,
) -> DecisionRecord {
    DecisionRecord {
        agent: agent.id.clone(),
        action: action.to_owned(),
        tier: authoriser.catalogue().get(action).map(|spec| spec.tier),
        decision_code: decision.code().to_owned(),
        decision_reason: decision.sentence(),
        approval_reference: ctx
            .human_approval
            .as_ref()
            .map(|approval| approval.reference.clone()),
        at: now,
    }
}

/// The audit record a [`DecisionRecord`] becomes: the action's name and the decision's code
/// only, never its arguments, and always marked masked.
#[must_use]
pub fn audit_record_of(entry: &DecisionRecord) -> AuditRecord {
    let mut payload = Payload::new();
    payload.insert(
        "action".to_owned(),
        PayloadValue::Text(entry.action.clone()),
    );
    payload.insert(
        "decision".to_owned(),
        PayloadValue::Text(entry.decision_code.clone()),
    );
    AuditRecord {
        kind: DECISION_KIND.to_owned(),
        actor: format!("agent:{}", entry.agent),
        on_behalf_of: None,
        subject: None,
        at: entry.at.saturating_mul(1_000_000),
        payload,
        masked: true,
    }
}

/// Appends every pending decision record to `chain`, in order. Returns how many were appended.
///
/// # Errors
///
/// The chain's store or signer error.
pub async fn drain<S: AuditStore>(
    rx: &mut DecisionReceiver,
    chain: &AuditChain<S>,
    limit: usize,
) -> Result<u64, spot_audit_chain::AuditError> {
    let mut appended = 0u64;
    while (appended as usize) < limit {
        let Some(entry) = rx.recv().await else {
            break;
        };
        chain.append(audit_record_of(&entry)).await?;
        appended += 1;
    }
    Ok(appended)
}

/// The fail-closed authoriser: it reserves a channel permit first, then decides and records under
/// it. With no room it counts the drop and — under [`OverflowPolicy::FailClosed`] — refuses the
/// action without calling the authoriser, so an unauditable action never runs.
pub struct AuditedAuthoriser {
    authoriser: Authoriser,
    sink: Arc<AgentDecisionAudit>,
    policy: OverflowPolicy,
    telemetry: Arc<dyn Telemetry>,
}

impl AuditedAuthoriser {
    /// Wraps `authoriser`, recording through `sink` under `policy`.
    #[must_use]
    pub fn new(
        authoriser: Authoriser,
        sink: Arc<AgentDecisionAudit>,
        policy: OverflowPolicy,
        telemetry: Arc<dyn Telemetry>,
    ) -> AuditedAuthoriser {
        AuditedAuthoriser {
            authoriser,
            sink,
            policy,
            telemetry,
        }
    }

    /// Decides `action` for `agent` and records the outcome, refusing the action when the audit
    /// cannot be recorded under the fail-closed policy.
    ///
    /// # Errors
    ///
    /// [`AuditUnavailable`] under [`OverflowPolicy::FailClosed`] when the channel is full.
    pub fn authorise(
        &self,
        agent: &AgentIdentity,
        action: &str,
        ctx: &Context,
        now: i64,
    ) -> Result<Decision, AuditUnavailable> {
        if let Some(permit) = self.sink.try_reserve() {
            let decision = self.authoriser.decide(agent, action, ctx);
            permit.send(record_of(
                &self.authoriser,
                agent,
                action,
                ctx,
                &decision,
                now,
            ));
            return Ok(decision);
        }
        self.telemetry.counter(DROPPED_COUNTER, 1);
        match self.policy {
            OverflowPolicy::FailClosed => Err(AuditUnavailable::new(format!(
                "the audit channel is full; the action `{action}` was refused rather than run unaudited"
            ))),
            OverflowPolicy::FailOpen => Ok(self.authoriser.decide(agent, action, ctx)),
        }
    }
}
