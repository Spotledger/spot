//! The lane-sharded appender: chains each lane's audit events, in commit order, off the hot path.
//!
//! The business transaction only writes an audit event to the outbox; the appender reads that
//! lane's events from the outbox intake, appends each to the lane's [`AuditChain`] in the order
//! they were published, and advances its bookmark only after the append is durable. A document is
//! always in one lane, so its events keep their order without any cross-lane coordination: two
//! appenders never share a lock, and no lane's head serialises another lane.
//!
//! Two topics are chained: `sys_doc_audit` (a document change) and `sys_secret_audit` (a secret
//! operation, WP-SEC-03.T7). Both go to the lane of the event's subject, in outbox order, with
//! the same idempotency and the same bookmark rule; an entry's kind (`doc.*` or `secret.*`) tells
//! them apart. An outbox row is never skipped past before it is chained: a topic this appender
//! does not chain is the only thing the bookmark passes over unchained.
//!
//! Idempotency is by the outbox event id: a re-delivered event (a crash before the bookmark
//! advanced) is appended once. A production [`AppenderStore`] must commit the chain entry and the
//! event-id index together; the reference store does the two writes back to back, which is safe
//! for the redelivery case the tests cover (see the report's open questions).

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use spot_audit_chain::{
    AppendOutcome, AuditChain, AuditRecord, AuditStore, ChainEntry, Checkpoint, CheckpointPolicy,
    CheckpointSigner, MAX_PAYLOAD_BYTES, PayloadValue, StoreFuture, Subject, genesis_hash,
    payload_encoded_len,
};
use spot_events_contract::payload::PayloadDocument;
use spot_storage_contract::feed::{ChangeFeed, FeedPosition, OutboxRecord};

use crate::error::DocAuditError;
use crate::event::{DocAuditEvent, encode_event, sha256_hex};
use crate::lane::{LaneConfig, LaneHead, SiteCheckpoint, lane_of, lane_site};
use crate::secret::{SECRET_AUDIT_TOPIC, SecretAuditEvent, decode_secret_outbox, secret_record_of};
use crate::step::AUDIT_TOPIC;

/// An [`AuditStore`] over a shared handle, so several lanes' chains use one store.
pub struct SharedStore<S>(Arc<S>);

impl<S> SharedStore<S> {
    /// A shared store over `store`.
    #[must_use]
    pub fn new(store: Arc<S>) -> Self {
        Self(store)
    }
}

impl<S: AuditStore> AuditStore for SharedStore<S> {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        self.0.head(site)
    }

    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.0.append_if_head(site, expected_seq, entry)
    }

    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        self.0.range(site, from, to)
    }

    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.0.put_checkpoint(checkpoint)
    }

    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.0.latest_checkpoint(site)
    }
}

/// Reads the outbox as an object-safe feed (the storage [`ChangeFeed`] is not dyn-compatible).
pub trait OutboxFeed: Send + Sync {
    /// Publishes pending committed events and returns up to `limit` after `after`.
    fn read<'a>(
        &'a self,
        after: FeedPosition,
        limit: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<OutboxRecord>, DocAuditError>> + Send + 'a>>;
}

impl<T: ChangeFeed + ?Sized> OutboxFeed for T {
    fn read<'a>(
        &'a self,
        after: FeedPosition,
        limit: u32,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<OutboxRecord>, DocAuditError>> + Send + 'a>> {
        Box::pin(async move {
            self.read_outbox(after, limit)
                .await
                .map_err(|e| DocAuditError::Chain(e.explanation))
        })
    }
}

/// The appender's durable state: per-lane bookmarks, the event-id index and the per-document
/// index. A production implementation is a table beside the chain; the in-memory one is the
/// reference.
pub trait AppenderStore: Send + Sync {
    /// The lane's bookmark: the last outbox position it scanned.
    fn bookmark(&self, lane: u32) -> FeedPosition;
    /// Sets the lane's bookmark, after an append is durable.
    fn set_bookmark(&self, lane: u32, position: FeedPosition);
    /// The lane and sequence an event id was already appended at, if any.
    fn appended(&self, event_id: &str) -> Option<(u32, u64)>;
    /// Records that an event id was appended at `(lane, seq)`.
    fn record_appended(&self, event_id: &str, lane: u32, seq: u64);
    /// The `(lane, seq)` entries of one document, in append order.
    fn document_entries(&self, doctype: &str, id: &str) -> Vec<(u32, u64)>;
    /// Records one `(lane, seq)` entry for a document.
    fn record_document_entry(&self, doctype: &str, id: &str, lane: u32, seq: u64);
    /// The newest signed site checkpoint.
    fn site_checkpoint(&self) -> Option<SiteCheckpoint>;
    /// Stores a signed site checkpoint.
    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint);
}

#[derive(Default)]
struct AppenderState {
    bookmarks: HashMap<u32, FeedPosition>,
    appended: HashMap<String, (u32, u64)>,
    documents: HashMap<(String, String), Vec<(u32, u64)>>,
    checkpoint: Option<SiteCheckpoint>,
}

/// The in-memory reference [`AppenderStore`].
#[derive(Default)]
pub struct InMemoryAppenderStore {
    state: Mutex<AppenderState>,
}

impl InMemoryAppenderStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AppenderState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl AppenderStore for InMemoryAppenderStore {
    fn bookmark(&self, lane: u32) -> FeedPosition {
        self.lock()
            .bookmarks
            .get(&lane)
            .copied()
            .unwrap_or(FeedPosition::START)
    }

    fn set_bookmark(&self, lane: u32, position: FeedPosition) {
        self.lock().bookmarks.insert(lane, position);
    }

    fn appended(&self, event_id: &str) -> Option<(u32, u64)> {
        self.lock().appended.get(event_id).copied()
    }

    fn record_appended(&self, event_id: &str, lane: u32, seq: u64) {
        self.lock()
            .appended
            .insert(event_id.to_owned(), (lane, seq));
    }

    fn document_entries(&self, doctype: &str, id: &str) -> Vec<(u32, u64)> {
        self.lock()
            .documents
            .get(&(doctype.to_owned(), id.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    fn record_document_entry(&self, doctype: &str, id: &str, lane: u32, seq: u64) {
        self.lock()
            .documents
            .entry((doctype.to_owned(), id.to_owned()))
            .or_default()
            .push((lane, seq));
    }

    fn site_checkpoint(&self) -> Option<SiteCheckpoint> {
        self.lock().checkpoint.clone()
    }

    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint) {
        self.lock().checkpoint = Some(checkpoint);
    }
}

/// What one drain did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrainReport {
    /// How many outbox records were read.
    pub read: usize,
    /// How many were appended (a re-delivered event is not).
    pub appended: usize,
    /// The lane's bookmark after the drain.
    pub position: FeedPosition,
}

/// What one append did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppendResult {
    /// The event was chained at `seq`.
    Appended {
        /// The lane sequence number.
        seq: u64,
    },
    /// The event id was already chained; nothing was appended.
    AlreadyChained {
        /// The lane it was chained in.
        lane: u32,
        /// The sequence number it was chained at.
        seq: u64,
    },
}

/// The appender for one lane of one site.
pub struct LaneAppender<S: AuditStore> {
    chain: AuditChain<SharedStore<S>>,
    store: Arc<S>,
    site: String,
    lane: u32,
    config: LaneConfig,
    signer: Arc<dyn CheckpointSigner>,
    state: Arc<dyn AppenderStore>,
}

impl<S: AuditStore> LaneAppender<S> {
    /// An appender for lane `lane` of `site`, chaining into `store` and recording state in
    /// `state`.
    #[must_use]
    pub fn new(
        store: Arc<S>,
        site: &str,
        lane: u32,
        config: LaneConfig,
        signer: Arc<dyn CheckpointSigner>,
        state: Arc<dyn AppenderStore>,
    ) -> Self {
        let chain = AuditChain::new(
            SharedStore::new(Arc::clone(&store)),
            lane_site(site, lane),
            Arc::clone(&signer),
            CheckpointPolicy::default(),
        );
        Self {
            chain,
            store,
            site: site.to_owned(),
            lane,
            config,
            signer,
            state,
        }
    }

    /// The lane.
    #[must_use]
    pub fn lane(&self) -> u32 {
        self.lane
    }

    /// The lane's chain.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<SharedStore<S>> {
        &self.chain
    }

    /// Drains up to `limit` outbox records from the lane's bookmark, appending this lane's audit
    /// events in order and advancing the bookmark only after each append is durable.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] on a feed, decode or append failure.
    pub async fn drain(
        &self,
        feed: &dyn OutboxFeed,
        limit: u32,
    ) -> Result<DrainReport, DocAuditError> {
        let start = self.state.bookmark(self.lane);
        let records = feed.read(start, limit).await?;
        let mut appended = 0;
        let mut position = start;
        for record in &records {
            position = record.position;
            let topic = record.event.topic.as_str();
            if topic == SECRET_AUDIT_TOPIC {
                let event = decode_secret_outbox(record)?;
                if lane_of(&event.subject_doctype, &event.subject_id, self.config.n) != self.lane {
                    self.state.set_bookmark(self.lane, position);
                    continue;
                }
                let event_id = record.event.id.to_string();
                match self.append_secret_event(&event_id, &event).await? {
                    AppendResult::Appended { .. } => appended += 1,
                    AppendResult::AlreadyChained { .. } => {}
                }
                self.state.set_bookmark(self.lane, position);
                continue;
            }
            if topic != AUDIT_TOPIC {
                self.state.set_bookmark(self.lane, position);
                continue;
            }
            let event = decode_outbox(record)?;
            if lane_of(&event.doctype, &event.id, self.config.n) != self.lane {
                self.state.set_bookmark(self.lane, position);
                continue;
            }
            let event_id = record.event.id.to_string();
            match self.append_event(&event_id, &event).await? {
                AppendResult::Appended { .. } => appended += 1,
                AppendResult::AlreadyChained { .. } => {}
            }
            self.state.set_bookmark(self.lane, position);
        }
        Ok(DrainReport {
            read: records.len(),
            appended,
            position,
        })
    }

    /// Appends one event to the lane chain, or reports it already chained.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] when the append fails.
    pub async fn append_event(
        &self,
        event_id: &str,
        event: &DocAuditEvent,
    ) -> Result<AppendResult, DocAuditError> {
        if let Some((lane, seq)) = self.state.appended(event_id) {
            return Ok(AppendResult::AlreadyChained { lane, seq });
        }
        let record = record_of(event)?;
        let entry = self.chain.append(record).await?;
        self.state.record_appended(event_id, self.lane, entry.seq);
        self.state
            .record_document_entry(&event.doctype, &event.id, self.lane, entry.seq);
        Ok(AppendResult::Appended { seq: entry.seq })
    }

    /// Appends one secret audit event to the lane chain, or reports it already chained.
    ///
    /// The entry is not recorded in the per-document index: that index feeds the document
    /// history, which shows document changes only.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] when the append fails.
    pub async fn append_secret_event(
        &self,
        event_id: &str,
        event: &SecretAuditEvent,
    ) -> Result<AppendResult, DocAuditError> {
        if let Some((lane, seq)) = self.state.appended(event_id) {
            return Ok(AppendResult::AlreadyChained { lane, seq });
        }
        let record = secret_record_of(event)?;
        let entry = self.chain.append(record).await?;
        self.state.record_appended(event_id, self.lane, entry.seq);
        Ok(AppendResult::Appended { seq: entry.seq })
    }

    /// Signs and stores a site checkpoint of every lane's current head.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] on a store or signer failure.
    pub async fn sign_checkpoint(&self, at: i64) -> Result<SiteCheckpoint, DocAuditError> {
        let heads = self.lane_heads().await?;
        let checkpoint = SiteCheckpoint::sign(&*self.signer, &self.site, &self.config, &heads, at)?;
        self.state.set_site_checkpoint(checkpoint.clone());
        Ok(checkpoint)
    }

    /// Every lane's current head, with the genesis hash for an empty lane.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] on a store failure.
    pub async fn lane_heads(&self) -> Result<Vec<LaneHead>, DocAuditError> {
        let mut heads = Vec::new();
        for lane in self.config.lanes() {
            let site = lane_site(&self.site, lane);
            let head = self.store.head(&site).await?;
            let (seq, head_hash) = match head {
                Some(h) => (h.seq, h.hash),
                None => (0, genesis_hash(&site)),
            };
            heads.push(LaneHead {
                lane,
                seq,
                head_hash,
            });
        }
        Ok(heads)
    }
}

/// Decodes the audit event an outbox record carries.
///
/// # Errors
/// [`DocAuditError::Encoding`] when the payload is not an audit event.
pub fn decode_outbox(record: &OutboxRecord) -> Result<DocAuditEvent, DocAuditError> {
    let doc = PayloadDocument::from_outbox_event(&record.event)
        .map_err(|e| DocAuditError::Encoding(e.to_string()))?;
    serde_json::from_value(doc.data).map_err(|e| DocAuditError::Encoding(e.to_string()))
}

/// The chain record of an audit event: kind `doc.<operation>`, the document as its subject, the
/// event as its payload, `masked = true`.
///
/// # Errors
/// [`DocAuditError::Chain`] when the record does not validate or the payload cannot be encoded.
pub fn record_of(event: &DocAuditEvent) -> Result<AuditRecord, DocAuditError> {
    let json = encode_event(event)?;
    let digest = sha256_hex(json.as_bytes());
    let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
    payload.insert("event".to_owned(), PayloadValue::Text(json));
    payload.insert("digest".to_owned(), PayloadValue::Text(digest));
    payload.insert("truncated".to_owned(), PayloadValue::Bool(event.truncated));
    if let Some(d) = &event.diff_digest {
        payload.insert("diff_digest".to_owned(), PayloadValue::Text(d.clone()));
    }
    if payload_encoded_len(&payload) > MAX_PAYLOAD_BYTES {
        let mut compact = event.clone();
        compact.diff = Vec::new();
        compact.truncated = true;
        let compact_json = encode_event(&compact)?;
        payload.insert("event".to_owned(), PayloadValue::Text(compact_json));
        payload.insert("truncated".to_owned(), PayloadValue::Bool(true));
    }
    let record = AuditRecord {
        kind: if event.outcome.as_deref() == Some(crate::event::OUTCOME_DENIED) {
            concat!("doc", ".denied").to_owned()
        } else {
            format!("doc.{}", event.operation.kind_segment())
        },
        actor: event.actor.actor.clone(),
        on_behalf_of: event.actor.on_behalf_of.clone(),
        subject: Some(Subject {
            doctype: event.doctype.clone(),
            id: event.id.clone(),
        }),
        at: event.at,
        payload,
        masked: true,
    };
    record.validate()?;
    Ok(record)
}

/// The audit event stored in a chain entry, when the payload carries one.
///
/// # Errors
/// [`DocAuditError::Encoding`] when the entry has no decodable event.
pub fn event_of(entry: &ChainEntry) -> Result<DocAuditEvent, DocAuditError> {
    match entry.record.payload.get("event") {
        Some(PayloadValue::Text(json)) => crate::event::decode_event(json),
        _ => Err(DocAuditError::Encoding(
            "the chain entry carries no audit event".to_owned(),
        )),
    }
}
