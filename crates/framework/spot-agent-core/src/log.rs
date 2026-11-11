//! The tamper-evident result log: every decision and every execution result is one entry
//! of a local hash-chained log built on [`spot_audit_chain`].
//!
//! Two implementations are provided. [`MemoryResultLog`] is a synchronous, in-process
//! chain built from the audit chain's pure API ([`ChainEntry::new`] and
//! [`genesis_hash`]) — the reference used by tests and by a host that only needs a local
//! record. [`ChainResultLog`] composes the full asynchronous [`AuditChain`] over an
//! injected [`AuditExecutor`], so a host with durable storage (the PostgreSQL store of
//! WP-AI-11.T2) records the same entries without this crate doing any I/O of its own.
//!
//! Each entry carries the command digest, the signers, the outcome and, on a refusal, its
//! stable code. Secrets never reach the log: the digest is a hash and the arguments are
//! never recorded.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;

use spot_audit_chain::{
    AuditChain, AuditRecord, AuditStore, ChainEntry, PayloadValue, genesis_hash,
};

use crate::error::Refusal;

/// What one decision or execution result records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionRecord {
    /// `system`, `user:<id>` or `agent:<id>`; the agent records as `agent:<id>`.
    pub actor: String,
    /// The command's random id.
    pub command_id: String,
    /// The digest of the command's signed bytes.
    pub command_digest: [u8; 32],
    /// The operation id.
    pub op: String,
    /// The distinct signer key ids, in the order they verified.
    pub signers: Vec<String>,
    /// A short outcome token such as `execute`, `executed` or `executor_refused` (no dot).
    pub outcome: String,
    /// The refusal's stable code, when this record is a refusal.
    pub code: Option<String>,
    /// The instant of the record, Unix seconds.
    pub at: i64,
}

impl DecisionRecord {
    /// Renders the record as an audit-chain record. The kind is `agent_core.<outcome>`,
    /// the actor is the record's actor, and the payload carries the digest, the signers
    /// and the outcome.
    fn to_audit_record(&self) -> Result<AuditRecord, Refusal> {
        let at = self
            .at
            .checked_mul(1_000_000)
            .ok_or(Refusal::LogUnavailable)?;
        let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
        payload.insert(
            "command_id".to_owned(),
            PayloadValue::Text(self.command_id.clone()),
        );
        payload.insert(
            "command_digest".to_owned(),
            PayloadValue::Text(hex(&self.command_digest)),
        );
        payload.insert("op".to_owned(), PayloadValue::Text(self.op.clone()));
        payload.insert(
            "outcome".to_owned(),
            PayloadValue::Text(self.outcome.clone()),
        );
        payload.insert(
            "signers".to_owned(),
            PayloadValue::Text(self.signers.join(",")),
        );
        if let Some(code) = &self.code {
            payload.insert("code".to_owned(), PayloadValue::Text(code.clone()));
        }
        Ok(AuditRecord {
            kind: format!("agent_core.{}", self.outcome),
            actor: self.actor.clone(),
            on_behalf_of: None,
            subject: None,
            at,
            payload,
            masked: true,
        })
    }
}

/// The result-log contract. A failure to record refuses the command (fail closed): a
/// decision without evidence is not a decision the platform will stand behind.
pub trait ResultLog: Send + Sync {
    /// The log's site name (one chain per site).
    fn site(&self) -> &str;

    /// Appends one record and returns the linked entry, so the caller can ship it with the
    /// result.
    ///
    /// # Errors
    /// [`Refusal::LogUnavailable`] when the record is malformed or the store fails.
    fn append(&self, record: &DecisionRecord) -> Result<ChainEntry, Refusal>;
}

/// The synchronous boundary in front of the asynchronous audit chain. A host provides its
/// executor; this crate reads no clock and starts no threads. Implementations must drive
/// the whole future to completion and never detach it.
pub trait AuditExecutor: Send + Sync {
    /// Runs `future` to completion and returns its output.
    ///
    /// # Errors
    /// [`Refusal::LogUnavailable`] when the executor cannot complete the future.
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, Refusal>;
}

/// An in-memory, synchronous hash chain: the reference [`ResultLog`].
///
/// The chain is built from the audit chain's pure API, so it is the same
/// `SHA-256(seq, prev, record)` structure a durable store keeps; [`MemoryResultLog::verify`]
/// re-checks every link and hash.
pub struct MemoryResultLog {
    site: String,
    entries: Mutex<Vec<ChainEntry>>,
}

impl MemoryResultLog {
    /// An empty chain for `site`.
    #[must_use]
    pub fn new(site: impl Into<String>) -> Self {
        Self {
            site: site.into(),
            entries: Mutex::new(Vec::new()),
        }
    }

    /// The entries appended so far, in order.
    ///
    /// # Errors
    /// [`Refusal::LogUnavailable`] when the lock is poisoned.
    pub fn entries(&self) -> Result<Vec<ChainEntry>, Refusal> {
        let entries = self.entries.lock().map_err(|_| Refusal::LogUnavailable)?;
        Ok(entries.clone())
    }

    /// Re-checks every link and hash: the customer-side view of the local log. `true` only
    /// when the chain is intact from the genesis hash to the head.
    #[must_use]
    pub fn verify(&self) -> bool {
        let Ok(entries) = self.entries.lock() else {
            return false;
        };
        let mut prev = genesis_hash(&self.site);
        for (index, entry) in entries.iter().enumerate() {
            let seq = u64::try_from(index).map_or(u64::MAX, |n| n.saturating_add(1));
            if entry.seq != seq || entry.prev_hash != prev || entry.recompute_hash() != entry.hash {
                return false;
            }
            prev = entry.hash;
        }
        true
    }
}

impl ResultLog for MemoryResultLog {
    fn site(&self) -> &str {
        &self.site
    }

    fn append(&self, record: &DecisionRecord) -> Result<ChainEntry, Refusal> {
        let audit = record.to_audit_record()?;
        audit.validate().map_err(|_| Refusal::LogUnavailable)?;
        let mut entries = self.entries.lock().map_err(|_| Refusal::LogUnavailable)?;
        let prev = entries
            .last()
            .map_or_else(|| genesis_hash(&self.site), |entry| entry.hash);
        let seq = u64::try_from(entries.len())
            .map_err(|_| Refusal::LogUnavailable)?
            .saturating_add(1);
        let entry = ChainEntry::new(seq, prev, audit);
        entries.push(entry.clone());
        Ok(entry)
    }
}

impl std::fmt::Debug for MemoryResultLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryResultLog")
            .field("site", &self.site)
            .field(
                "entries",
                &self.entries.lock().map(|e| e.len()).unwrap_or(0),
            )
            .finish()
    }
}

/// A [`ResultLog`] over the asynchronous [`AuditChain`], driven by an injected
/// [`AuditExecutor`]. No I/O happens in this crate; the host's store does it.
pub struct ChainResultLog<S: AuditStore, E: AuditExecutor> {
    chain: AuditChain<S>,
    executor: E,
}

impl<S: AuditStore, E: AuditExecutor> ChainResultLog<S, E> {
    /// Wraps `chain`, driving its futures with `executor`.
    #[must_use]
    pub fn new(chain: AuditChain<S>, executor: E) -> Self {
        Self { chain, executor }
    }

    /// The wrapped chain, for the customer-side verifier.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<S> {
        &self.chain
    }
}

impl<S: AuditStore, E: AuditExecutor> ResultLog for ChainResultLog<S, E> {
    fn site(&self) -> &str {
        self.chain.site()
    }

    fn append(&self, record: &DecisionRecord) -> Result<ChainEntry, Refusal> {
        let audit = record.to_audit_record()?;
        audit.validate().map_err(|_| Refusal::LogUnavailable)?;
        self.executor
            .run(self.chain.append(audit))?
            .map_err(|_| Refusal::LogUnavailable)
    }
}

impl<S: AuditStore, E: AuditExecutor> std::fmt::Debug for ChainResultLog<S, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainResultLog")
            .field("site", &self.chain.site())
            .finish()
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn record(at: i64) -> DecisionRecord {
        DecisionRecord {
            actor: "agent:agent-1".to_owned(),
            command_id: "c1".to_owned(),
            command_digest: [9u8; 32],
            op: "server.service.restart".to_owned(),
            signers: vec!["aa".repeat(16)],
            outcome: "execute".to_owned(),
            code: None,
            at,
        }
    }

    #[test]
    fn appends_link_and_verify() {
        let log = MemoryResultLog::new("agent-1");
        assert!(log.verify());
        let first = log.append(&record(10)).unwrap();
        let second = log.append(&record(11)).unwrap();
        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(second.prev_hash, first.hash);
        assert_eq!(log.entries().unwrap().len(), 2);
        assert!(log.verify());
        assert_eq!(log.site(), "agent-1");
    }

    #[test]
    fn a_refusal_code_is_recorded() {
        let log = MemoryResultLog::new("agent-1");
        let mut refusal = record(10);
        refusal.outcome = "refuse".to_owned();
        refusal.code = Some("agent_core.expired".to_owned());
        let entry = log.append(&refusal).unwrap();
        assert_eq!(
            entry.record.payload.get("code"),
            Some(&PayloadValue::Text("agent_core.expired".to_owned()))
        );
        assert_eq!(entry.record.kind, "agent_core.refuse");
    }
}
