//! Auditing a quarantine decision (ADR-061, ADR-062 rule 7).
//!
//! Every **decision** the queue makes about an item — a discard, a release, an automatic or manual
//! retry — is written as an audit entry, so "what happened to this record on that branch?" is
//! answerable. A discard in particular is a person's decision and must be provable.
//!
//! The seam is [`QuarantineAudit`]; [`RecordingAudit`] keeps entries in memory for tests and
//! [`ChainAudit`] appends them to the tamper-evident audit chain (WP-AI-11).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use spot_audit_chain::{
    AuditChain, AuditRecord, AuditStore, CheckpointPolicy, CheckpointSigner, PayloadValue, Subject,
};
use spot_sync_protocol::SourceId;

use crate::error::QuarantineError;
use crate::item::{ItemId, RecordRef};

/// The boxed future of the audit sink (the trait stays object-safe).
pub type AuditFuture<'a> = Pin<Box<dyn Future<Output = Result<(), QuarantineError>> + Send + 'a>>;

/// What the queue did to an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditAction {
    /// An operator discarded the item.
    Discarded,
    /// The item was re-applied successfully.
    Released,
    /// The item was retried (and may have failed again).
    Retried,
}

impl AuditAction {
    /// The stable text form, used in the audit entry's kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AuditAction::Discarded => "discarded",
            AuditAction::Released => "released",
            AuditAction::Retried => "retried",
        }
    }
}

/// One quarantine decision to audit. It names no secret: the item, the record's apply key, the
/// actor and the operator's own note.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// What was done.
    pub action: AuditAction,
    /// The item.
    pub item: ItemId,
    /// The record the item is about.
    pub record_ref: RecordRef,
    /// The sender whose stream it came from.
    pub source: SourceId,
    /// The actor: `system`, or a user name.
    pub actor: String,
    /// The operator's note (empty for an automatic retry).
    pub reason: String,
    /// Unix microseconds, supplied by the caller.
    pub at: i64,
}

/// Where a quarantine decision is audited.
pub trait QuarantineAudit: Send + Sync {
    /// Records one decision.
    ///
    /// # Errors
    /// [`QuarantineError::Audit`] when the sink fails.
    fn record<'a>(&'a self, entry: &'a AuditEntry) -> AuditFuture<'a>;
}

/// A [`QuarantineAudit`] that keeps entries in memory, for tests.
#[derive(Default)]
pub struct RecordingAudit {
    entries: Mutex<Vec<AuditEntry>>,
}

impl RecordingAudit {
    /// An empty recording sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every recorded entry, in order.
    #[must_use]
    pub fn entries(&self) -> Vec<AuditEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// How many entries have been recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Whether nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for RecordingAudit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingAudit")
            .field("entries", &self.len())
            .finish_non_exhaustive()
    }
}

impl QuarantineAudit for RecordingAudit {
    fn record<'a>(&'a self, entry: &'a AuditEntry) -> AuditFuture<'a> {
        Box::pin(async move {
            self.entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(entry.clone());
            Ok(())
        })
    }
}

/// The actor string the audit chain accepts (`system`, `user:<id>` or `agent:<id>`).
fn chain_actor(actor: &str) -> String {
    if actor.is_empty() || actor == "system" {
        return "system".to_owned();
    }
    if actor.starts_with("user:") || actor.starts_with("agent:") {
        return actor.to_owned();
    }
    format!("user:{actor}")
}

/// A [`QuarantineAudit`] that appends each decision to the tamper-evident audit chain (WP-AI-11).
///
/// One chain is created per sender site over a fresh `S`; the signing key is supplied by the
/// caller. The entry's kind is `quarantine_<action>` and its payload is the item id, the apply
/// key and the operator's note — never a field value.
pub struct ChainAudit<S: AuditStore + Default + 'static> {
    signer: Arc<dyn CheckpointSigner>,
    chains: tokio::sync::Mutex<HashMap<String, AuditChain<S>>>,
}

impl<S: AuditStore + Default + 'static> ChainAudit<S> {
    /// An adapter that signs checkpoints with `signer`.
    #[must_use]
    pub fn new(signer: Arc<dyn CheckpointSigner>) -> Self {
        Self {
            signer,
            chains: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The newest audit entry for `site`, or `None` when nothing has been recorded.
    ///
    /// # Errors
    /// [`QuarantineError::Audit`] when the chain store fails.
    pub async fn head(
        &self,
        site: &str,
    ) -> Result<Option<spot_audit_chain::ChainEntry>, QuarantineError> {
        let chains = self.chains.lock().await;
        match chains.get(site) {
            Some(chain) => chain.head().await.map_err(|error| QuarantineError::Audit {
                why: error.to_string(),
            }),
            None => Ok(None),
        }
    }
}

impl<S: AuditStore + Default + 'static> QuarantineAudit for ChainAudit<S> {
    fn record<'a>(&'a self, entry: &'a AuditEntry) -> AuditFuture<'a> {
        Box::pin(async move {
            let mut chains = self.chains.lock().await;
            let site = entry.source.site().to_owned();
            let chain = chains.entry(site.clone()).or_insert_with(|| {
                AuditChain::new(
                    S::default(),
                    site,
                    Arc::clone(&self.signer),
                    CheckpointPolicy::default(),
                )
            });
            let mut payload = BTreeMap::new();
            payload.insert(
                "item".to_owned(),
                PayloadValue::Int(i64::try_from(entry.item.0).unwrap_or(i64::MAX)),
            );
            payload.insert(
                "name".to_owned(),
                PayloadValue::Text(entry.record_ref.name.as_str().to_owned()),
            );
            payload.insert(
                "version".to_owned(),
                PayloadValue::Int(i64::try_from(entry.record_ref.version).unwrap_or(i64::MAX)),
            );
            if !entry.reason.is_empty() {
                payload.insert(
                    "reason".to_owned(),
                    PayloadValue::Text(entry.reason.clone()),
                );
            }
            let record = AuditRecord {
                kind: format!("quarantine_{}", entry.action.as_str()),
                actor: chain_actor(&entry.actor),
                on_behalf_of: None,
                subject: Some(Subject {
                    doctype: entry.record_ref.entity.clone(),
                    id: entry.record_ref.id.clone(),
                }),
                at: entry.at,
                payload,
                masked: true,
            };
            chain
                .append(record)
                .await
                .map_err(|error| QuarantineError::Audit {
                    why: error.to_string(),
                })?;
            Ok(())
        })
    }
}
