//! The engine: named consumers over the outbox, with checkpoints, partitions, filtering,
//! flow control and resync handling. It opens no socket and spawns nothing; the host drives
//! [`StreamEngine::pull`] and passes the clock.
//!
//! # Delivery level D1 (ADR-078 section 6)
//!
//! At-least-once and resumable. An event is visible only after the transaction that produced
//! it committed, because the outbox holds committed entries only. [`StreamEngine::pull`] reads
//! from a checkpoint and builds envelopes; it **never** advances a checkpoint on delivery.
//! Only [`StreamEngine::ack`] advances it, and only for the position of a batch the caller was
//! actually delivered (ack-before-advance). A crash between delivery and ack redelivers exactly
//! the unacknowledged events with the same event ids, so the consumer deduplicates.
//!
//! # Order
//!
//! Events are returned in `(generation, seq, ordinal)` order **within one partition**. There is
//! **no ordering promise across partitions**: a consumer that needs a total order uses one
//! partition. A partition is `fnv1a(subject.id) mod partitions`, fixed for the life of the
//! consumer.
//!
//! # Isolation
//!
//! One active connection per consumer: [`StreamEngine::acquire`] takes a fenced lease from the
//! [`Coordinator`]; a second connection is refused with `eventstream.consumer_busy`, and a
//! holder that lost its lease cannot ack. A consumer that stops acking is **parked per
//! partition** (it stops consuming engine capacity) without touching any other consumer.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_coordinator::Coordinator;
use spot_coordinator::error::LEASE_HELD;
use spot_core::Clock;
use spot_events_contract::envelope::{EventEnvelope, Position};
use spot_events_contract::names::SourceId;
use spot_expr::{DateTime, Expr, MapContext, Value as ExprValue, parse as parse_expr};
use spot_permissions::{Principal, TENANT_ADMINISTRATOR};
use spot_storage_contract::{ChangeFeed, ErrorKind, FeedPosition};

use crate::audit::{AuditRecord, AuditSink};
use crate::error::StreamError;
use crate::position::{position_from_feed, position_to_feed};
use crate::spec::{ConsumerSpec, PayloadMode, Start};
use crate::store::{ConsumerRecord, ConsumerStore, RegisterOutcome};
use crate::visibility::Visibility;

pub use crate::policy::{
    AllowAllStreamRights, Batch, BatchPolicy, ConsumerLease, DenyStreamRights, FeedHead,
    ParkPolicy, ParkedConsumer, StreamRights,
};

/// The injected seams of an engine.
pub struct Seams<F: ChangeFeed> {
    /// The outbox feed to read.
    pub feed: Arc<F>,
    /// The outbox head, when known.
    pub head: Option<Arc<dyn FeedHead>>,
    /// The consumer registry and checkpoints.
    pub store: Arc<dyn ConsumerStore>,
    /// The fenced leases behind one-connection-per-consumer.
    pub coordinator: Arc<dyn Coordinator>,
    /// The read filter and the field projection.
    pub visibility: Arc<dyn Visibility>,
    /// The audit trail.
    pub audit: Arc<dyn AuditSink>,
    /// The clock; the engine reads time only through it.
    pub clock: Arc<dyn Clock>,
    /// The reserved-class stream rights.
    pub rights: Arc<dyn StreamRights>,
}

/// The engine's own settings.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// The source database id stamped on every envelope.
    pub source: SourceId,
    /// The batch limits.
    pub policy: BatchPolicy,
    /// The parking policy.
    pub park: ParkPolicy,
    /// The connection holder name used in the coordinator lease.
    pub holder: String,
    /// The lease ttl.
    pub lease_ttl: Duration,
}

impl EngineConfig {
    /// A config for `source` with the default policy, park policy, a `"stream"` holder and a
    /// 60-second lease.
    #[must_use]
    pub fn new(source: SourceId) -> Self {
        Self {
            source,
            policy: BatchPolicy::default(),
            park: ParkPolicy::default(),
            holder: "stream".to_owned(),
            lease_ttl: Duration::from_secs(60),
        }
    }
}

/// The per-partition in-memory state of one engine instance.
#[derive(Debug, Default)]
struct PartitionState {
    /// The next feed position to read from (advances past scanned entries, acked or not).
    read_cursor: Option<FeedPosition>,
    /// The in-flight window: delivered but unacknowledged events.
    delivered: Vec<Delivered>,
    /// The `next_position` of the last delivered batch.
    last_delivered: Option<Position>,
}

#[derive(Debug)]
struct Delivered {
    position: Position,
    at: Duration,
}

/// What a pull's window/parking check decided, computed under the lock and acted on after.
enum Gate {
    /// The partition is stalled and must be parked.
    Park,
    /// The in-flight window is full.
    WindowFull,
    /// The pull may read the feed.
    Proceed,
}

/// The engine. Generic over the feed because [`ChangeFeed`] is not dyn-compatible (its methods
/// return `impl Future`).
pub struct StreamEngine<F: ChangeFeed> {
    source: SourceId,
    feed: Arc<F>,
    head: Option<Arc<dyn FeedHead>>,
    store: Arc<dyn ConsumerStore>,
    coordinator: Arc<dyn Coordinator>,
    visibility: Arc<dyn Visibility>,
    audit: Arc<dyn AuditSink>,
    clock: Arc<dyn Clock>,
    rights: Arc<dyn StreamRights>,
    policy: BatchPolicy,
    park: ParkPolicy,
    holder: String,
    lease_ttl: Duration,
    windows: Mutex<HashMap<(String, u32), PartitionState>>,
    filters: Mutex<HashMap<String, Expr>>,
}

impl<F: ChangeFeed> StreamEngine<F> {
    /// An engine over the given seams and config.
    #[must_use]
    pub fn new(seams: Seams<F>, config: EngineConfig) -> Self {
        Self {
            source: config.source,
            feed: seams.feed,
            head: seams.head,
            store: seams.store,
            coordinator: seams.coordinator,
            visibility: seams.visibility,
            audit: seams.audit,
            clock: seams.clock,
            rights: seams.rights,
            policy: config.policy,
            park: config.park,
            holder: config.holder,
            lease_ttl: config.lease_ttl,
            windows: Mutex::new(HashMap::new()),
            filters: Mutex::new(HashMap::new()),
        }
    }

    // ------------------------------------------------------------------ registry

    /// Registers a consumer on behalf of `actor`.
    ///
    /// The actor must be the owner or a tenant administrator. A reserved topic class needs the
    /// owner's explicit stream right. Re-registering with an identical definition is
    /// idempotent; a changed definition is refused (a changed partition count is a new
    /// consumer).
    ///
    /// # Errors
    ///
    /// [`StreamError::SpecInvalid`] for a bad definition or a reserved class the owner may not
    /// stream; [`StreamError::ConsumerNotFound`] (worded as a denial) when the actor may not
    /// act for the owner.
    pub async fn register(
        &self,
        spec: ConsumerSpec,
        actor: &Principal,
    ) -> Result<ConsumerRecord, StreamError> {
        spec.validate()?;
        self.require_owner(actor, &spec.owner, &spec.name)?;
        for selector in &spec.topics {
            if let Some(class) = selector.reserved_class()
                && !self.rights.may_stream_class(&spec.owner, class)
            {
                return Err(StreamError::spec(format!(
                    "the owner holds no stream right for the reserved class {class}"
                )));
            }
        }
        let mut record = ConsumerRecord::new(spec, self.clock.now_utc());
        if matches!(record.spec.start, Start::Tail)
            && let Some(head) = self.head.as_ref().and_then(|h| h.head())
        {
            for checkpoint in &mut record.checkpoints {
                *checkpoint = position_from_feed(head);
            }
        }
        // Atomic create: two nodes starting the same new consumer, or a node that read before
        // another registered and advanced, can never reset acknowledged progress.
        match self.store.register_if_absent(&record).await? {
            RegisterOutcome::Created => {
                if let Some(filter) = &record.spec.filter
                    && let Ok(expr) = parse_expr(filter)
                {
                    self.filters
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(record.spec.name.clone(), expr);
                }
            }
            RegisterOutcome::Existing(existing) => {
                if existing.spec == record.spec {
                    return Ok(*existing);
                }
                return Err(StreamError::spec(
                    "the consumer already exists with a different definition; a new partition count or topics is a new consumer",
                ));
            }
        }
        self.audit.record(AuditRecord::Registered {
            actor: actor.user.clone(),
            consumer: record.spec.name.clone(),
            at: self.clock.now_utc(),
        });
        Ok(record)
    }

    /// Loads a consumer on behalf of `actor` (the owner or a tenant administrator).
    ///
    /// # Errors
    ///
    /// [`StreamError::ConsumerNotFound`] when it does not exist or the actor may not read it.
    pub async fn get(&self, name: &str, actor: &Principal) -> Result<ConsumerRecord, StreamError> {
        let record = self.load(name).await?;
        self.require_owner(actor, &record.spec.owner, name)?;
        Ok(record)
    }

    /// Lists the consumers `actor` may see, in name order.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn list(&self, actor: &Principal) -> Result<Vec<ConsumerRecord>, StreamError> {
        let all = self.store.list().await?;
        Ok(all
            .into_iter()
            .filter(|r| self.may_act(actor, &r.spec.owner))
            .collect())
    }

    /// Deletes a consumer on behalf of `actor`.
    ///
    /// # Errors
    ///
    /// [`StreamError::ConsumerNotFound`] when it does not exist or the actor may not delete it.
    pub async fn delete(&self, name: &str, actor: &Principal) -> Result<(), StreamError> {
        let record = self.load(name).await?;
        self.require_owner(actor, &record.spec.owner, name)?;
        self.store.delete(name).await?;
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(c, _), _| c != name);
        self.filters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
        self.audit.record(AuditRecord::Deleted {
            actor: actor.user.clone(),
            consumer: name.to_owned(),
            at: self.clock.now_utc(),
        });
        Ok(())
    }

    /// The acknowledged checkpoint of one partition.
    ///
    /// # Errors
    ///
    /// [`StreamError::ConsumerNotFound`] or [`StreamError::SpecInvalid`].
    pub async fn checkpoint(&self, name: &str, partition: u32) -> Result<Position, StreamError> {
        self.store.checkpoint(name, partition).await
    }

    /// The retention horizon: the earliest checkpoint over every registered consumer and
    /// partition. It is never later than the slowest registered consumer's checkpoint.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn retained_from(&self) -> Result<Position, StreamError> {
        let mut min: Option<FeedPosition> = None;
        for record in self.store.list().await? {
            for checkpoint in &record.checkpoints {
                let feed = position_to_feed(*checkpoint);
                min = Some(min.map_or(feed, |m| m.min(feed)));
            }
        }
        Ok(min.map_or(
            Position {
                generation: 0,
                seq: 0,
            },
            position_from_feed,
        ))
    }

    /// The partitions parked at least [`ParkPolicy::report_after`] ago, by name.
    ///
    /// # Errors
    ///
    /// A store failure.
    pub async fn parked_report(&self) -> Result<Vec<ParkedConsumer>, StreamError> {
        let now = self.clock.now_utc();
        let report_after =
            time::Duration::try_from(self.park.report_after).unwrap_or(time::Duration::ZERO);
        let mut out = Vec::new();
        for record in self.store.list().await? {
            for (index, parked) in record.parked.iter().enumerate() {
                if !*parked {
                    continue;
                }
                if let Some(since) = record.parked_since[index]
                    && now - since >= report_after
                {
                    out.push(ParkedConsumer {
                        consumer: record.spec.name.clone(),
                        partition: u32::try_from(index).unwrap_or(0),
                        since,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Unparks a partition so an operator can resume it.
    ///
    /// # Errors
    ///
    /// [`StreamError::ConsumerNotFound`] or [`StreamError::SpecInvalid`].
    pub async fn unpark(&self, name: &str, partition: u32) -> Result<(), StreamError> {
        self.load(name).await?;
        self.store.unpark(name, partition).await
    }

    // ------------------------------------------------------------------ connection lease

    /// Gives the connection lease back, so another connection may take it.
    ///
    /// # Errors
    ///
    /// [`StreamError::Coordinator`] when the coordinator refuses (a stale lease is ignored).
    pub async fn release(&self, lease: &ConsumerLease) -> Result<(), StreamError> {
        match self.coordinator.release(&lease.lease).await {
            Ok(()) => Ok(()),
            Err(error) => Err(StreamError::Coordinator {
                code: error.code,
                message: error.to_string(),
            }),
        }
    }

    /// Takes the consumer's fenced lease, refusing a second connection.
    ///
    /// # Errors
    ///
    /// [`StreamError::ConsumerNotFound`] when the consumer does not exist;
    /// [`StreamError::ConsumerBusy`] when another connection holds it.
    pub async fn acquire(&self, name: &str) -> Result<ConsumerLease, StreamError> {
        self.load(name).await?;
        let lease_name = self.lease_name(name);
        match self
            .coordinator
            .acquire(&lease_name, &self.holder, self.lease_ttl)
            .await
        {
            Ok(lease) => Ok(ConsumerLease {
                consumer: name.to_owned(),
                lease,
            }),
            Err(error) if error.code == LEASE_HELD => Err(StreamError::ConsumerBusy {
                name: name.to_owned(),
                reason: error.message,
            }),
            Err(error) => Err(StreamError::Coordinator {
                code: error.code,
                message: error.to_string(),
            }),
        }
    }

    // ------------------------------------------------------------------ pull

    /// Reads one batch for `partition`, from the checkpoint through the feed.
    ///
    /// Applies the topic selector, the filter and the permission filter, then builds the
    /// envelopes in order. It never commits the checkpoint. `limit` caps the events in this
    /// batch (a smaller value than [`BatchPolicy::max_events`] wins); `wait` makes a full
    /// in-flight window a [`StreamError::WindowFull`] rather than an empty flagged batch.
    ///
    /// # Errors
    ///
    /// [`StreamError::ConsumerNotFound`], [`StreamError::SpecInvalid`],
    /// [`StreamError::ConsumerBusy`] (lease lost), [`StreamError::ResyncRequired`],
    /// [`StreamError::WindowFull`], [`StreamError::Feed`] or [`StreamError::Permission`].
    pub async fn pull(
        &self,
        lease: &ConsumerLease,
        partition: u32,
        limit: Option<u32>,
        wait: bool,
    ) -> Result<Batch, StreamError> {
        let name = lease.consumer.as_str();
        let record = self.load(name).await?;
        let index = record.index(partition)?;
        if record.parked[index] {
            return Ok(Batch::parked());
        }
        self.check_fence(name, lease).await?;

        let now = self.clock.monotonic();
        let window = self.policy.window;
        let (gate, window_remaining) = {
            let mut windows = self.windows();
            let state = windows.entry((name.to_owned(), partition)).or_default();
            let remaining = window.saturating_sub(state.delivered.len());
            let gate = if self.stalled(state, now) {
                Gate::Park
            } else if remaining == 0 {
                Gate::WindowFull
            } else {
                Gate::Proceed
            };
            (gate, remaining)
        };
        match gate {
            Gate::Park => {
                self.park_partition(name, partition).await?;
                return Ok(Batch::parked());
            }
            Gate::WindowFull => {
                if wait {
                    return Err(StreamError::WindowFull {
                        name: name.to_owned(),
                        partition,
                    });
                }
                return Ok(Batch::window_full());
            }
            Gate::Proceed => {}
        }

        let checkpoint = record.checkpoints[index];
        let from = {
            let windows = self.windows();
            match windows
                .get(&(name.to_owned(), partition))
                .and_then(|s| s.read_cursor)
            {
                Some(cursor) => cursor,
                None => position_to_feed(checkpoint),
            }
        };

        let records = match self.feed.read_outbox(from, self.policy.page_limit).await {
            Ok(records) => records,
            Err(error) if error.kind == ErrorKind::ResyncRequired => {
                let retained = self.retained_from().await?;
                return Err(StreamError::ResyncRequired {
                    name: name.to_owned(),
                    retained_from: retained,
                    reason: error.explanation,
                });
            }
            Err(error) => return Err(StreamError::Feed(error)),
        };

        let max_events = limit
            .map_or(self.policy.max_events, |l| l as usize)
            .min(self.policy.max_events)
            .min(window_remaining);
        let filter = self.filter_for(&record.spec);
        let mut events: Vec<EventEnvelope> = Vec::new();
        let mut skipped = 0u64;
        let mut scanned = from;
        let mut bytes = 0usize;

        for rec in records {
            if record.spec.partition_for(&rec.event.subject.id) != partition {
                scanned = rec.position;
                continue;
            }
            let mut envelope = match EventEnvelope::from_record(&self.source, &rec) {
                Ok(envelope) => envelope,
                Err(error) => {
                    // A malformed payload is a producer bug; the event is skipped, not half-sent.
                    skipped += 1;
                    scanned = rec.position;
                    let _ = error;
                    continue;
                }
            };
            if !record.spec.matches_topic(envelope.topic.as_str()) {
                scanned = rec.position;
                continue;
            }
            if let Some(expr) = &filter
                && !self.filter_matches(expr, &envelope)
            {
                skipped += 1;
                scanned = rec.position;
                continue;
            }
            if !self
                .visibility
                .may_read(&record.spec.owner, &envelope.subject)?
            {
                skipped += 1;
                scanned = rec.position;
                continue;
            }
            self.apply_mode(&record.spec, &mut envelope)?;
            let size = serde_json::to_string(&envelope).map_or(0, |text| text.len());
            if !events.is_empty() && bytes.saturating_add(size) > self.policy.max_bytes {
                break;
            }
            bytes = bytes.saturating_add(size);
            events.push(envelope);
            scanned = rec.position;
            if events.len() >= max_events {
                break;
            }
        }

        let next_position = events.last().map(|e| e.position);
        {
            let mut windows = self.windows();
            let state = windows.entry((name.to_owned(), partition)).or_default();
            state.read_cursor = Some(scanned);
            if let Some(position) = next_position {
                state.last_delivered = Some(position);
                for event in &events {
                    state.delivered.push(Delivered {
                        position: event.position,
                        at: now,
                    });
                }
            }
        }
        Ok(Batch {
            events,
            next_position,
            skipped,
            window_full: false,
            parked: false,
        })
    }

    // ------------------------------------------------------------------ ack / restart

    /// Acknowledges a batch's own position, advancing the checkpoint (ack-before-advance).
    ///
    /// The position must be the `next_position` of a batch this connection was delivered, the
    /// connection must still hold the lease, and the checkpoint must not have moved since it
    /// was read. Acknowledging the same position twice is idempotent.
    ///
    /// # Errors
    ///
    /// [`StreamError::ConsumerNotFound`], [`StreamError::SpecInvalid`],
    /// [`StreamError::ConsumerBusy`] (lease lost) or [`StreamError::AckRefused`].
    pub async fn ack(
        &self,
        lease: &ConsumerLease,
        partition: u32,
        position: Position,
    ) -> Result<(), StreamError> {
        let name = lease.consumer.as_str();
        let record = self.load(name).await?;
        let index = record.index(partition)?;
        self.check_fence(name, lease).await?;

        let expected = {
            let windows = self.windows();
            match windows
                .get(&(name.to_owned(), partition))
                .and_then(|s| s.last_delivered)
            {
                Some(position) => position,
                None => {
                    return Err(StreamError::ack(
                        "no batch is outstanding for this connection and partition",
                    ));
                }
            }
        };
        if position_to_feed(position) != position_to_feed(expected) {
            return Err(StreamError::ack(
                "the position is not the delivered batch's own position",
            ));
        }
        let current = record.checkpoints[index];
        self.store
            .advance(name, partition, position, current)
            .await?;

        {
            let mut windows = self.windows();
            let state = windows.entry((name.to_owned(), partition)).or_default();
            let acked = position_to_feed(position);
            state
                .delivered
                .retain(|d| position_to_feed(d.position) > acked);
        }
        self.audit.record(AuditRecord::Acked {
            actor: record.spec.owner.clone(),
            consumer: name.to_owned(),
            partition,
            position: position.seq,
            generation: position.generation,
            at: self.clock.now_utc(),
        });
        Ok(())
    }

    /// Restarts a partition from a stated position, after a resync.
    ///
    /// This is the one operation that moves a checkpoint backwards. It clears the in-flight
    /// window and continues from `from`; a position still lost to retention resyncs again on
    /// the next pull.
    ///
    /// # Errors
    ///
    /// [`StreamError::ConsumerNotFound`], [`StreamError::SpecInvalid`],
    /// [`StreamError::ConsumerBusy`] (lease lost).
    pub async fn restart(
        &self,
        lease: &ConsumerLease,
        partition: u32,
        from: Position,
    ) -> Result<(), StreamError> {
        let name = lease.consumer.as_str();
        let record = self.load(name).await?;
        let _ = record.index(partition)?;
        self.check_fence(name, lease).await?;
        self.store.restart(name, partition, from).await?;
        {
            let mut windows = self.windows();
            let state = windows.entry((name.to_owned(), partition)).or_default();
            state.read_cursor = Some(position_to_feed(from));
            state.delivered.clear();
            state.last_delivered = None;
        }
        self.audit.record(AuditRecord::Restarted {
            actor: record.spec.owner.clone(),
            consumer: name.to_owned(),
            partition,
            position: from.seq,
            generation: from.generation,
            at: self.clock.now_utc(),
        });
        Ok(())
    }

    // ------------------------------------------------------------------ internals

    async fn load(&self, name: &str) -> Result<ConsumerRecord, StreamError> {
        self.store
            .get(name)
            .await?
            .ok_or_else(|| StreamError::ConsumerNotFound {
                name: name.to_owned(),
            })
    }

    fn windows(&self) -> std::sync::MutexGuard<'_, HashMap<(String, u32), PartitionState>> {
        self.windows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lease_name(&self, name: &str) -> String {
        format!("eventstream.consumer.{name}")
    }

    fn may_act(&self, actor: &Principal, owner: &str) -> bool {
        actor.is_system || actor.user == owner || is_tenant_admin(actor)
    }

    fn require_owner(&self, actor: &Principal, owner: &str, name: &str) -> Result<(), StreamError> {
        if self.may_act(actor, owner) {
            Ok(())
        } else {
            // Worded identically to a missing consumer: a refusal reveals nothing.
            Err(StreamError::ConsumerNotFound {
                name: name.to_owned(),
            })
        }
    }

    async fn check_fence(&self, name: &str, lease: &ConsumerLease) -> Result<(), StreamError> {
        match self
            .coordinator
            .check_fence(&self.lease_name(name), lease.lease.token)
            .await
        {
            Ok(()) => Ok(()),
            Err(error) => Err(StreamError::ConsumerBusy {
                name: name.to_owned(),
                reason: format!("the lease is no longer held: {}", error.message),
            }),
        }
    }

    fn stalled(&self, state: &PartitionState, now: Duration) -> bool {
        match state.delivered.iter().map(|d| d.at).min() {
            Some(oldest) => now.saturating_sub(oldest) > self.park.park_after,
            None => false,
        }
    }

    async fn park_partition(&self, name: &str, partition: u32) -> Result<(), StreamError> {
        let at = self.clock.now_utc();
        self.store.park(name, partition, at).await?;
        self.audit.record(AuditRecord::Parked {
            consumer: name.to_owned(),
            partition,
            at,
        });
        Ok(())
    }

    fn filter_for(&self, spec: &ConsumerSpec) -> Option<Expr> {
        let filter = spec.filter.as_ref()?;
        if let Some(expr) = self
            .filters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&spec.name)
        {
            return Some(expr.clone());
        }
        let expr = parse_expr(filter).ok()?;
        self.filters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(spec.name.clone(), expr.clone());
        Some(expr)
    }

    /// Evaluates a filter over the envelope's `meta` and `subject`. An evaluation error is a
    /// non-match (the consumer's own data is wrong), never a failed pull.
    fn filter_matches(&self, expr: &Expr, envelope: &EventEnvelope) -> bool {
        let micros =
            i64::try_from(self.clock.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(0);
        let Some(now) = DateTime::from_micros(micros) else {
            return false;
        };
        let mut context = MapContext::new(now);
        let view = serde_json::json!({
            "meta": envelope.meta,
            "subject": envelope.subject,
        });
        context = flatten(context, "doc", &view, 0);
        match expr.eval(&context) {
            Ok(ExprValue::Bool(b)) => b,
            Ok(ExprValue::Null) => false,
            Ok(_) | Err(_) => false,
        }
    }

    fn apply_mode(
        &self,
        spec: &ConsumerSpec,
        envelope: &mut EventEnvelope,
    ) -> Result<(), StreamError> {
        match spec.payload_mode {
            PayloadMode::IdsOnly => {
                envelope.data = serde_json::json!({});
                Ok(())
            }
            PayloadMode::Fields => {
                let mut map = envelope
                    .data
                    .as_object()
                    .cloned()
                    .unwrap_or_else(serde_json::Map::new);
                self.visibility
                    .project(&spec.owner, &envelope.subject, &mut map)?;
                envelope.data = serde_json::Value::Object(map);
                Ok(())
            }
        }
    }
}

/// Whether the principal is a tenant administrator.
#[must_use]
pub fn is_tenant_admin(principal: &Principal) -> bool {
    principal.assignments.iter().any(|assignment| {
        assignment
            .roles
            .iter()
            .any(|role| role.0 == TENANT_ADMINISTRATOR)
    })
}

/// The deepest nested object flattened into the filter context; a deeper value reads as `null`.
const MAX_FILTER_DEPTH: u32 = 4;

/// Flattens `value` under `prefix` into the flat context: objects become dotted paths, arrays
/// become lists of scalars, and a non-scalar inside a list reads as `null`.
fn flatten(context: MapContext, prefix: &str, value: &serde_json::Value, depth: u32) -> MapContext {
    match value {
        serde_json::Value::Object(map) => {
            if depth > MAX_FILTER_DEPTH {
                return context;
            }
            let mut context = context;
            for (key, child) in map {
                context = flatten(context, &format!("{prefix}.{key}"), child, depth + 1);
            }
            context
        }
        serde_json::Value::Array(items) => context.with(
            prefix,
            ExprValue::List(items.iter().map(|item| scalar(item, 0)).collect()),
        ),
        other => context.with(prefix, scalar(other, 0)),
    }
}

/// A scalar value; an object (or a nested list) reads as `null`.
fn scalar(value: &serde_json::Value, depth: u32) -> ExprValue {
    match value {
        serde_json::Value::Null => ExprValue::Null,
        serde_json::Value::Bool(b) => ExprValue::Bool(*b),
        serde_json::Value::Number(n) => number(n),
        serde_json::Value::String(s) => ExprValue::Text(s.clone()),
        serde_json::Value::Array(items) => {
            if depth > MAX_FILTER_DEPTH {
                ExprValue::Null
            } else {
                ExprValue::List(items.iter().map(|item| scalar(item, depth + 1)).collect())
            }
        }
        serde_json::Value::Object(_) => ExprValue::Null,
    }
}

/// A JSON number as an integer or an exact decimal (never a float, ADR-12).
fn number(n: &serde_json::Number) -> ExprValue {
    if let Some(i) = n.as_i64() {
        return ExprValue::Int(i);
    }
    if let Some(u) = n.as_u64()
        && let Ok(i) = i64::try_from(u)
    {
        return ExprValue::Int(i);
    }
    match rust_decimal_from_str(&n.to_string()) {
        Some(d) => ExprValue::Decimal(d),
        None => ExprValue::Text(n.to_string()),
    }
}

/// Parses a decimal from its text without pulling `rust_decimal` in directly.
fn rust_decimal_from_str(text: &str) -> Option<spot_app_api::prelude::Decimal> {
    use std::str::FromStr;
    spot_app_api::prelude::Decimal::from_str(text).ok()
}
