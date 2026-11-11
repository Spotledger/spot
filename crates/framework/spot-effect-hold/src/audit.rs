//! Recording hold transitions: the [`AuditSink`] trait, a recording fake, and the adapter that
//! appends to the tamper-evident audit chain (WP-AI-11).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use spot_audit_chain::{
    AuditChain, AuditRecord, AuditStore, CheckpointPolicy, CheckpointSigner, PayloadValue,
};
use spot_core::SiteId;

use crate::error::{Error, Result};
use crate::hold::MigrationId;

/// The kind of a recorded hold transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditKind {
    /// A hold began.
    Begin,
    /// A hold was released and its effects dispatched.
    Release,
    /// A hold was discarded.
    Discard,
    /// A release resumed from a checkpoint.
    Resume,
    /// A stale effect waited for confirmation.
    Confirmation,
}

impl AuditKind {
    /// The short name appended to the audit chain's `hold.` prefix.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            AuditKind::Begin => "begin",
            AuditKind::Release => "release",
            AuditKind::Discard => "discard",
            AuditKind::Resume => "resume",
            AuditKind::Confirmation => "confirmation",
        }
    }
}

/// One recorded transition. Counts only; never a payload, message body or secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEvent {
    /// The kind.
    pub kind: AuditKind,
    /// The site.
    pub site: SiteId,
    /// The migration, when the transition belongs to one.
    pub migration_id: Option<MigrationId>,
    /// Unix microseconds.
    pub at: i64,
    /// Counts and other non-sensitive integers (effects held, dispatched, discarded, ...).
    pub counts: BTreeMap<String, i64>,
}

impl AuditEvent {
    /// A transition with no counts.
    #[must_use]
    pub fn new(kind: AuditKind, site: SiteId, migration_id: Option<MigrationId>, at: i64) -> Self {
        Self {
            kind,
            site,
            migration_id,
            at,
            counts: BTreeMap::new(),
        }
    }

    /// The same event with one count.
    #[must_use]
    pub fn with_count(mut self, name: impl Into<String>, value: i64) -> Self {
        self.counts.insert(name.into(), value);
        self
    }
}

/// Where hold transitions are recorded.
pub trait AuditSink: Send + Sync {
    /// Records one transition.
    fn record<'a>(&'a self, event: AuditEvent) -> BoxFuture<'a, Result<()>>;
}

/// An audit sink that records events in memory, for tests.
#[derive(Default)]
pub struct RecordingAudit {
    events: Mutex<Vec<AuditEvent>>,
}

impl RecordingAudit {
    /// An empty recording sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every recorded event, in order.
    #[must_use]
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The events of one kind, in order.
    #[must_use]
    pub fn events_of_kind(&self, kind: AuditKind) -> Vec<AuditEvent> {
        self.events()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }
}

impl std::fmt::Debug for RecordingAudit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingAudit")
            .field("events", &self.events().len())
            .finish_non_exhaustive()
    }
}

impl AuditSink for RecordingAudit {
    fn record<'a>(&'a self, event: AuditEvent) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(event);
            Ok(())
        })
    }
}

/// An [`AuditSink`] that appends each transition to the tamper-evident audit chain.
///
/// One chain is created per site over a fresh `S`; this adapter is the reference implementation
/// over the in-memory chain store. A managed key is supplied by the caller — this crate never
/// holds one.
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
    /// [`Error::AuditFailed`] when the chain store fails.
    pub async fn head(&self, site: &SiteId) -> Result<Option<spot_audit_chain::ChainEntry>> {
        let chains = self.chains.lock().await;
        match chains.get(site.as_str()) {
            Some(chain) => chain.head().await.map_err(|error| Error::AuditFailed {
                reason: error.to_string(),
            }),
            None => Ok(None),
        }
    }
}

impl<S: AuditStore + Default + 'static> AuditSink for ChainAudit<S> {
    fn record<'a>(&'a self, event: AuditEvent) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut chains = self.chains.lock().await;
            let site = event.site.as_str().to_owned();
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
                "kind".to_owned(),
                PayloadValue::Text(event.kind.name().to_owned()),
            );
            if let Some(migration_id) = &event.migration_id {
                payload.insert(
                    "migration".to_owned(),
                    PayloadValue::Text(migration_id.as_str().to_owned()),
                );
            }
            for (name, value) in &event.counts {
                payload.insert(name.clone(), PayloadValue::Int(*value));
            }
            // The audit chain requires a dotted lower-case kind; the prefix carries the crate's
            // namespace and the event kind is appended.
            let record = AuditRecord {
                kind: format!("hold.{}", event.kind.name()),
                actor: "system".to_owned(),
                on_behalf_of: None,
                subject: None,
                at: event.at,
                payload,
                masked: true,
            };
            chain
                .append(record)
                .await
                .map_err(|error| Error::AuditFailed {
                    reason: error.to_string(),
                })?;
            Ok(())
        })
    }
}
