//! Facts that were signed by the agent they describe, and are fresh.
//!
//! The pre-flight checks and the plan lean on what the agents report: versions, health,
//! capacity, ports, clock skew. An unsigned report, or one that can be replayed, would let
//! anyone who can reach the console steer the plan. So the engine never accepts a bare
//! [`AgentFacts`]: it takes [`ServerFacts`], which can only be built from a [`SignedFacts`]
//! report that [`SignedFacts::verify`] accepted:
//!
//! 1. the agent that signed it is the one enrolled for that host ([`EnrolledAgents`]);
//! 2. the signature verifies against that agent's enrolled identity key;
//! 3. `observed_at` is not in the future and not older than the policy's maximum age;
//! 4. the sequence is above the last accepted one for that agent ([`FactsLedger`]), which is
//!    checked and recorded in one step, and only after the signature verified, so a forged
//!    report cannot burn a sequence number.
//!
//! The enrolled identity key itself lives in the management layer (`spot-agent-enroll`); this
//! crate only asks the port whether a signature verifies. Time is an argument.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{FACTS_REPLAYED, FACTS_STALE, FACTS_UNTRUSTED, FACTS_WRONG_AGENT, WizardError};
use crate::facts::AgentFacts;
use crate::topology::Host;

/// The default longest a facts report may be old, in seconds.
pub const DEFAULT_MAX_FACTS_AGE_SECS: i64 = 300;

/// The most a deployment may be configured to tolerate, in seconds.
pub const MAX_FACTS_AGE_CEILING_SECS: i64 = 3_600;

/// The domain prefix of the signed bytes, so a facts signature is never a valid signature
/// over anything else the agent key signs.
const DOMAIN: &[u8] = b"spot.deploy-wizard.facts.v1\0";

/// How old facts may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FactsPolicy {
    max_age_secs: i64,
}

impl FactsPolicy {
    /// A policy accepting reports up to `max_age_secs` old.
    ///
    /// # Errors
    /// [`WizardError::Refused`] with [`FACTS_STALE`] when the age is not between 1 and
    /// [`MAX_FACTS_AGE_CEILING_SECS`].
    pub fn new(max_age_secs: i64) -> Result<Self, WizardError> {
        if max_age_secs <= 0 || max_age_secs > MAX_FACTS_AGE_CEILING_SECS {
            return Err(WizardError::refused(
                FACTS_STALE,
                "the maximum facts age must be between 1 second and 1 hour",
            ));
        }
        Ok(Self { max_age_secs })
    }

    /// The maximum age, in seconds.
    #[must_use]
    pub fn max_age_secs(&self) -> i64 {
        self.max_age_secs
    }
}

impl Default for FactsPolicy {
    fn default() -> Self {
        Self {
            max_age_secs: DEFAULT_MAX_FACTS_AGE_SECS,
        }
    }
}

/// The enrolled agents, implemented by the management layer over the enrolment records.
pub trait EnrolledAgents {
    /// The id of the agent enrolled for `host`, when one is.
    fn enrolled_agent(&self, host: &Host) -> Option<String>;

    /// Whether `signature` is a valid signature by `agent_id`'s enrolled identity key over
    /// `message`. Any doubt (unknown agent, revoked key, malformed signature) is `false`.
    fn verify(&self, agent_id: &str, message: &[u8], signature: &[u8]) -> bool;
}

/// The last accepted sequence per agent, kept by the management layer.
pub trait FactsLedger {
    /// Accepts `sequence` for `agent_id` when it is above every sequence accepted before,
    /// and records it; returns `false` (recording nothing) otherwise. The check and the
    /// record are one step, so two reports with the same sequence cannot both pass.
    fn accept(&mut self, agent_id: &str, sequence: u64) -> bool;
}

/// A ledger in memory, for tests and single-process consoles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryFactsLedger {
    last: BTreeMap<String, u64>,
}

impl MemoryFactsLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl FactsLedger for MemoryFactsLedger {
    fn accept(&mut self, agent_id: &str, sequence: u64) -> bool {
        match self.last.get(agent_id) {
            Some(last) if *last >= sequence => false,
            _ => {
                self.last.insert(agent_id.to_owned(), sequence);
                true
            }
        }
    }
}

/// A facts report as the agent sent it: signed, numbered and timed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedFacts {
    /// The agent that produced the report.
    pub agent_id: String,
    /// The agent's report counter; strictly increasing.
    pub sequence: u64,
    /// When the agent observed the facts, in seconds.
    pub observed_at: i64,
    /// The facts.
    pub facts: AgentFacts,
    /// The agent's signature over [`SignedFacts::signed_message`].
    pub signature: Vec<u8>,
}

impl fmt::Debug for SignedFacts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignedFacts")
            .field("agent_id", &self.agent_id)
            .field("sequence", &self.sequence)
            .field("observed_at", &self.observed_at)
            .field("host", &self.facts.host)
            .field("signature_len", &self.signature.len())
            .finish()
    }
}

fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    out.extend_from_slice(bytes);
}

impl SignedFacts {
    /// The bytes the agent signs: a domain prefix, then the agent id, the sequence, the
    /// observation time and the facts, each unambiguously framed.
    ///
    /// # Errors
    /// [`WizardError::Refused`] with [`FACTS_UNTRUSTED`] when the facts cannot be encoded.
    pub fn signed_message(
        agent_id: &str,
        sequence: u64,
        observed_at: i64,
        facts: &AgentFacts,
    ) -> Result<Vec<u8>, WizardError> {
        let body = serde_json::to_vec(facts).map_err(|_| {
            WizardError::refused(FACTS_UNTRUSTED, "the facts report cannot be encoded")
        })?;
        let mut out = Vec::with_capacity(DOMAIN.len() + body.len() + 64);
        out.extend_from_slice(DOMAIN);
        push_field(&mut out, agent_id.as_bytes());
        out.extend_from_slice(&sequence.to_be_bytes());
        out.extend_from_slice(&observed_at.to_be_bytes());
        push_field(&mut out, &body);
        Ok(out)
    }

    /// Verifies the report and turns it into [`ServerFacts`].
    ///
    /// # Errors
    /// In the order checked: [`FACTS_WRONG_AGENT`] (not the enrolled agent for the host),
    /// [`FACTS_UNTRUSTED`] (the signature does not verify), [`FACTS_STALE`] (older than the
    /// policy allows, or from the future), [`FACTS_REPLAYED`] (sequence not above the last
    /// accepted one).
    pub fn verify(
        &self,
        enrolled: &dyn EnrolledAgents,
        ledger: &mut dyn FactsLedger,
        policy: FactsPolicy,
        now: i64,
    ) -> Result<ServerFacts, WizardError> {
        if enrolled.enrolled_agent(&self.facts.host).as_deref() != Some(self.agent_id.as_str()) {
            return Err(WizardError::refused(
                FACTS_WRONG_AGENT,
                "the report is not from the agent enrolled for that host",
            ));
        }
        let message =
            Self::signed_message(&self.agent_id, self.sequence, self.observed_at, &self.facts)?;
        if !enrolled.verify(&self.agent_id, &message, &self.signature) {
            return Err(WizardError::refused(
                FACTS_UNTRUSTED,
                "the report's signature does not verify against the enrolled agent key",
            ));
        }
        if !is_fresh(self.observed_at, now, policy.max_age_secs) {
            return Err(WizardError::refused(
                FACTS_STALE,
                "the report is older than the maximum age, or dated in the future",
            ));
        }
        if !ledger.accept(&self.agent_id, self.sequence) {
            return Err(WizardError::refused(
                FACTS_REPLAYED,
                "the report's sequence is not above the last accepted one",
            ));
        }
        Ok(ServerFacts {
            facts: self.facts.clone(),
            agent_id: self.agent_id.clone(),
            sequence: self.sequence,
            observed_at: self.observed_at,
            max_age_secs: policy.max_age_secs,
        })
    }
}

/// Whether facts observed at `observed_at` are usable at `now` for at most `max_age_secs`.
pub(crate) fn is_fresh(observed_at: i64, now: i64, max_age_secs: i64) -> bool {
    observed_at <= now && now.saturating_sub(observed_at) <= max_age_secs
}

/// Facts the engine may plan from: verified, fresh when accepted, and stamped with where
/// they came from. There is no public way to build one except [`SignedFacts::verify`]
/// (and [`ServerFacts::for_testing`] under the `testing` feature).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerFacts {
    facts: AgentFacts,
    agent_id: String,
    sequence: u64,
    observed_at: i64,
    max_age_secs: i64,
}

impl ServerFacts {
    /// The facts.
    #[must_use]
    pub fn facts(&self) -> &AgentFacts {
        &self.facts
    }

    /// The agent that signed them.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The report's sequence.
    #[must_use]
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// When the agent observed them.
    #[must_use]
    pub fn observed_at(&self) -> i64 {
        self.observed_at
    }

    /// The oldest the facts may be when a run starts, in seconds.
    #[must_use]
    pub fn max_age_secs(&self) -> i64 {
        self.max_age_secs
    }

    /// Facts that skip verification, for tests only.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn for_testing(facts: AgentFacts, observed_at: i64) -> Self {
        Self {
            agent_id: format!("test-agent-{}", facts.host.as_str()),
            facts,
            sequence: 1,
            observed_at,
            max_age_secs: DEFAULT_MAX_FACTS_AGE_SECS,
        }
    }

    /// Like [`ServerFacts::for_testing`] with an explicit maximum age.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn for_testing_with_age(facts: AgentFacts, observed_at: i64, max_age_secs: i64) -> Self {
        let mut built = Self::for_testing(facts, observed_at);
        built.max_age_secs = max_age_secs;
        built
    }
}
