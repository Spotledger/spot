//! The record of every decision (the acceptance test is "out-of-tier action refused and
//! logged"): the sink trait, the record shape, the recording authoriser and the in-memory
//! sink.
//!
//! Every call through [`Authoriser::authorise_and_record`] writes exactly one
//! [`DecisionRecord`] — allowed or refused, refusals first of all — to the caller's
//! [`DecisionSink`]. The record names the action, never its arguments.

use serde::{Deserialize, Serialize};

use crate::authoriser::{Authoriser, Context, Decision};
use crate::identity::AgentIdentity;
use crate::tier::Tier;

/// What one recorded decision carries. The action's name only: its arguments never travel
/// into the record (they may carry business data; the decision needs only the name).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionRecord {
    /// The agent's id.
    pub agent: String,
    /// The action's name.
    pub action: String,
    /// The tier the action needed; `None` when the action was unknown (no spec, no tier).
    pub tier: Option<Tier>,
    /// The decision's stable code (`agent.allow` or the refusal's `agent.*` code).
    pub decision_code: String,
    /// The decision's plain-language reason text.
    pub decision_reason: String,
    /// The approval reference the request carried, if any (evidence of who approved).
    pub approval_reference: Option<String>,
    /// When the decision was made (unix seconds i64; the caller supplies the time).
    pub at: i64,
}

/// Where decisions are recorded. The authoriser calls it once per decided request — for
/// refusals first of all — and awaits nothing; an implementation that writes to storage,
/// a journal or a queue does that itself.
pub trait DecisionSink {
    /// Record one decision.
    fn record(&self, entry: DecisionRecord);
}

/// The in-memory sink: every record kept in order, for tests and examples.
///
/// Shared through `Arc<Mutex<_>>`-style interior mutability clones (a clone shares the
/// same list), so a test can hand [`Authoriser::authorise_and_record`] a `&MemorySink` and
/// then read what landed.
#[derive(Clone, Debug, Default)]
pub struct MemorySink {
    entries: std::sync::Arc<std::sync::Mutex<Vec<DecisionRecord>>>,
}

impl MemorySink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The recorded decisions, in recording order.
    ///
    /// A poisoned lock (another writer panicked) is recovered: the records survive.
    #[must_use]
    pub fn entries(&self) -> Vec<DecisionRecord> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl DecisionSink for MemorySink {
    fn record(&self, entry: DecisionRecord) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.push(entry);
    }
}

impl Authoriser {
    /// Decides a request and records the outcome — always, allowed or refused, refusals
    /// first of all — and returns the decision.
    ///
    /// Exactly one [`DecisionRecord`] per call, whatever the checks said. The record's
    /// `at` is the caller's `now: i64` (unix seconds); the crate keeps no clock.
    pub fn authorise_and_record(
        &self,
        agent: &AgentIdentity,
        action: &str,
        ctx: &Context,
        sink: &dyn DecisionSink,
        now: i64,
    ) -> Decision {
        let decision = self.decide(agent, action, ctx);
        let record = DecisionRecord {
            agent: agent.id.clone(),
            action: action.to_owned(),
            tier: self.catalogue().get(action).map(|spec| spec.tier),
            decision_code: decision.code().to_owned(),
            decision_reason: decision.sentence(),
            approval_reference: ctx
                .human_approval
                .as_ref()
                .map(|approval| approval.reference.clone()),
            at: now,
        };
        sink.record(record);
        decision
    }
}
