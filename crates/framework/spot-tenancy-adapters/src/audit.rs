//! The audit adapter: the control plane's attempt log, appended to `spot-audit-chain`.
//!
//! Every port call the plane makes is recorded, including the ones that fail, and a failed
//! append fails the step: provisioning never proceeds unaudited. Each record names the tenant as
//! its subject, the action and the outcome in its payload, and is linked into the site's chain.
//! A replayed step appends a second record — the trail is a log of attempts, so removing the
//! first would be the wrong kind of idempotency.

use std::sync::Arc;

use spot_audit_chain::{
    AuditChain, AuditError, AuditRecord, AuditStore, CheckpointPolicy, CheckpointSigner, Payload,
    PayloadValue, Subject,
};
use spot_tenancy::ports::{AuditEvent, AuditSink, Outcome, PortResult};

use crate::block::block_on;
use crate::error::AdapterError;

/// [`AuditSink`](spot_tenancy::ports::AuditSink) over a [`AuditChain`].
pub struct ChainAuditSink<S: AuditStore> {
    chain: AuditChain<S>,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl<S: AuditStore> ChainAuditSink<S> {
    /// A sink appending to `store`'s chain for `site`, signing checkpoints with `signer` under
    /// `policy`.
    ///
    /// `runtime` is what the synchronous port method blocks on.
    pub fn new(
        store: S,
        site: impl Into<String>,
        signer: Arc<dyn CheckpointSigner>,
        policy: CheckpointPolicy,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Self {
        Self {
            chain: AuditChain::new(store, site, signer, policy),
            runtime,
        }
    }

    /// The chain, for the verifier and for tests.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<S> {
        &self.chain
    }
}

impl<S: AuditStore> AuditSink for ChainAuditSink<S> {
    fn record(&self, event: &AuditEvent) -> PortResult {
        let record = record_of(event);
        block_on(self.runtime.as_ref(), self.chain.append(record))
            .map(|_| ())
            .map_err(|_: AuditError| AdapterError::Audit.failure(true))
    }
}

/// The chain record for one control-plane event.
fn record_of(event: &AuditEvent) -> AuditRecord {
    let mut payload = Payload::new();
    payload.insert(
        "action".to_owned(),
        PayloadValue::Text(event.action.to_owned()),
    );
    payload.insert(
        "outcome".to_owned(),
        PayloadValue::Text(match event.outcome {
            Outcome::Success => "success".to_owned(),
            Outcome::Failure(code) => code.to_owned(),
        }),
    );
    if let Some(step) = event.step {
        payload.insert("step".to_owned(), PayloadValue::Text(step.to_owned()));
    }
    AuditRecord {
        kind: format!("tenancy.{}", event.action),
        actor: actor_of(&event.actor),
        on_behalf_of: None,
        subject: Some(Subject {
            doctype: "tenant".to_owned(),
            id: event.tenant.to_string(),
        }),
        // The chain counts microseconds; the plane carries milliseconds.
        at: i64::try_from(event.at.saturating_mul(1_000)).unwrap_or(i64::MAX),
        payload,
        // No protected value is recorded: the payload is a code, a word and a step name.
        masked: true,
    }
}

/// The chain's actor form: `system`, `user:<id>` or `agent:<id>`. A bare control-plane name is a
/// user, and an absent one is the system.
fn actor_of(actor: &str) -> String {
    let actor = actor.trim();
    if actor.is_empty() {
        return "system".to_owned();
    }
    if actor == "system" || actor.starts_with("user:") || actor.starts_with("agent:") {
        return actor.to_owned();
    }
    format!("user:{actor}")
}
