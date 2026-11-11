//! The feed: [`FeedPump`] turns committed change-feed and outbox entries into room events.
//!
//! One pump serves one site (ADR-34: one tenant = one site = one database, and the bus is
//! already site-scoped). The pump drives two logs — the change feed and the outbox —
//! each from its own durable position, read and written through an injected
//! [`PositionStore`]. A restart continues where the last successful publish-and-save
//! stopped; a crash between the bus publishes and the position save re-reads the same
//! entries and republishes them with the **same event ids** (at-least-once; clients
//! deduplicate). Nothing is published before commit by construction: both logs hold
//! committed entries only.
//!
//! # What is mapped to what
//!
//! **Change feed** (`read_changes`, each entry is one document's net change in one
//! committed transaction) → kind `doc_created` / `doc_updated` / `doc_deleted` for
//! `Insert` / `Update` / `Delete`, published to **both** `doc:<doctype>:<id>` and
//! `doctype:<doctype>`, payload `{event_id, doctype, id, version}` — the contract's
//! `{doctype, id, version}` for document events, plus the deduplication id it rides on.
//! The `event_id` is derived deterministically from the entry's feed position, so a
//! redelivered entry carries the same id. No field values, ever (ADR-25).
//!
//! **Outbox** (`read_outbox`) → the topic distinctions the change feed cannot express,
//! and the user notifications:
//!
//! - `doc_posted`, `doc_cancelled`, `doc_renamed`, `doc_updated_posted` map onto the
//!   contract's kinds (`doc_updated_posted` → `doc_updated`) with payload
//!   `{event_id, doctype, id, version, changed: [field names]}`. `changed` carries the
//!   changed **field names** for updates (the payload document's `changed_fields`, which
//!   the schema keeps a list of names) and `["name"]` for a rename — the **name of what
//!   changed**, never a value: a rename's `old_name`/`new_name` are values and are never
//!   sent. The lifecycle topics `doc_created` and `doc_updated` are **skipped**: the
//!   change-feed entry of the same commit already carries that fact, and both events
//!   describing one (document, version) would make clients refetch twice.
//! - any `identity_*` topic whose payload parses becomes a `notification` event in the
//!   `user:<subject>` room: the **subject** is the user the identity event is about
//!   (the schemas put the subject in the payload's `user` field, as a record id). The
//!   payload carries `{event_id, event: <topic>}` and nothing else. A payload that does
//!   not parse, or names no user, is logged and skipped — never half-sent.
//! - [`TOPIC_NOTICE`] (`sys_user_notice`, what `HookContext::notify_user` writes) becomes a
//!   `notification` event in the `user:<user>` room with payload
//!   `{event_id, event: "user_notice", data: <the notice payload>}`. The addressee is the
//!   record id in the **committed event** (the stored `data.user`), never anything the
//!   notice payload says: a payload field named `user` cannot redirect delivery. The room's
//!   authorisation is the room authoriser's, so this crate adds no room and no rule. The
//!   notice payload is app-supplied JSON and passes the boundary rule below; an entry that
//!   breaks it is refused (`rtbridge.notice_rejected`), counted, and never sent.
//! - every other topic is skipped: app topics have no contract room yet.
//!
//! # Notice content is the caller's responsibility
//!
//! Every other event this pump publishes is built from identifiers the storage wrote; a
//! notice is the one event whose `data` an **app** supplies. The only guard the pump can
//! offer is the shape rule of `notice_shape_ok` — an object of scalars, record ids and
//! arrays of scalars/ids, at most `NOTICE_MAX_BYTES` serialised, no string over
//! `NOTICE_MAX_TEXT_BYTES` — which is what keeps the channel to ids and short scalars
//! (ADR-25), not what makes a value safe. It cannot tell a document number from a secret,
//! so **the caller of `notify_user` is responsible for the notice's content**: send
//! identifiers and short scalars, never a field value the recipient may not read.
//!
//! # Gaps and resync
//!
//! A stored position the feed refuses (`storage.resync_required` — retired, another
//! stream generation, or past the end) means the pump can vouch for nothing in the range
//! it missed. It publishes one `resync` event ([`RESYNC_KIND`]) to every room the log has
//! fed since its last resync (doctype rooms — which reach every document subscriber —
//! and the notification rooms), then resets that log's stored position to
//! [`FeedPosition::START`] so later runs redeliver the retained range with unchanged
//! event ids. The marker payload is `{event_id, reason: rtbridge.position_lost}`; the
//! contract's wire event kinds do not include `resync`, so the native transport drops it
//! — clients learn the refetch from the protocol's own signals (a `seq` gap, or a
//! `resync_required` join outcome). See the report.
//!
//! # Bounded memory
//!
//! The pump keeps only room **names** fed since each log's last resync (deduplicated by
//! doctype and user id), never payload content; a resync frees the set. Names are capped
//! per log at [`PumpConfig::max_fed_names`] by dropping the oldest — a cap reached
//! without a resync only narrows the set of rooms a later resync would reach (a signal
//! some clients miss is acceptable; a wrong or unbounded one is not).
//!
//! # The pump never awaits a subscriber
//!
//! Publishing goes through [`RealtimeBus::publish`] — synchronous, never awaiting — and
//! `pumpOnce` awaits only the feed reads and the position store. A stalled subscriber is
//! closed by the bus (`Closed::SlowConsumer`, wire 4008); the proof lives in the crate's
//! `backpressure` test.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use spot_core::SiteId;
use spot_events_contract::lifecycle::Operation;
use spot_realtime::{RealtimeBus, Room};
use spot_storage_contract::{
    Change, ChangeFeed, ChangeOp, ErrorKind, FeedPosition, OutboxRecord, RecordId,
};

use crate::error::{BridgeError, CODE_NOTICE_REJECTED, CODE_POSITION_LOST};

/// The kind of the event the pump publishes to rooms it can no longer vouch for.
pub const RESYNC_KIND: &str = "resync";

/// The outbox topic of a realtime notice to one user, as the document engine writes it
/// (`HookContext::notify_user`). The engine owns the literal
/// (`spot_document::engine::outbox::TOPIC_NOTICE`, whose stored `data` is
/// `{user, payload}`); this crate repeats it rather than depending on the engine, and the
/// `notice` suite — the one place `spot-document` is a dev-dependency — asserts the two
/// agree and that the engine's own reader parses what this pump maps.
pub const TOPIC_NOTICE: &str = "sys_user_notice";

/// The `event` name a notice carries inside its `notification` event: the notice's own
/// vocabulary, distinct from the `identity_*` topics that share the kind.
pub const NOTICE_EVENT: &str = "user_notice";

/// The largest notice payload accepted, in bytes of its compact JSON serialisation
/// (ADR-25: the realtime channel is a doorbell, not a data path).
pub const NOTICE_MAX_BYTES: usize = 2 * 1024;

/// The longest string (key or value) a notice payload may hold; a notice points at a
/// document, it does not quote one.
pub const NOTICE_MAX_TEXT_BYTES: usize = 256;

/// The outbox topics mapped onto room events. `doc_created`/`doc_updated` are not here:
/// the change-feed entry of the same commit already carries those facts.
fn outbox_lifecycle_topics() -> [&'static str; 4] {
    [
        Operation::Post.topic_name(),
        Operation::Cancel.topic_name(),
        Operation::Rename.topic_name(),
        Operation::UpdatePosted.topic_name(),
    ]
}

/// The event kinds the two mappings produce, all within the contract's pinned vocabulary
/// except `RESYNC_KIND`.
pub(crate) fn doc_kind_for_change(op: ChangeOp) -> &'static str {
    match op {
        ChangeOp::Insert => "doc_created",
        ChangeOp::Update => "doc_updated",
        ChangeOp::Delete => "doc_deleted",
    }
}

/// The wire kind for an outbox lifecycle topic, or `None` when the topic is skipped.
pub(crate) fn doc_kind_for_topic(topic: &str) -> Option<&'static str> {
    let topics = outbox_lifecycle_topics();
    if topic == topics[0] {
        return Some("doc_posted");
    }
    if topic == topics[1] {
        return Some("doc_cancelled");
    }
    if topic == topics[2] {
        return Some("doc_renamed");
    }
    if topic == topics[3] {
        return Some("doc_updated");
    }
    None
}

/// The deterministic event id of a change-feed entry: `cf:<generation>:<seq>`. Positions
/// strictly increase and a re-read after a crash returns the same positions, so every
/// redelivery carries the same id.
pub(crate) fn change_event_id(position: FeedPosition) -> String {
    format!("cf:{}:{}", position.generation(), position.seq())
}

/// The room names one log fed since its last resync: doctype names and user ids only.
/// Cloned between runs into the saved [`PumpPosition`], bounded by
/// [`PumpConfig::max_fed_names`], freed by a resync.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FedRooms {
    /// The doctypes the log's events named (each implying its `doctype:` room).
    pub doctypes: BTreeSet<String>,
    /// The users the log's notifications named (each implying its `user:` room).
    pub users: BTreeSet<String>,
}

impl FedRooms {
    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.doctypes.is_empty() && self.users.is_empty()
    }

    /// The `doctype:` rooms implied, oldest-remembered last; capped by `max`.
    fn doctype_rooms(&self, max: usize) -> Vec<String> {
        cap_front(self.doctypes.iter().cloned().collect(), max)
    }

    /// The `user:` rooms implied, oldest-remembered last; capped by `max`.
    fn user_rooms(&self, max: usize) -> Vec<String> {
        cap_front(self.users.iter().cloned().collect(), max)
    }
}

/// The first `max` names of `names` (the remembered order); the tail beyond it is the
/// part that would be dropped on overflow.
fn cap_front(mut names: Vec<String>, max: usize) -> Vec<String> {
    if names.len() > max {
        names.truncate(max);
    }
    names
}

/// The stored feed state: one position per log plus the rooms each fed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpPosition {
    /// Position in the change feed.
    pub changes: FeedPosition,
    /// Position in the outbox feed.
    pub outbox: FeedPosition,
    /// The rooms the change feed has fed since its last resync.
    pub fed_changes: FedRooms,
    /// The rooms the outbox has fed since its last resync.
    pub fed_outbox: FedRooms,
}

impl Default for PumpPosition {
    fn default() -> Self {
        Self {
            changes: FeedPosition::START,
            outbox: FeedPosition::START,
            fed_changes: FedRooms::default(),
            fed_outbox: FedRooms::default(),
        }
    }
}

/// The durable home of a [`PumpPosition`]. Implementations must save the pair atomically
/// enough that a save leaves either both logs' positions or neither lost-as-read — the
/// pump saves the read positions once per publishing run, and any lost save only causes a
/// redelivery with the same event ids.
pub trait PositionStore: Send + Sync {
    /// The last saved position, or `None` before the first save. Boxed so the trait
    /// stays object-safe.
    fn load(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<PumpPosition>, String>> + Send + '_>>;
    /// Makes `position` the durable position. Boxed so the trait stays object-safe.
    fn save<'a>(
        &'a self,
        position: &'a PumpPosition,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// What the pump's lag metric is measured against: the head of a log, when the backend
/// can answer it. `None` ("unknown") simply leaves the metric unrecorded for that run —
/// the pump never guesses a head.
pub trait FeedHead: Send + Sync {
    /// The current head of the change feed, when the backend knows it. Boxed so the trait
    /// stays object-safe.
    fn head_changes(&self) -> Pin<Box<dyn Future<Output = Option<FeedPosition>> + Send + '_>>;
    /// The current head of the outbox feed, when the backend knows it. Boxed so the trait
    /// stays object-safe.
    fn head_outbox(&self) -> Pin<Box<dyn Future<Output = Option<FeedPosition>> + Send + '_>>;
}

/// The observability seam: the host injects a handle that forwards into its metrics
/// (`spot-telemetry`). The pump records through it and nowhere else; this crate holds no
/// global state.
pub trait PumpTelemetry: Send + Sync {
    /// One run's mapping outcome, for counters.
    fn record_run(&self, outcome: &MappingOutcome);
    /// The pump's lag on one log: how many published entries lie at or after the stored
    /// position and at or before the head. Recorded only when the head is known.
    fn record_lag(&self, log: &'static str, entries_behind: u64);
    /// A resync triggered on one log: its stored position was abandoned and reset.
    fn record_resync(&self, log: &'static str);
}

/// The mapping outcome of one `pump_once`, for the telemetry handle and assertions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MappingOutcome {
    /// Change-feed entries mapped to room events.
    pub changes_published: u64,
    /// Outbox events mapped to room events (skipped topics not counted).
    pub outbox_published: u64,
    /// Notice entries the boundary refused: not sent, logged once each, consumed.
    pub outbox_notices_rejected: u64,
    /// Room events published in total (a document event reaches two rooms).
    pub room_events: u64,
    /// The change feed's stored position was reset: redelivery from the start follows.
    pub changes_resynced: bool,
    /// The outbox's stored position was reset: redelivery from the start follows.
    pub outbox_resynced: bool,
}

/// One `pump_once` result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PumpOutcome {
    /// What was mapped.
    pub mapping: MappingOutcome,
    /// The position as saved after the run (or as read, when nothing published).
    pub position: PumpPosition,
}

/// Limits of one pump.
#[derive(Clone, Debug)]
pub struct PumpConfig {
    /// How many entries one `read_changes`/`read_outbox` page fetches; one page per log
    /// per run.
    pub page_limit: u32,
    /// Room names remembered per log between resyncs; the oldest beyond the cap are
    /// dropped (a later resync may then not reach them — narrowing a refetch signal is
    /// acceptable, growing without bound is not).
    pub max_fed_names: usize,
}

impl Default for PumpConfig {
    fn default() -> Self {
        Self {
            page_limit: 500,
            max_fed_names: 10_000,
        }
    }
}

/// What one outbox record became, and so whether the stored outbox position moves past it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mapped {
    /// Published to at least one room.
    Published,
    /// Nothing published, and nothing a re-read could change: the entry is consumed and the
    /// position advances past it, so a refusal is logged once and not on every run.
    Consumed,
    /// Nothing published, but the entry is left for the next run (an app topic this pump
    /// does not map, or an identity payload that may parse once the writer is fixed).
    Held,
}

/// The feed: reads one site's change feed and outbox, publishes room events on the bus,
/// advances two durable positions.
///
/// Generic over the feed because
/// [`ChangeFeed`](spot_storage_contract::ChangeFeed) is not dyn-compatible (its methods
/// return `impl Future`); the host wires its concrete backend, the tests theirs. Every
/// injected seam ([`PositionStore`], [`FeedHead`], [`PumpTelemetry`]) is object-safe, so
/// the runtime knobs stay `Arc<dyn _>`.
pub struct FeedPump<F: ChangeFeed, T: PumpTelemetry + 'static> {
    site: SiteId,
    feed: Arc<F>,
    bus: RealtimeBus,
    store: Arc<dyn PositionStore>,
    head: Option<Arc<dyn FeedHead>>,
    telemetry: Arc<T>,
    config: PumpConfig,
}

impl<F: ChangeFeed, T: PumpTelemetry + 'static> FeedPump<F, T> {
    /// A pump for `site` over `feed`, publishing to `bus`, storing positions in `store`,
    /// probing lag through `head` (`None`: that metric is simply unrecorded) and
    /// reporting through `telemetry`.
    pub fn new(
        site: SiteId,
        feed: Arc<F>,
        bus: RealtimeBus,
        store: Arc<dyn PositionStore>,
        head: Option<Arc<dyn FeedHead>>,
        telemetry: Arc<T>,
        config: PumpConfig,
    ) -> Self {
        Self {
            site,
            feed,
            bus,
            store,
            head,
            telemetry,
            config,
        }
    }

    /// The site the pump serves.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// One pump run: read both logs from the stored positions, map, publish, advance.
    ///
    /// # Errors
    ///
    /// [`BridgeError::PositionStore`] when the position cannot be read (nothing is
    /// published that run) or saved (the publishes already happened; the next run
    /// redelivers with the same event ids); [`BridgeError::Feed`] when a log fails with
    /// anything other than an unusable position — a `storage.resync_required` is handled
    /// inside (resync markers, position reset, later redelivery) and never returned.
    pub async fn pump_once(&self) -> Result<PumpOutcome, BridgeError> {
        let stored = match self.store.load().await {
            Ok(Some(position)) => position,
            Ok(None) => PumpPosition::default(),
            Err(message) => {
                return Err(BridgeError::PositionStore {
                    while_doing: "load",
                    message,
                });
            }
        };

        let mut outcome = MappingOutcome::default();
        let mut fed_changes = stored.fed_changes.clone();
        let mut fed_outbox = stored.fed_outbox.clone();
        let mut next_changes = stored.changes;
        let mut next_outbox = stored.outbox;

        // --- the change feed ---------------------------------------------------------
        match self
            .feed
            .read_changes(stored.changes, self.config.page_limit)
            .await
        {
            Ok(changes) => {
                for change in &changes {
                    self.map_change(change);
                    fed_changes
                        .doctypes
                        .insert(change.document.entity.as_str().to_owned());
                    next_changes = change.position;
                    outcome.changes_published += 1;
                }
            }
            Err(error) if error.kind == ErrorKind::ResyncRequired => {
                tracing::warn!(site = %self.site, "change feed position lost; resyncing rooms");
                self.telemetry.record_resync("changes");
                outcome.changes_resynced = true;
                self.publish_resync(&fed_changes, &FedRooms::default());
                next_changes = FeedPosition::START;
                fed_changes = FedRooms::default();
            }
            Err(error) => {
                return Err(BridgeError::Feed {
                    feed: "changes",
                    error,
                });
            }
        }

        // --- the outbox ---------------------------------------------------------------
        match self
            .feed
            .read_outbox(stored.outbox, self.config.page_limit)
            .await
        {
            Ok(records) => {
                for record in &records {
                    // A consumed entry moves the position too: its outcome cannot change on a
                    // re-read (a notice the boundary refused), so it must not be re-read and
                    // re-logged on every run. A held entry — an unmapped app topic, an
                    // identity payload that may yet parse — leaves the position where it is.
                    match self.map_outbox(record, &mut fed_outbox) {
                        Mapped::Published => {
                            next_outbox = record.position;
                            outcome.outbox_published += 1;
                        }
                        Mapped::Consumed => {
                            next_outbox = record.position;
                            outcome.outbox_notices_rejected += 1;
                        }
                        Mapped::Held => {}
                    }
                }
            }
            Err(error) if error.kind == ErrorKind::ResyncRequired => {
                tracing::warn!(site = %self.site, "outbox position lost; resyncing rooms");
                self.telemetry.record_resync("outbox");
                outcome.outbox_resynced = true;
                self.publish_resync(&FedRooms::default(), &fed_outbox);
                next_outbox = FeedPosition::START;
                fed_outbox = FedRooms::default();
            }
            Err(error) => {
                return Err(BridgeError::Feed {
                    feed: "outbox",
                    error,
                });
            }
        }

        // --- advance ------------------------------------------------------------------
        let position = PumpPosition {
            changes: next_changes,
            outbox: next_outbox,
            fed_changes,
            fed_outbox,
        };
        if (outcome.changes_published > 0
            || outcome.outbox_published > 0
            || outcome.outbox_notices_rejected > 0
            || outcome.changes_resynced
            || outcome.outbox_resynced)
            && let Err(message) = self.store.save(&position).await
        {
            tracing::error!(site = %self.site, message, "position store save failed");
            return Err(BridgeError::PositionStore {
                while_doing: "save",
                message,
            });
        }

        self.observe_lag(&position).await;
        outcome.room_events = outcome.changes_published * 2 + outcome.outbox_published;
        self.telemetry.record_run(&outcome);
        Ok(PumpOutcome {
            mapping: outcome,
            position,
        })
    }

    /// Publishes one change-feed entry to its two rooms.
    fn map_change(&self, change: &Change) {
        let doctype = change.document.entity.as_str();
        let id = change.document.id.to_string();
        let kind = doc_kind_for_change(change.op);
        let payload = serde_json::json!({
            "event_id": change_event_id(change.position),
            "doctype": doctype,
            "id": id,
            "version": change.version.0,
        });
        self.publish_pair(doctype, &id, kind, payload);
    }

    /// Publishes one outbox record to its rooms and says whether the position advances.
    fn map_outbox(&self, record: &OutboxRecord, rooms: &mut FedRooms) -> Mapped {
        let topic = record.event.topic.as_str();
        if let Some(kind) = doc_kind_for_topic(topic) {
            let doctype = record.event.subject.entity.as_str();
            let id = record.event.subject.id.to_string();
            let version = record.event.subject_version.map_or(0, |v| v.0);
            let payload = serde_json::json!({
                "event_id": outbox_event_id(record),
                "doctype": doctype,
                "id": id,
                "version": version,
                "changed": changed_names(record),
            });
            self.publish_pair(doctype, &id, kind, payload);
            rooms.doctypes.insert(doctype.to_owned());
            return Mapped::Published;
        }
        if topic.starts_with("identity_") {
            return if self.map_notification(record, topic, rooms) {
                Mapped::Published
            } else {
                Mapped::Held
            };
        }
        if topic == TOPIC_NOTICE {
            return self.map_notice(record, rooms);
        }
        // Every other topic is skipped: no contract room for app topics yet.
        Mapped::Held
    }

    /// Publishes a notice into the room of the user the **committed event** names.
    ///
    /// The whole entry is checked before anything is published (never half-sent): the
    /// payload must parse, name a record id in `user`, and carry a `payload` object inside
    /// the boundary rule of [`notice_shape_ok`]. Anything else is refused, counted, and
    /// logged with the topic and the position only — never the payload, which is app data
    /// this pump has no business writing to a log.
    fn map_notice(&self, record: &OutboxRecord, rooms: &mut FedRooms) -> Mapped {
        let topic = record.event.topic.as_str();
        let Ok(doc) =
            serde_json::from_str::<spot_events_contract::PayloadDocument>(&record.event.payload)
        else {
            self.refuse_notice(topic, record.position);
            return Mapped::Consumed;
        };
        let Some(user) = doc
            .data
            .get("user")
            .and_then(json_str)
            .and_then(record_id_text)
        else {
            self.refuse_notice(topic, record.position);
            return Mapped::Consumed;
        };
        let Some(body) = doc.data.get("payload") else {
            self.refuse_notice(topic, record.position);
            return Mapped::Consumed;
        };
        if !notice_shape_ok(body) {
            self.refuse_notice(topic, record.position);
            return Mapped::Consumed;
        }
        let Ok(room) = Room::user(&user) else {
            self.refuse_notice(topic, record.position);
            return Mapped::Consumed;
        };
        let payload = serde_json::json!({
            "event_id": outbox_event_id(record),
            "event": NOTICE_EVENT,
            "data": body,
        });
        self.bus.publish(&self.site, &room, "notification", payload);
        rooms.users.insert(user);
        Mapped::Published
    }

    /// One warn line per refused notice: its topic and position, the code and nothing of the
    /// payload.
    fn refuse_notice(&self, topic: &str, position: FeedPosition) {
        tracing::warn!(
            site = %self.site,
            topic,
            generation = position.generation(),
            seq = position.seq(),
            code = CODE_NOTICE_REJECTED,
            "notice refused by the realtime boundary; skipped"
        );
    }

    /// Publishes a notification for an identity event into the subject user's room.
    fn map_notification(&self, record: &OutboxRecord, topic: &str, rooms: &mut FedRooms) -> bool {
        let doc: spot_events_contract::PayloadDocument = match serde_json::from_str(
            &record.event.payload,
        ) {
            Ok(doc) => doc,
            Err(error) => {
                tracing::warn!(topic, error = %error, "identity payload does not parse; skipped");
                return false;
            }
        };
        let Some(user) = doc.data.get("user").and_then(json_str) else {
            tracing::warn!(topic, "identity payload names no user; skipped");
            return false;
        };
        let Ok(room) = Room::user(user) else {
            tracing::warn!(topic, user, "identity user id is not a room name; skipped");
            return false;
        };
        let payload = serde_json::json!({
            "event_id": outbox_event_id(record),
            "event": topic,
        });
        self.bus.publish(&self.site, &room, "notification", payload);
        rooms.users.insert(user.to_owned());
        true
    }

    /// Publishes one document event to both of its rooms.
    fn publish_pair(&self, doctype: &str, id: &str, kind: &str, payload: serde_json::Value) {
        let doc_room = Room::document(doctype, id);
        let doctype_room = Room::doctype(doctype);
        if let (Ok(doc_room), Ok(doctype_room)) = (doc_room, doctype_room) {
            self.bus
                .publish(&self.site, &doc_room, kind, payload.clone());
            self.bus.publish(&self.site, &doctype_room, kind, payload);
        } else {
            // Feed data not expressible as a room (storage names make this unreachable
            // today; logged and skipped — a missing notification is acceptable, a wrong
            // or oversized one is not).
            tracing::warn!(doctype, id, kind, "change names an unusable room; skipped");
        }
    }

    /// Publishes one `resync` marker to every room `fed_changes`/`fed_outbox` imply.
    ///
    /// Only `doctype:` rooms (reaching every document subscriber) and the notification
    /// rooms: the pump remembers no per-document ids (an unbounded set); a `doc:` room's
    /// subscribers learn the refetch through the doctype room they typically also hold —
    /// and from the `seq` gap either way.
    fn publish_resync(&self, fed_changes: &FedRooms, fed_outbox: &FedRooms) {
        let mut rooms = Vec::new();
        for doctype in fed_changes.doctype_rooms(self.config.max_fed_names) {
            if let Ok(room) = Room::doctype(&doctype) {
                rooms.push(room);
            }
        }
        for doctype in fed_outbox.doctype_rooms(self.config.max_fed_names) {
            if let Ok(room) = Room::doctype(&doctype) {
                rooms.push(room);
            }
        }
        for user in fed_changes
            .user_rooms(self.config.max_fed_names)
            .into_iter()
            .chain(fed_outbox.user_rooms(self.config.max_fed_names))
        {
            if let Ok(room) = Room::user(&user) {
                rooms.push(room);
            }
        }
        rooms.sort();
        rooms.dedup();
        for room in rooms {
            // A stable marker id: every room's marker names the position-lost code; the
            // reason is vocabulary, never data.
            let payload = serde_json::json!({
                "event_id": format!("{RESYNC_KIND}:{CODE_POSITION_LOST}"),
                "reason": CODE_POSITION_LOST,
            });
            self.bus.publish(&self.site, &room, RESYNC_KIND, payload);
        }
    }

    /// Records the lag of each log, when the head probe is wired and the head is known.
    async fn observe_lag(&self, position: &PumpPosition) {
        let Some(head) = self.head.as_ref() else {
            return;
        };
        if let Some(head_pos) = head.head_changes().await {
            let behind = head_pos.seq().saturating_sub(position.changes.seq());
            self.telemetry.record_lag("changes", behind);
        }
        if let Some(head_pos) = head.head_outbox().await {
            let behind = head_pos.seq().saturating_sub(position.outbox.seq());
            self.telemetry.record_lag("outbox", behind);
        }
    }
}

/// The changed **field names** of an outbox record: the payload document's
/// `changed_fields` (strings only), and `["name"]` for a rename — the field that changed,
/// never its old or new value.
fn changed_names(record: &OutboxRecord) -> Vec<String> {
    if record.event.topic.as_str() == Operation::Rename.topic_name() {
        return vec!["name".to_owned()];
    }
    match serde_json::from_str::<spot_events_contract::PayloadDocument>(&record.event.payload) {
        Ok(doc) => doc
            .data
            .get("changed_fields")
            .and_then(|names| names.as_array())
            .map(|names| {
                names
                    .iter()
                    .filter_map(json_str)
                    .map(str::to_owned)
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// The `&str` of a JSON string value, borrowed by the caller.
fn json_str(value: &serde_json::Value) -> Option<&str> {
    value.as_str()
}

/// The event id every outbox mapping carries: the id the engine assigned the event inside
/// the committing transaction. A position names exactly one event, so a redelivery after a
/// crash between the publishes and the position save re-reads the same entry and publishes
/// the same id (at-least-once; clients deduplicate on it).
fn outbox_event_id(record: &OutboxRecord) -> String {
    record.event.id.to_string()
}

/// Whether a notice's `data.payload` is inside the ADR-25 boundary: an object of scalars,
/// record ids and arrays of scalars/ids (no object below the root, no array inside an
/// array), at most [`NOTICE_MAX_BYTES`] of compact JSON, with no string — key or value —
/// longer than [`NOTICE_MAX_TEXT_BYTES`]. Nothing but the shape is checked: the caller
/// owns the content.
fn notice_shape_ok(body: &serde_json::Value) -> bool {
    let Some(fields) = body.as_object() else {
        return false;
    };
    let Ok(encoded) = serde_json::to_vec(body) else {
        return false;
    };
    encoded.len() <= NOTICE_MAX_BYTES
        && fields
            .iter()
            .all(|(name, value)| name.len() <= NOTICE_MAX_TEXT_BYTES && notice_value_ok(value))
}

/// One top-level notice value: a scalar, or an array of scalars.
fn notice_value_ok(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(items) => items.iter().all(notice_scalar_ok),
        other => notice_scalar_ok(other),
    }
}

/// One notice scalar: null, a boolean, a number, or a string of at most
/// [`NOTICE_MAX_TEXT_BYTES`]. An object or a nested array is not a scalar.
fn notice_scalar_ok(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => true,
        serde_json::Value::String(text) => text.len() <= NOTICE_MAX_TEXT_BYTES,
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => false,
    }
}

/// The canonical text of the record id `text` names, or `None` when it is not one.
///
/// Accepts what [`RecordId`]'s `Display` writes (a hyphenated UUIDv7), and returns that
/// canonical form: the room a notice is delivered to and the name remembered for a resync
/// are one string per user, whatever spelling the event stored.
fn record_id_text(text: &str) -> Option<String> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    RecordId::from_bytes(bytes).ok().map(|id| id.to_string())
}
