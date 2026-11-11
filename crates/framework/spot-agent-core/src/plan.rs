//! The execution plan: the only thing a privileged executor may act on.
//!
//! A plan is produced exclusively by [`crate::verify::Evaluator`] after every check
//! passed. It carries the validated arguments, the risk level taken from the catalogue,
//! the distinct signers, the command digest and a deterministic hash over all of it
//! ([`ExecutionPlan::plan_hash`]). The hash is what the broker signs; the executor
//! recomputes the whole plan from the original command and refuses anything that is not
//! byte-for-byte the same derivation.

use sha2::{Digest as _, Sha256};
use spot_mgmt_ops::{OpId, Risk, Validated};
use spot_signed_command::{Command, KeyId};

use crate::error::Refusal;

/// A SHA-256 digest.
pub type Digest = [u8; 32];

/// Hashes bytes with SHA-256.
fn hash(bytes: &[u8]) -> Digest {
    Sha256::digest(bytes).into()
}

/// Renders bytes as lower-case hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The digest of a command's signed bytes (domain prefix plus canonical command): what the
/// tamper-evident log records and what the plan commits to.
///
/// # Errors
/// [`Refusal::EnvelopeMalformed`] when the command has no canonical bytes (a float in a
/// parameter, which the ctor-verified command cannot have but the API cannot promise).
pub fn command_digest(command: &Command) -> Result<Digest, Refusal> {
    let message = command
        .signed_message()
        .map_err(|_| Refusal::EnvelopeMalformed)?;
    Ok(hash(&message))
}

/// A verified, ready-to-run instruction: everything a privileged executor needs, and
/// nothing a compromised broker could invent.
#[derive(Clone, PartialEq, Eq)]
pub struct ExecutionPlan {
    /// The agent the command is for.
    agent_id: String,
    /// The command's random id.
    command_id: String,
    /// The digest of the command's signed bytes.
    command_digest: Digest,
    /// The operation.
    op: OpId,
    /// The validated arguments (defaults filled).
    args: Validated,
    /// The risk level, taken from the catalogue.
    risk: Risk,
    /// The distinct valid administrator keys that signed.
    signers: Vec<KeyId>,
    /// The command's issue instant.
    issued_at: i64,
    /// The hash that binds every field above; what the broker signs.
    plan_hash: Digest,
}

/// The fields an [`ExecutionPlan`] is built from, gathered so the constructor stays within
/// the argument-count lint. Built only from a fully verified command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanParts {
    /// The agent the command is for.
    pub agent_id: String,
    /// The command's random id.
    pub command_id: String,
    /// The digest of the command's signed bytes.
    pub command_digest: Digest,
    /// The operation.
    pub op: OpId,
    /// The validated arguments.
    pub args: Validated,
    /// The risk level, from the catalogue.
    pub risk: Risk,
    /// The distinct valid signers.
    pub signers: Vec<KeyId>,
    /// The command's issue instant.
    pub issued_at: i64,
}

impl ExecutionPlan {
    /// Builds a plan and computes its hash from the fields.
    #[must_use]
    pub fn new(parts: PlanParts) -> Self {
        let mut plan = Self {
            agent_id: parts.agent_id,
            command_id: parts.command_id,
            command_digest: parts.command_digest,
            op: parts.op,
            args: parts.args,
            risk: parts.risk,
            signers: parts.signers,
            issued_at: parts.issued_at,
            plan_hash: [0u8; 32],
        };
        plan.plan_hash = plan.compute_hash();
        plan
    }

    /// The agent the command is for.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The command's random id.
    #[must_use]
    pub fn command_id(&self) -> &str {
        &self.command_id
    }

    /// The digest of the command's signed bytes.
    #[must_use]
    pub fn command_digest(&self) -> &Digest {
        &self.command_digest
    }

    /// The operation.
    #[must_use]
    pub fn op(&self) -> &OpId {
        &self.op
    }

    /// The validated arguments.
    #[must_use]
    pub fn args(&self) -> &Validated {
        &self.args
    }

    /// The risk level, from the catalogue.
    #[must_use]
    pub fn risk(&self) -> Risk {
        self.risk
    }

    /// The distinct valid signers.
    #[must_use]
    pub fn signers(&self) -> &[KeyId] {
        &self.signers
    }

    /// The command's issue instant.
    #[must_use]
    pub fn issued_at(&self) -> i64 {
        self.issued_at
    }

    /// The plan hash, as built.
    #[must_use]
    pub fn plan_hash(&self) -> &Digest {
        &self.plan_hash
    }

    /// Recomputes the plan hash from the fields. A plan whose stored hash differs from
    /// this has been tampered with after it was signed.
    #[must_use]
    pub fn compute_hash(&self) -> Digest {
        let signers: Vec<&str> = self.signers.iter().map(KeyId::as_str).collect();
        let encoded = serde_json::to_vec(&(
            &self.agent_id,
            &self.command_id,
            hex(&self.command_digest),
            self.op.as_str(),
            &self.args,
            risk_name(self.risk),
            signers,
            self.issued_at,
        ))
        .unwrap_or_else(|_| b"agent-core:plan-encode-failed".to_vec());
        hash(&encoded)
    }
}

impl std::fmt::Debug for ExecutionPlan {
    /// Redacts the arguments' secret values through [`Validated`]'s own `Debug`; prints
    /// the identity of the plan, the signers and the hash, never a parameter value.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionPlan")
            .field("agent_id", &self.agent_id)
            .field("command_id", &self.command_id)
            .field("op", &self.op.as_str())
            .field("risk", &risk_name(self.risk))
            .field("signers", &self.signers)
            .field("args", &self.args)
            .field("plan_hash", &hex(&self.plan_hash))
            .finish()
    }
}

/// The lower-case name of a risk level, for the plan hash.
fn risk_name(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "read",
        Risk::Low => "low",
        Risk::Medium => "medium",
        Risk::High => "high",
        Risk::Critical => "critical",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_mgmt_ops::{OpId, Validated};
    use spot_signed_command::{Command, KeyId};

    use super::*;

    fn plan(signers: Vec<KeyId>) -> ExecutionPlan {
        ExecutionPlan::new(PlanParts {
            agent_id: "agent-1".to_owned(),
            command_id: "c1".to_owned(),
            command_digest: [1u8; 32],
            op: OpId::new("server.service.restart").unwrap(),
            args: Validated {
                op: OpId::new("server.service.restart").unwrap(),
                params: Vec::new(),
            },
            risk: Risk::High,
            signers,
            issued_at: 1_000,
        })
    }

    #[test]
    fn the_hash_is_stable_and_field_sensitive() {
        let a = plan(vec![KeyId::from_public_key(&[3u8; 32])]);
        assert_eq!(a.plan_hash(), &a.compute_hash());
        let b = plan(vec![KeyId::from_public_key(&[4u8; 32])]);
        assert_ne!(a.plan_hash(), b.plan_hash());
        let mut tampered = a.clone();
        tampered.risk = Risk::Low;
        assert_ne!(tampered.compute_hash(), *a.plan_hash());
    }

    #[test]
    fn the_command_digest_is_the_hash_of_the_signed_bytes() {
        let command = Command::new(
            "server.service.restart",
            serde_json::json!({}),
            "agent-1",
            KeyId::from_public_key(&[1u8; 32]),
            10,
            20,
        );
        assert_eq!(
            command_digest(&command).unwrap(),
            hash(&command.signed_message().unwrap())
        );
    }
}
