//! The stored documents: outcome, checkpoint and proof as JSON, and their decoding.
//!
//! Only integers and text are stored (no floats, ADR-12). A document that does not decode is a
//! store failure, never a default.

use serde::{Deserialize, Serialize};
use spot_data_policy::proof::to_hex;
use spot_data_policy::store::{Checkpoint, ItemOutcome, OutcomeStatus, SkipReason};
use spot_data_policy::{Action, DeletionProof};

use crate::error::OpError;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OutcomeDoc {
    rule_id: String,
    action: Option<String>,
    status: OutcomeStatus,
    reason: Option<SkipReason>,
    attempt: u32,
    at: u64,
    proof_digest: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointDoc {
    plan_digest: String,
    next_index: u64,
    attempt: u32,
    read: u64,
    applied: u64,
    skipped: u64,
    failed: u64,
    completed: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProofDoc {
    doctype: String,
    rule_id: String,
    action: String,
    instant: u64,
    digest: String,
}

pub(crate) fn encode_outcome(outcome: &ItemOutcome) -> Result<String, OpError> {
    let doc = OutcomeDoc {
        rule_id: outcome.rule_id.clone(),
        action: outcome.action.map(|a| a.as_str().to_owned()),
        status: outcome.status,
        reason: outcome.reason.clone(),
        attempt: outcome.attempt,
        at: outcome.at,
        proof_digest: outcome.proof_digest.clone(),
    };
    serde_json::to_string(&doc).map_err(|_| OpError::Decode)
}

pub(crate) fn decode_outcome(
    run_id: String,
    record_id: String,
    text: &str,
) -> Result<ItemOutcome, OpError> {
    let doc: OutcomeDoc = serde_json::from_str(text).map_err(|_| OpError::Decode)?;
    let action = match doc.action {
        Some(text) => Some(text.parse::<Action>().map_err(|_| OpError::Decode)?),
        None => None,
    };
    Ok(ItemOutcome {
        run_id,
        record_id,
        rule_id: doc.rule_id,
        action,
        status: doc.status,
        reason: doc.reason,
        attempt: doc.attempt,
        at: doc.at,
        proof_digest: doc.proof_digest,
    })
}

pub(crate) fn encode_checkpoint(checkpoint: &Checkpoint) -> Result<String, OpError> {
    let doc = CheckpointDoc {
        plan_digest: to_hex(&checkpoint.plan_digest),
        next_index: u64::try_from(checkpoint.next_index).map_err(|_| OpError::OutOfRange)?,
        attempt: checkpoint.attempt,
        read: checkpoint.read,
        applied: checkpoint.applied,
        skipped: checkpoint.skipped,
        failed: checkpoint.failed,
        completed: checkpoint.completed,
    };
    serde_json::to_string(&doc).map_err(|_| OpError::Decode)
}

pub(crate) fn decode_checkpoint(run_id: String, text: &str) -> Result<Checkpoint, OpError> {
    let doc: CheckpointDoc = serde_json::from_str(text).map_err(|_| OpError::Decode)?;
    Ok(Checkpoint {
        run_id,
        plan_digest: from_hex(&doc.plan_digest)?,
        next_index: usize::try_from(doc.next_index).map_err(|_| OpError::Decode)?,
        attempt: doc.attempt,
        read: doc.read,
        applied: doc.applied,
        skipped: doc.skipped,
        failed: doc.failed,
        completed: doc.completed,
    })
}

pub(crate) fn encode_proof(proof: &DeletionProof) -> Result<String, OpError> {
    let doc = ProofDoc {
        doctype: proof.doctype.clone(),
        rule_id: proof.rule_id.clone(),
        action: proof.action.as_str().to_owned(),
        instant: proof.instant,
        digest: to_hex(&proof.digest),
    };
    serde_json::to_string(&doc).map_err(|_| OpError::Decode)
}

pub(crate) fn decode_proof(record_id: String, text: &str) -> Result<DeletionProof, OpError> {
    let doc: ProofDoc = serde_json::from_str(text).map_err(|_| OpError::Decode)?;
    Ok(DeletionProof {
        record_id,
        doctype: doc.doctype,
        rule_id: doc.rule_id,
        action: doc.action.parse::<Action>().map_err(|_| OpError::Decode)?,
        instant: doc.instant,
        digest: from_hex(&doc.digest)?,
    })
}

fn from_hex(text: &str) -> Result<[u8; 32], OpError> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return Err(OpError::Decode);
    }
    let mut out = [0_u8; 32];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks(2)) {
        let high = nibble(pair[0])?;
        let low = nibble(pair[1])?;
        *slot = (high << 4) | low;
    }
    Ok(out)
}

fn nibble(byte: u8) -> Result<u8, OpError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(OpError::Decode),
    }
}
