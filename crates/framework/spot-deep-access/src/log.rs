//! Customer-owned logging through the existing audit chain, with an injected executor.
use crate::DeepError;
use serde::{Deserialize, Serialize};
use spot_audit_chain::{
    AuditChain, AuditRecord, AuditStore, Checkpoint, KeyRing, PayloadValue, Subject, Verifier,
};
use std::collections::BTreeMap;
use std::future::Future;

/// One customer-visible session transition; arguments and secrets are never stored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeepAccessEvent {
    /// Stable transition: requested, consented, issued, used, denied, escalated, revoked, expired.
    pub kind: String,
    /// Hexadecimal session identifier, or request identifier before issue.
    pub session: String,
    /// Support-side identity.
    pub who: String,
    /// Ticket and reason, scope summary, or action outcome.
    pub summary: String,
    /// Stable decision code, where applicable.
    pub code: String,
    /// Unix seconds, converted to audit microseconds with checked arithmetic.
    pub at: i64,
}
/// An injected synchronous boundary for the asynchronous audit-chain dependency.
/// The platform provides its executor; this crate reads no clock and starts no threads.
/// Implementations must drive the entire future to completion, never detach it.
pub trait AuditExecutor {
    /// Complete the operation before returning. A failed execution must fail closed.
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, DeepError>;
}
/// Trusted tenant-owned writer boundary. Implementations must durably append to the
/// tenant's spot-audit-chain before success. It is never passed to the operator.
pub trait SessionLog {
    /// Tenant owning the one chain this writer controls.
    fn tenant(&self) -> &str;
    /// Append exactly one transition and make its checkpoint customer-visible.
    fn append(&mut self, event: DeepAccessEvent) -> Result<(), DeepError>;
}
/// Adapter composing the existing chain with the platform's execution boundary.
/// No alternative hash chain is implemented here.
pub struct ChainLog<S: AuditStore, E: AuditExecutor> {
    chain: AuditChain<S>,
    executor: E,
}
impl<S: AuditStore, E: AuditExecutor> ChainLog<S, E> {
    /// Wrap the customer's chain; its signer and store stay platform-side.
    #[must_use]
    pub fn new(chain: AuditChain<S>, executor: E) -> Self {
        Self { chain, executor }
    }
    /// Render a verified plain-language report using an externally trusted checkpoint.
    pub fn customer_view(
        &self,
        session: &str,
        keys: &KeyRing,
        checkpoint: &Checkpoint,
    ) -> Result<SessionReport, DeepError> {
        let verifier = Verifier::new(keys);
        let full = self
            .executor
            .run(verifier.verify_range(self.chain.store(), self.chain.site(), 1, u64::MAX))?
            .map_err(|_| DeepError::AuditUnavailable)?
            .is_ok();
        let verified = full
            && self
                .executor
                .run(verifier.verify_incremental(
                    self.chain.store(),
                    self.chain.site(),
                    checkpoint,
                ))?
                .map_err(|_| DeepError::AuditUnavailable)?
                .is_ok();
        let entries = self
            .executor
            .run(self.chain.read_range(1, u64::MAX))?
            .map_err(|_| DeepError::AuditUnavailable)?;
        let mut lines = Vec::new();
        for entry in entries {
            if entry
                .record
                .subject
                .as_ref()
                .is_none_or(|s| s.id != session || s.doctype != "DeepAccessSession")
            {
                continue;
            }
            if let Some(PayloadValue::Text(body)) = entry.record.payload.get("event") {
                let event: DeepAccessEvent =
                    serde_json::from_str(body).map_err(|_| DeepError::AuditUnavailable)?;
                lines.push(format!(
                    "{}: {} — {} ({})",
                    event.at, event.who, event.summary, event.code
                ));
            }
        }
        lines.push(format!(
            "Chain verified: {}. Checkpoint {}.",
            if verified { "yes" } else { "no" },
            checkpoint.seq
        ));
        Ok(SessionReport {
            lines,
            verified,
            checkpoint: checkpoint.clone(),
        })
    }
    /// Obtain the current signed checkpoint for customer-held verification.
    pub fn checkpoint(&self, at: i64) -> Result<Checkpoint, DeepError> {
        self.executor
            .run(
                self.chain
                    .checkpoint_now(at.checked_mul(1_000_000).ok_or(DeepError::InvalidRequest)?),
            )?
            .map_err(|_| DeepError::AuditUnavailable)?
            .ok_or(DeepError::AuditUnavailable)
    }
}
impl<S: AuditStore, E: AuditExecutor> SessionLog for ChainLog<S, E> {
    fn tenant(&self) -> &str {
        self.chain.site()
    }
    fn append(&mut self, event: DeepAccessEvent) -> Result<(), DeepError> {
        let mut payload = BTreeMap::new();
        payload.insert(
            "event".to_owned(),
            PayloadValue::Text(
                serde_json::to_string(&event).map_err(|_| DeepError::AuditUnavailable)?,
            ),
        );
        let record = AuditRecord {
            kind: format!("deep.{}", event.kind),
            actor: "system".to_owned(),
            on_behalf_of: Some(format!("user:{}", event.who)),
            subject: Some(Subject {
                doctype: "DeepAccessSession".to_owned(),
                id: event.session,
            }),
            at: event
                .at
                .checked_mul(1_000_000)
                .ok_or(DeepError::AuditUnavailable)?,
            payload,
            masked: true,
        };
        self.executor
            .run(self.chain.append(record))?
            .map_err(|_| DeepError::AuditUnavailable)?;
        Ok(())
    }
}
/// Customer-readable history and verification result.
#[derive(Clone, Debug)]
pub struct SessionReport {
    /// Who, why, scope, window, actions, denials and the verification line.
    pub lines: Vec<String>,
    /// Whether the chain agrees with the customer's checkpoint.
    pub verified: bool,
    /// Checkpoint used, suitable for independent verification.
    pub checkpoint: Checkpoint,
}
pub(crate) fn session_text(id: [u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
pub(crate) fn event(
    kind: &str,
    session: &str,
    who: &str,
    summary: String,
    code: &str,
    at: i64,
) -> DeepAccessEvent {
    DeepAccessEvent {
        kind: kind.to_owned(),
        session: session.to_owned(),
        who: who.to_owned(),
        summary,
        code: code.to_owned(),
        at,
    }
}
