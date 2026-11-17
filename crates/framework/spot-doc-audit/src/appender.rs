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
//! # Idempotency: exactly once, by the outbox event id
//!
//! A re-delivered event (a crash before the bookmark advanced) is chained once. The chain append
//! and the index writes are separate commits, so the chain itself is the durable proof: every
//! chain record built here carries the outbox event id as `payload["event_id"]`. A normal append
//! is `chain.append`, then the document-index row (document events only), then the event-id row
//! **last**. When the event-id index does not know an event, the appender looks at the newest
//! [`RECOVERY_WINDOW`] entries of its own lane for that id before it appends: a crash after the
//! chain commit leaves the entry there, and the appender adopts it (writes the index rows) instead
//! of chaining the event a second time. A lane has one active appender (the fenced job lease), so
//! the only entry the index can miss is one of the last few of the lane. When the index already
//! knows the event, the document-index row is written again (an idempotent upsert) so an event
//! indexed by an older version of this code still gets its document entry. A chain read that
//! fails fails the call; it is never taken for "not found". An entry written before the id was
//! added has no `event_id` and is never matched.
//!
//! # The state port is async and fallible
//!
//! [`AppenderStore`] returns a future of a `Result` from every method. There is no blocking
//! bridge and no latch: a state call that fails ends [`LaneAppender::drain`] with
//! `docaudit.appender_state` at once, the bookmark is left where it was, and the job that runs the
//! drain retries it. The order of the state calls is the correctness argument and is kept: the
//! chain append first, then the index writes, then the bookmark.

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
use crate::secret::{
    SECRET_AUDIT_TOPIC, SecretAuditEvent, decode_secret_outbox, secret_record_of_with_id,
};
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

    fn append_many_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entries: Vec<ChainEntry>,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.0.append_many_if_head(site, expected_seq, entries)
    }

    fn batches_appends(&self) -> bool {
        self.0.batches_appends()
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

/// A boxed, `Send` future of one appender-state operation. A failure is a typed error, never an
/// empty or default answer: the appender must be able to tell "the database said no" from "the
/// database could not answer".
pub type StateFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, DocAuditError>> + Send + 'a>>;

/// The appender's durable state: per-lane bookmarks, the event-id index and the per-document
/// index, and the newest signed site checkpoint. A production implementation is a set of tables
/// beside the chain; the in-memory one is the reference.
///
/// The port is **async and fallible** (ADR-082 section 1): every method returns a future of a
/// `Result`. The appender awaits each call and ends the drain with the error at once, so the
/// bookmark never moves past an event whose chain entry or index row is not stored, and a failed
/// read is never taken for "not appended" (that would chain an event twice) or "appended" (that
/// would drop it). The trait is object-safe: `Arc<dyn AppenderStore>` is what the appender holds.
///
/// Every method is one statement or one transaction and leaves no half-applied write.
pub trait AppenderStore: Send + Sync {
    /// The lane's bookmark: the last outbox position it scanned ([`FeedPosition::START`] when
    /// none was stored).
    fn bookmark(&self, lane: u32) -> StateFuture<'_, FeedPosition>;
    /// Sets the lane's bookmark, after an append is durable.
    fn set_bookmark(&self, lane: u32, position: FeedPosition) -> StateFuture<'_, ()>;
    /// The lane and sequence an event id was already appended at, if any.
    fn appended<'a>(&'a self, event_id: &'a str) -> StateFuture<'a, Option<(u32, u64)>>;
    /// Records that an event id was appended at `(lane, seq)`.
    fn record_appended<'a>(&'a self, event_id: &'a str, lane: u32, seq: u64)
    -> StateFuture<'a, ()>;
    /// The `(lane, seq)` entries of one document, in append order.
    fn document_entries<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> StateFuture<'a, Vec<(u32, u64)>>;
    /// Records one `(lane, seq)` entry for a document; recording it again changes nothing.
    fn record_document_entry<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
        lane: u32,
        seq: u64,
    ) -> StateFuture<'a, ()>;
    /// The newest signed site checkpoint.
    fn site_checkpoint(&self) -> StateFuture<'_, Option<SiteCheckpoint>>;
    /// Stores a signed site checkpoint.
    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint) -> StateFuture<'_, ()>;

    /// Whether [`appended_many`](Self::appended_many) and [`record_many`](Self::record_many) are
    /// cheaper than a loop of the single calls (one statement for the lot). Only then does the
    /// appender drain in groups; a store that keeps the default `false` is driven one event at
    /// a time, call by call, exactly as before.
    fn batches(&self) -> bool {
        false
    }

    /// [`appended`](Self::appended) for each id, in the order given.
    fn appended_many<'a>(
        &'a self,
        event_ids: &'a [String],
    ) -> StateFuture<'a, Vec<Option<(u32, u64)>>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(event_ids.len());
            for id in event_ids {
                out.push(self.appended(id).await?);
            }
            Ok(out)
        })
    }

    /// Records the document entries of a group, then (last) its event ids: every
    /// `(doctype, id, lane, seq)` as [`record_document_entry`](Self::record_document_entry) and
    /// every `(event_id, lane, seq)` as [`record_appended`](Self::record_appended). The event ids
    /// are written after the document entries, as in the single calls.
    fn record_many<'a>(
        &'a self,
        documents: &'a [DocumentEntry],
        events: &'a [(String, u32, u64)],
    ) -> StateFuture<'a, ()> {
        Box::pin(async move {
            for (doctype, id, lane, seq) in documents {
                self.record_document_entry(doctype, id, *lane, *seq).await?;
            }
            for (event_id, lane, seq) in events {
                self.record_appended(event_id, *lane, *seq).await?;
            }
            Ok(())
        })
    }
}

/// One `(doctype, document id, lane, seq)` document-index entry.
pub type DocumentEntry = (String, String, u32, u64);

#[derive(Default)]
struct AppenderState {
    bookmarks: HashMap<u32, FeedPosition>,
    appended: HashMap<String, (u32, u64)>,
    documents: HashMap<(String, String), Vec<(u32, u64)>>,
    checkpoint: Option<SiteCheckpoint>,
}

/// The in-memory reference [`AppenderStore`]. Its futures are ready at once and never fail.
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

fn ready<'a, T: Send + 'a>(value: T) -> StateFuture<'a, T> {
    Box::pin(std::future::ready(Ok(value)))
}

impl AppenderStore for InMemoryAppenderStore {
    fn bookmark(&self, lane: u32) -> StateFuture<'_, FeedPosition> {
        ready(
            self.lock()
                .bookmarks
                .get(&lane)
                .copied()
                .unwrap_or(FeedPosition::START),
        )
    }

    fn set_bookmark(&self, lane: u32, position: FeedPosition) -> StateFuture<'_, ()> {
        self.lock().bookmarks.insert(lane, position);
        ready(())
    }

    fn appended<'a>(&'a self, event_id: &'a str) -> StateFuture<'a, Option<(u32, u64)>> {
        ready(self.lock().appended.get(event_id).copied())
    }

    fn record_appended<'a>(
        &'a self,
        event_id: &'a str,
        lane: u32,
        seq: u64,
    ) -> StateFuture<'a, ()> {
        self.lock()
            .appended
            .insert(event_id.to_owned(), (lane, seq));
        ready(())
    }

    fn document_entries<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> StateFuture<'a, Vec<(u32, u64)>> {
        ready(
            self.lock()
                .documents
                .get(&(doctype.to_owned(), id.to_owned()))
                .cloned()
                .unwrap_or_default(),
        )
    }

    fn record_document_entry<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
        lane: u32,
        seq: u64,
    ) -> StateFuture<'a, ()> {
        let mut state = self.lock();
        let entries = state
            .documents
            .entry((doctype.to_owned(), id.to_owned()))
            .or_default();
        // An upsert: the same entry recorded twice is one entry.
        if !entries.contains(&(lane, seq)) {
            entries.push((lane, seq));
        }
        drop(state);
        ready(())
    }

    fn batches(&self) -> bool {
        true
    }

    fn appended_many<'a>(
        &'a self,
        event_ids: &'a [String],
    ) -> StateFuture<'a, Vec<Option<(u32, u64)>>> {
        let state = self.lock();
        ready(
            event_ids
                .iter()
                .map(|id| state.appended.get(id).copied())
                .collect(),
        )
    }

    fn record_many<'a>(
        &'a self,
        documents: &'a [DocumentEntry],
        events: &'a [(String, u32, u64)],
    ) -> StateFuture<'a, ()> {
        let mut state = self.lock();
        for (doctype, id, lane, seq) in documents {
            let entries = state
                .documents
                .entry((doctype.clone(), id.clone()))
                .or_default();
            if !entries.contains(&(*lane, *seq)) {
                entries.push((*lane, *seq));
            }
        }
        for (event_id, lane, seq) in events {
            state.appended.insert(event_id.clone(), (*lane, *seq));
        }
        drop(state);
        ready(())
    }

    fn site_checkpoint(&self) -> StateFuture<'_, Option<SiteCheckpoint>> {
        ready(self.lock().checkpoint.clone())
    }

    fn set_site_checkpoint(&self, checkpoint: SiteCheckpoint) -> StateFuture<'_, ()> {
        self.lock().checkpoint = Some(checkpoint);
        ready(())
    }
}

/// The key of the chain-record payload that carries the outbox event id.
pub const EVENT_ID_KEY: &str = "event_id";

/// How many of the newest entries of its own lane the appender reads when it looks for an event
/// the index does not know. One active appender per lane (the fenced job lease) means a crash
/// leaves at most the last entry unindexed; 32 also covers an appender that took over from a
/// stalled one that got a few appends in before it was fenced. The window is fixed so recovery
/// cannot become a scan.
pub const RECOVERY_WINDOW: u64 = 32;

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
    /// The head `seq` of this lane up to which every entry is known to be in the event index (set
    /// by a group drain): the recovery window is not read again while the head is still there.
    indexed_through: Mutex<Option<u64>>,
}

/// How many of the lane's own events one group appends and indexes together. Below
/// [`RECOVERY_WINDOW`], so a crash between the chain write and the index writes of a group leaves
/// entries the recovery window still reaches.
const GROUP: usize = 16;

/// One of the lane's own outbox events waiting for its group.
enum Own {
    Doc(DocAuditEvent),
    Secret(SecretAuditEvent),
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
            indexed_through: Mutex::new(None),
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
    /// [`DocAuditError::Chain`] on a feed, decode or append failure; [`DocAuditError::AppenderState`]
    /// when the durable state cannot be read or written (the drain stops there).
    pub async fn drain(
        &self,
        feed: &dyn OutboxFeed,
        limit: u32,
    ) -> Result<DrainReport, DocAuditError> {
        let start = self.state.bookmark(self.lane).await?;
        let records = feed.read(start, limit).await?;
        if self.state.batches() && self.chain.store().batches_appends() {
            return self.drain_grouped(start, &records).await;
        }
        let mut appended = 0;
        let mut position = start;
        // The furthest position the durable bookmark holds. A record that is not this lane's (the
        // feed carries every lane's events and other topics) changes no state, so its position
        // is persisted together with the next record this lane does append, or when the drain
        // ends, not once per record: with several lanes reading one feed, a write per record
        // was most of the commits of a bulk write and serialised them on the bookmark rows.
        let mut stored = start;
        for record in &records {
            position = record.position;
            let topic = record.event.topic.as_str();
            let own = if topic == SECRET_AUDIT_TOPIC {
                let event = decode_secret_outbox(record)?;
                if lane_of(&event.subject_doctype, &event.subject_id, self.config.n) == self.lane {
                    let event_id = record.event.id.to_string();
                    match self.append_secret_event(&event_id, &event).await? {
                        AppendResult::Appended { .. } => appended += 1,
                        AppendResult::AlreadyChained { .. } => {}
                    }
                    true
                } else {
                    false
                }
            } else if topic == AUDIT_TOPIC {
                let event = decode_outbox(record)?;
                if lane_of(&event.doctype, &event.id, self.config.n) == self.lane {
                    let event_id = record.event.id.to_string();
                    match self.append_event(&event_id, &event).await? {
                        AppendResult::Appended { .. } => appended += 1,
                        AppendResult::AlreadyChained { .. } => {}
                    }
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if own {
                // The append is durable (the chain, then the index rows): advance past it.
                self.state.set_bookmark(self.lane, position).await?;
                stored = position;
            }
        }
        if stored != position {
            self.state.set_bookmark(self.lane, position).await?;
        }
        Ok(DrainReport {
            read: records.len(),
            appended,
            position,
        })
    }

    /// [`drain`](Self::drain) for stores that batch: the lane's own events are chained and
    /// indexed in groups of [`GROUP`], each group with one read of the index, one chain write,
    /// one index write and one bookmark write. The order of the chain, the exactly-once rule
    /// (index first, then the newest [`RECOVERY_WINDOW`] entries) and the bookmark rule (never
    /// past an event that is not chained and indexed) are the single-event path's.
    async fn drain_grouped(
        &self,
        start: FeedPosition,
        records: &[OutboxRecord],
    ) -> Result<DrainReport, DocAuditError> {
        let mut appended = 0;
        let mut position = start;
        let mut stored = start;
        let mut group: Vec<(String, Own, FeedPosition)> = Vec::new();
        for record in records {
            position = record.position;
            let topic = record.event.topic.as_str();
            let own = if topic == SECRET_AUDIT_TOPIC {
                match decode_secret_outbox(record) {
                    Ok(event)
                        if lane_of(&event.subject_doctype, &event.subject_id, self.config.n)
                            == self.lane =>
                    {
                        Some(Own::Secret(event))
                    }
                    Ok(_) => None,
                    Err(e) => return self.fail_after(&mut group, &mut appended, e).await,
                }
            } else if topic == AUDIT_TOPIC {
                match decode_outbox(record) {
                    Ok(event) if lane_of(&event.doctype, &event.id, self.config.n) == self.lane => {
                        Some(Own::Doc(event))
                    }
                    Ok(_) => None,
                    Err(e) => return self.fail_after(&mut group, &mut appended, e).await,
                }
            } else {
                None
            };
            if let Some(own) = own {
                group.push((record.event.id.to_string(), own, position));
                if group.len() >= GROUP {
                    appended += self.flush_group(&mut group).await?;
                    stored = position;
                }
            }
        }
        if !group.is_empty() {
            appended += self.flush_group(&mut group).await?;
            stored = position;
        }
        if stored != position {
            self.state.set_bookmark(self.lane, position).await?;
        }
        Ok(DrainReport {
            read: records.len(),
            appended,
            position,
        })
    }

    /// A record that cannot be decoded ends the drain, after the events before it are done.
    async fn fail_after(
        &self,
        group: &mut Vec<(String, Own, FeedPosition)>,
        appended: &mut usize,
        error: DocAuditError,
    ) -> Result<DrainReport, DocAuditError> {
        if !group.is_empty() {
            *appended += self.flush_group(group).await?;
        }
        Err(error)
    }

    /// Chains and indexes one group of the lane's own events, then moves the bookmark to the
    /// group's last position. Returns how many events were appended (not found chained already).
    async fn flush_group(
        &self,
        group: &mut Vec<(String, Own, FeedPosition)>,
    ) -> Result<usize, DocAuditError> {
        let items = std::mem::take(group);
        let Some(last) = items.last().map(|(_, _, position)| *position) else {
            return Ok(0);
        };
        // An id seen twice in one group is chained once (the second is "already chained").
        let mut seen = std::collections::HashSet::new();
        let items: Vec<&(String, Own, FeedPosition)> = items
            .iter()
            .filter(|(id, _, _)| seen.insert(id.as_str()))
            .collect();
        let ids: Vec<String> = items.iter().map(|(id, _, _)| id.clone()).collect();
        let known = self.state.appended_many(&ids).await?;

        let mut documents: Vec<DocumentEntry> = Vec::new();
        let mut events: Vec<(String, u32, u64)> = Vec::new();
        let mut fresh: Vec<&(String, Own, FeedPosition)> = Vec::new();
        for (item, found) in items.iter().zip(known) {
            match (found, &item.1) {
                // An event indexed by an older version may lack its document entry.
                (Some((lane, seq)), Own::Doc(event)) => {
                    documents.push((event.doctype.clone(), event.id.clone(), lane, seq));
                }
                (Some(_), Own::Secret(_)) => {}
                (None, _) => fresh.push(item),
            }
        }

        // Fresh events the chain already holds (a crash between the chain write and the index
        // writes): adopted, not chained twice. The window is read only when the lane's head moved
        // since this appender last indexed everything.
        let mut head_seq = None;
        let mut adopted: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        if !fresh.is_empty() {
            let site = lane_site(&self.site, self.lane);
            head_seq = self.store.head(&site).await?.map(|h| h.seq);
            let clean = *self
                .indexed_through
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(head) = head_seq.filter(|head| clean != Some(*head)) {
                let from = head
                    .saturating_sub(RECOVERY_WINDOW.saturating_sub(1))
                    .max(1);
                for entry in self.store.range(&site, from, head).await? {
                    if let Some(PayloadValue::Text(id)) = entry.record.payload.get(EVENT_ID_KEY) {
                        adopted.insert(id.clone(), entry.seq);
                    }
                }
            }
        }
        let mut new: Vec<&(String, Own, FeedPosition)> = Vec::new();
        for item in fresh {
            match adopted.get(&item.0) {
                Some(seq) => {
                    if let Own::Doc(event) = &item.1 {
                        documents.push((event.doctype.clone(), event.id.clone(), self.lane, *seq));
                    }
                    events.push((item.0.clone(), self.lane, *seq));
                }
                None => new.push(item),
            }
        }

        // The rest are chained in order. A record that cannot be built ends the group there: the
        // events before it are chained and indexed, then the error is returned.
        let mut records = Vec::with_capacity(new.len());
        let mut failure = None;
        for item in &new {
            let built = match &item.1 {
                Own::Doc(event) => record_of_with_id(event, &item.0),
                Own::Secret(event) => secret_record_of_with_id(event, &item.0),
            };
            match built {
                Ok(record) => records.push(record),
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
        }
        let entries = self.chain.append_many(records).await?;
        for (item, entry) in new.iter().zip(&entries) {
            if let Own::Doc(event) = &item.1 {
                documents.push((
                    event.doctype.clone(),
                    event.id.clone(),
                    self.lane,
                    entry.seq,
                ));
            }
            events.push((item.0.clone(), self.lane, entry.seq));
        }
        // The index rows last, then the bookmark: a crash before them leaves what the recovery
        // window finds.
        self.state.record_many(&documents, &events).await?;
        let head_now = entries.last().map(|e| e.seq).or(head_seq);
        *self
            .indexed_through
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = head_now;
        if let Some(error) = failure {
            return Err(error);
        }
        self.state.set_bookmark(self.lane, last).await?;
        Ok(entries.len())
    }

    /// The sequence number of the entry of this lane that carries `event_id`, looking only at the
    /// newest [`RECOVERY_WINDOW`] entries.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] when the chain cannot be read (never "not found").
    async fn chained_at(&self, event_id: &str) -> Result<Option<u64>, DocAuditError> {
        let site = lane_site(&self.site, self.lane);
        let Some(head) = self.store.head(&site).await? else {
            return Ok(None);
        };
        let from = head
            .seq
            .saturating_sub(RECOVERY_WINDOW.saturating_sub(1))
            .max(1);
        let entries = self.store.range(&site, from, head.seq).await?;
        Ok(entries
            .iter()
            .rev()
            .find(|entry| carries_event_id(entry, event_id))
            .map(|entry| entry.seq))
    }

    /// Appends one event to the lane chain, or reports it already chained.
    ///
    /// Exactly once: an event the index knows is not appended again; an event the index does not
    /// know is first looked for among the newest entries of the lane (a crash between the chain
    /// commit and the index write leaves it there) and adopted instead of chained twice.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] when the append or the chain read fails;
    /// [`DocAuditError::AppenderState`] when the durable state cannot be read or written.
    pub async fn append_event(
        &self,
        event_id: &str,
        event: &DocAuditEvent,
    ) -> Result<AppendResult, DocAuditError> {
        if let Some((lane, seq)) = self.state.appended(event_id).await? {
            // An event indexed by an older version of this code may lack its document entry.
            self.state
                .record_document_entry(&event.doctype, &event.id, lane, seq)
                .await?;
            return Ok(AppendResult::AlreadyChained { lane, seq });
        }
        if let Some(seq) = self.chained_at(event_id).await? {
            self.state
                .record_document_entry(&event.doctype, &event.id, self.lane, seq)
                .await?;
            self.state.record_appended(event_id, self.lane, seq).await?;
            return Ok(AppendResult::AlreadyChained {
                lane: self.lane,
                seq,
            });
        }
        let record = record_of_with_id(event, event_id)?;
        let entry = self.chain.append(record).await?;
        self.state
            .record_document_entry(&event.doctype, &event.id, self.lane, entry.seq)
            .await?;
        // Last: any crash before this line leaves a state the recovery above completes.
        self.state
            .record_appended(event_id, self.lane, entry.seq)
            .await?;
        Ok(AppendResult::Appended { seq: entry.seq })
    }

    /// Appends one secret audit event to the lane chain, or reports it already chained, with the
    /// same recovery as [`LaneAppender::append_event`].
    ///
    /// The entry is not recorded in the per-document index: that index feeds the document
    /// history, which shows document changes only.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] when the append or the chain read fails;
    /// [`DocAuditError::AppenderState`] when the durable state cannot be read or written.
    pub async fn append_secret_event(
        &self,
        event_id: &str,
        event: &SecretAuditEvent,
    ) -> Result<AppendResult, DocAuditError> {
        if let Some((lane, seq)) = self.state.appended(event_id).await? {
            return Ok(AppendResult::AlreadyChained { lane, seq });
        }
        if let Some(seq) = self.chained_at(event_id).await? {
            self.state.record_appended(event_id, self.lane, seq).await?;
            return Ok(AppendResult::AlreadyChained {
                lane: self.lane,
                seq,
            });
        }
        let record = secret_record_of_with_id(event, event_id)?;
        let entry = self.chain.append(record).await?;
        self.state
            .record_appended(event_id, self.lane, entry.seq)
            .await?;
        Ok(AppendResult::Appended { seq: entry.seq })
    }

    /// Signs and stores a site checkpoint of every lane's current head.
    ///
    /// # Errors
    /// [`DocAuditError::Chain`] on a store or signer failure.
    pub async fn sign_checkpoint(&self, at: i64) -> Result<SiteCheckpoint, DocAuditError> {
        let heads = self.lane_heads().await?;
        let checkpoint = SiteCheckpoint::sign(&*self.signer, &self.site, &self.config, &heads, at)?;
        self.state.set_site_checkpoint(checkpoint.clone()).await?;
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
    build_record(event, None)
}

/// [`record_of`] for an outbox event: the record also carries the outbox event id as
/// `payload["event_id"]`, which is what the appender recovers a half-finished append by. The id
/// counts toward the payload size, so the compact fallback applies with it.
///
/// # Errors
/// As [`record_of`].
pub fn record_of_with_id(
    event: &DocAuditEvent,
    event_id: &str,
) -> Result<AuditRecord, DocAuditError> {
    build_record(event, Some(event_id))
}

fn build_record(
    event: &DocAuditEvent,
    event_id: Option<&str>,
) -> Result<AuditRecord, DocAuditError> {
    let json = encode_event(event)?;
    let digest = sha256_hex(json.as_bytes());
    let mut payload: BTreeMap<String, PayloadValue> = BTreeMap::new();
    payload.insert("event".to_owned(), PayloadValue::Text(json));
    payload.insert("digest".to_owned(), PayloadValue::Text(digest));
    payload.insert("truncated".to_owned(), PayloadValue::Bool(event.truncated));
    if let Some(d) = &event.diff_digest {
        payload.insert("diff_digest".to_owned(), PayloadValue::Text(d.clone()));
    }
    if let Some(id) = event_id {
        payload.insert(EVENT_ID_KEY.to_owned(), PayloadValue::Text(id.to_owned()));
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

/// Whether a chain entry carries the outbox event id `event_id` (entries written before the id
/// was added carry none and never match).
fn carries_event_id(entry: &ChainEntry, event_id: &str) -> bool {
    matches!(
        entry.record.payload.get(EVENT_ID_KEY),
        Some(PayloadValue::Text(text)) if text == event_id
    )
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
