//! The seams the apply path is built on: ownership, the audit marker, the delivery notice, and
//! the business-logic sentinel it must never touch.
//!
//! Every policy decision (who owns a record, whether a topic notifies, how much metadata skew is
//! accepted) is behind a trait or a plain value here, so a later ADR (063 to 067) can change the
//! policy without touching the apply path.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use spot_audit_chain::{
    AuditChain, AuditRecord, AuditStore, CheckpointPolicy, CheckpointSigner, PayloadValue,
};
use spot_storage_contract::{Name, RecordId};
use spot_sync_protocol::SourceId;

use crate::error::Reason;
use crate::record::{OriginFields, ReceivedRecord};

/// Whether this node owns a received record or holds it as a mirror (ADR-058 rule 5, ADR-063).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ownership {
    /// This node owns the record: it is stored as an ordinary, editable document.
    Owned,
    /// This node does not own it: it is stored with its origin and is read-only locally.
    Mirror,
}

/// Decides whether a sender may replicate a record and whether this node owns it (ADR-062 G1,
/// ADR-063).
///
/// The real implementation is [`PartitionOwners`](crate::PartitionOwners), which runs
/// `spot_partition::OwnerCheck::allow_replica` over the record's partition scope at the item's
/// instant. The trait lives here so a test or a specialised node can replace the policy.
///
/// `at` is the item's instant in Unix microseconds, supplied by the caller
/// ([`ApplyCtx::at`]); an implementation must never read a clock (ADR-077).
pub trait OwnerCheck: Send + Sync {
    /// Whether `sender` may replicate `record` to this node at `at`, and whether this node owns
    /// it.
    ///
    /// # Errors
    /// [`Reason::NotOwner`] when the sender is not the record's owner, and
    /// [`Reason::InvalidScope`] when the record's partition scope cannot be read. Both are
    /// ownership refusals: the record is never stored.
    fn allow_replica(
        &self,
        sender: &SourceId,
        record: &ReceivedRecord,
        at: i64,
    ) -> Result<Ownership, Reason>;
}

/// The simplest [`OwnerCheck`]: every record is a mirror from its sender.
///
/// Useful for tests and for a node that only ever receives.
#[derive(Clone, Copy, Debug, Default)]
pub struct MirrorAll;

impl OwnerCheck for MirrorAll {
    fn allow_replica(
        &self,
        _sender: &SourceId,
        _record: &ReceivedRecord,
        _at: i64,
    ) -> Result<Ownership, Reason> {
        Ok(Ownership::Mirror)
    }
}

/// An [`OwnerCheck`] where every record is owned locally (the rebuild-from-journal case).
#[derive(Clone, Copy, Debug, Default)]
pub struct OwnAll;

impl OwnerCheck for OwnAll {
    fn allow_replica(
        &self,
        _sender: &SourceId,
        _record: &ReceivedRecord,
        _at: i64,
    ) -> Result<Ownership, Reason> {
        Ok(Ownership::Owned)
    }
}

/// Where a record the ownership check refused is held so the stream can move on (ADR-062 G6).
///
/// This is the seam between the apply path and the quarantine queue (WP-EDG-06.T2): the apply
/// path never depends on the queue's store, and the queue maps [`Reason::code`] to the reason it
/// shows an operator. The queue holds the record image unchanged; the apply path has already
/// rolled its transaction back, so nothing was written.
pub trait QuarantinePort: Send + Sync {
    /// Holds `record` because the ownership check refused it with `reason`.
    ///
    /// # Errors
    /// A [`Reason`] when the queue could not store the item. The apply path reports that rather
    /// than dropping the record silently (ADR-062 G2).
    fn hold<'a>(
        &'a self,
        record: &'a ReceivedRecord,
        reason: &'a Reason,
        at: i64,
    ) -> BoxFuture<'a, Result<(), Reason>>;
}

/// A [`QuarantinePort`] that keeps refused records in memory, for tests and for a caller that
/// hands them to the real queue later.
#[derive(Default)]
pub struct RecordingQuarantine {
    held: Mutex<Vec<(ReceivedRecord, Reason)>>,
}

impl RecordingQuarantine {
    /// An empty port.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every held record and the reason it was refused, in order.
    #[must_use]
    pub fn held(&self) -> Vec<(ReceivedRecord, Reason)> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// How many records have been held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Whether nothing has been held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl std::fmt::Debug for RecordingQuarantine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingQuarantine")
            .field("held", &self.len())
            .finish_non_exhaustive()
    }
}

impl QuarantinePort for RecordingQuarantine {
    fn hold<'a>(
        &'a self,
        record: &'a ReceivedRecord,
        reason: &'a Reason,
        _at: i64,
    ) -> BoxFuture<'a, Result<(), Reason>> {
        Box::pin(async move {
            self.held
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((record.clone(), reason.clone()));
            Ok(())
        })
    }
}

/// The `replicated` marker written for one applied record, so "what happened to this record on
/// that branch" is answerable (ADR-061, ADR-062 rule 7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaMarker {
    /// The sender whose stream the record came from.
    pub source: SourceId,
    /// The record's stream sequence.
    pub seq: u64,
    /// The apply key's name part.
    pub name: String,
    /// The apply key's version part.
    pub version: u64,
    /// The root document's entity.
    pub entity: String,
    /// The root document's id.
    pub id: String,
    /// Whether the record was stored as a mirror.
    pub mirror: bool,
    /// Unix microseconds, supplied by the caller.
    pub at: i64,
}

/// Where a `replicated` audit marker is written.
pub trait ReplicaAudit: Send + Sync {
    /// Records one applied record.
    fn replicated<'a>(&'a self, marker: &'a ReplicaMarker) -> BoxFuture<'a, Result<(), Reason>>;
}

/// A [`ReplicaAudit`] that keeps markers in memory, for tests and for the quarantine queue.
#[derive(Default)]
pub struct RecordingAudit {
    markers: Mutex<Vec<ReplicaMarker>>,
}

impl RecordingAudit {
    /// An empty recording sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every recorded marker, in order.
    #[must_use]
    pub fn markers(&self) -> Vec<ReplicaMarker> {
        self.markers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// How many markers have been recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.markers
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
            .field("markers", &self.markers().len())
            .finish_non_exhaustive()
    }
}

impl ReplicaAudit for RecordingAudit {
    fn replicated<'a>(&'a self, marker: &'a ReplicaMarker) -> BoxFuture<'a, Result<(), Reason>> {
        Box::pin(async move {
            self.markers
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(marker.clone());
            Ok(())
        })
    }
}

/// A [`ReplicaAudit`] that appends each marker to the tamper-evident audit chain (WP-AI-11).
///
/// One chain is created per sender site over a fresh `S`; a managed signing key is supplied by
/// the caller. The entry's kind is `replicated` and its payload is the sender, sequence and
/// version only — never a field value.
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
    /// [`Reason::AuditFailed`] when the chain store fails.
    pub async fn head(&self, site: &str) -> Result<Option<spot_audit_chain::ChainEntry>, Reason> {
        let chains = self.chains.lock().await;
        match chains.get(site) {
            Some(chain) => chain.head().await.map_err(|error| Reason::AuditFailed {
                why: error.to_string(),
            }),
            None => Ok(None),
        }
    }
}

impl<S: AuditStore + Default + 'static> ReplicaAudit for ChainAudit<S> {
    fn replicated<'a>(&'a self, marker: &'a ReplicaMarker) -> BoxFuture<'a, Result<(), Reason>> {
        Box::pin(async move {
            let mut chains = self.chains.lock().await;
            let site = marker.source.site().to_owned();
            let chain = chains.entry(site.clone()).or_insert_with(|| {
                AuditChain::new(
                    S::default(),
                    site,
                    Arc::clone(&self.signer),
                    CheckpointPolicy::default(),
                )
            });
            let mut payload = BTreeMap::new();
            payload.insert("name".to_owned(), PayloadValue::Text(marker.name.clone()));
            payload.insert(
                "source".to_owned(),
                PayloadValue::Text(marker.source.as_str().to_owned()),
            );
            payload.insert(
                "seq".to_owned(),
                PayloadValue::Int(i64::try_from(marker.seq).unwrap_or(i64::MAX)),
            );
            payload.insert(
                "version".to_owned(),
                PayloadValue::Int(i64::try_from(marker.version).unwrap_or(i64::MAX)),
            );
            payload.insert("mirror".to_owned(), PayloadValue::Bool(marker.mirror));
            let record = AuditRecord {
                kind: "replicated".to_owned(),
                actor: "system".to_owned(),
                on_behalf_of: None,
                subject: Some(spot_audit_chain::Subject {
                    doctype: marker.entity.clone(),
                    id: marker.id.clone(),
                }),
                at: marker.at,
                payload,
                masked: true,
            };
            chain
                .append(record)
                .await
                .map_err(|error| Reason::AuditFailed {
                    why: error.to_string(),
                })?;
            Ok(())
        })
    }
}

/// A delivery notice appended for a topic declared `notify` (ADR-058 rule 9, ADR-080).
///
/// A notice is not a document event: it carries no business logic and triggers no outbox event.
/// Subscribers read it like any other feed and must be idempotent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryNotice {
    /// The exchange topic it arrived on.
    pub topic: Option<Name>,
    /// The root record's id.
    pub record_id: RecordId,
    /// The conversation it belongs to, for a message topic.
    pub conversation: Option<String>,
    /// The sender.
    pub source: SourceId,
    /// Unix microseconds, supplied by the caller.
    pub received_at: i64,
}

/// Where a delivery notice is appended, inside the same transaction as the record.
pub trait NoticeSink: Send + Sync {
    /// Appends one notice.
    fn append<'a>(&'a self, notice: &'a DeliveryNotice) -> BoxFuture<'a, Result<(), Reason>>;
}

/// A [`NoticeSink`] that keeps notices in memory, for tests and for the quarantine queue.
#[derive(Default)]
pub struct RecordingNoticeSink {
    notices: Mutex<Vec<DeliveryNotice>>,
}

impl RecordingNoticeSink {
    /// An empty recording sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every recorded notice, in order.
    #[must_use]
    pub fn notices(&self) -> Vec<DeliveryNotice> {
        self.notices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// How many notices have been recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.notices
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

impl std::fmt::Debug for RecordingNoticeSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingNoticeSink")
            .field("notices", &self.notices().len())
            .finish_non_exhaustive()
    }
}

impl NoticeSink for RecordingNoticeSink {
    fn append<'a>(&'a self, notice: &'a DeliveryNotice) -> BoxFuture<'a, Result<(), Reason>> {
        Box::pin(async move {
            self.notices
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(notice.clone());
            Ok(())
        })
    }
}

/// The business-logic seam the apply path must never touch.
///
/// Each method stands for one action a normal document write performs: a lifecycle hook, a
/// naming-series allocation, a server script, or the outbox emitter. A replicated record is
/// stored exactly as it arrived, so [`apply`](crate::apply) never calls any of them. The trait
/// exists so a test can pass a panicking implementation and prove the prohibition holds; a
/// production caller passes `None` in [`ApplyCtx`].
pub trait SideEffects: Send + Sync {
    /// Runs the lifecycle hooks a normal write would run.
    fn run_hooks(&self, entity: &str);
    /// Allocates the next number from a naming series.
    fn allocate_name(&self, entity: &str) -> String;
    /// Runs a server script a normal write would run.
    fn run_script(&self, entity: &str);
    /// Enqueues the lifecycle event a normal write would enqueue on the outbox.
    fn emit_event(&self, entity: &str);
}

/// Everything the apply path needs besides the transaction and the record.
///
/// Policy lives here, not in the apply path: ownership ([`OwnerCheck`]), the audit marker
/// ([`ReplicaAudit`]), the delivery notice ([`NoticeSink`]), the accepted metadata skew, the
/// origin field names, and the time (always an argument, never the wall clock).
///
/// Where an ownership refusal goes is a parameter of
/// [`apply_quarantining`](crate::apply_quarantining), not a field here, so a caller that holds no
/// queue ([`apply`](crate::apply)) needs no placeholder.
pub struct ApplyCtx<'a> {
    /// The ownership decision, run before any write.
    pub owner: &'a dyn OwnerCheck,
    /// Where the `replicated` marker is written.
    pub audit: &'a dyn ReplicaAudit,
    /// Where a delivery notice is appended, when the topic notifies.
    pub notice: Option<&'a dyn NoticeSink>,
    /// Whether the record's topic is declared `notify` (ADR-058 rule 9).
    pub notify: bool,
    /// The field names a mirror carries its origin in.
    pub origin: OriginFields,
    /// The metadata skew this node accepts from a sender (ADR-066).
    pub max_skew: u64,
    /// Unix microseconds, supplied by the caller.
    pub at: i64,
    /// A sentinel proving the apply path runs no business logic; `None` in production.
    pub side_effects: Option<&'a dyn SideEffects>,
}

impl<'a> ApplyCtx<'a> {
    /// A context with the ADR-058 origin fields, no notice, no skew and no sentinel.
    #[must_use]
    pub fn new(owner: &'a dyn OwnerCheck, audit: &'a dyn ReplicaAudit, at: i64) -> Self {
        Self {
            owner,
            audit,
            notice: None,
            notify: false,
            origin: OriginFields::default(),
            max_skew: 0,
            at,
            side_effects: None,
        }
    }

    /// The same context with a delivery-notice sink.
    #[must_use]
    pub fn with_notice(mut self, notice: &'a dyn NoticeSink) -> Self {
        self.notice = Some(notice);
        self
    }

    /// The same context declaring whether the record's topic notifies.
    #[must_use]
    pub fn with_notify(mut self, notify: bool) -> Self {
        self.notify = notify;
        self
    }

    /// The same context with explicit origin field names.
    #[must_use]
    pub fn with_origin(mut self, origin: OriginFields) -> Self {
        self.origin = origin;
        self
    }

    /// The same context accepting `skew` metadata versions ahead of this node.
    #[must_use]
    pub fn with_max_skew(mut self, skew: u64) -> Self {
        self.max_skew = skew;
        self
    }

    /// The same context with a business-logic sentinel.
    #[must_use]
    pub fn with_side_effects(mut self, side_effects: &'a dyn SideEffects) -> Self {
        self.side_effects = Some(side_effects);
        self
    }
}
