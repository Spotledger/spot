//! The production seams of the catch-up feed: [`PgSource`] reads the hub's registry and the
//! tenant's outbox (through `spot-sync-hub-pg`, the same stores the relay reads), [`NodeScopeOf`]
//! is a node's scope built from the sync policy, and [`MetaLinks`] reads record links from the
//! site's metadata. Everything here is blocking: the catch-up routes call the feed on the blocking
//! pool.
//!
//! # What is read, and how much
//!
//! - the registry row of the node (`PgNodeRegistry::get`): state, subscription, and the applied
//!   prefix and acknowledged serve sequence **the hub recorded**;
//! - the stream head and generation, and the oldest published position (a one-row read);
//! - the outbox rows after a position, in one `PgHubStream::read_after` call, bounded by the
//!   caller (the capture mapper below sees every row the stream decodes, keeps the `doc_state`
//!   ones with their payload decoded once, and forwards nothing: the relay's own mapper is a
//!   different instance, so this read cannot disturb it);
//! - for a record whose scope key the gap does not show, one image read (`PgOutboxImages`).
//!
//! Nothing is written, deleted or rewritten, and no payload is put in a log line.
//!
//! # The scope mirrors the relay
//!
//! The subscription and the policy's scope keys come from the same `PolicyHandle` the relay's
//! destinations are built from, and an outbox row becomes a stream entry by the rule of
//! `sync_hub_assembly`'s mapper (only `doc_state` with a version of at least 1 and a schema major;
//! a delete is a tombstone entry), so a node is offered in a catch-up what the relay would offer it
//! live. Without a loaded policy generation nothing is in scope (fail closed).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use spot_sync_bootstrap::HubHead;
use spot_sync_closure::{ImageSource, RecordKey, Redactor, element_id};
use spot_sync_hub::{DestinationScope, EntryRole, HubStream, StreamEntry};
use spot_sync_hub_pg::{
    EntryMapper, NodeState as RegistryState, OutboxRow, PgHub, PgHubStream, PgOutboxImages,
    current_generation,
};
use spot_sync_obligations::Lane;
use spot_sync_policy_host::scope::FieldReader;
use spot_sync_protocol::NodeId;
use spot_sync_skew::{NodeReport, SoftwareVersion};

use super::feed::{
    FeedFault, FeedSource, Links, NodeFacts, NodeScope, RawEvent, TOPIC_DOC_STATE, Window,
};
use crate::services::sync_policy::PolicyHandle;

/// Everything the capture mapper saw in one read.
#[derive(Default)]
struct Captured {
    /// Rows decoded by the stream, of every topic.
    seen: usize,
    /// The position of the first row, whatever its topic.
    first: Option<u64>,
    /// The `doc_state` rows, in position order.
    events: Vec<RawEvent>,
}

/// An [`EntryMapper`] that keeps what it sees and forwards nothing.
struct Capture(Arc<Mutex<Captured>>);

impl EntryMapper for Capture {
    fn map(&self, row: &OutboxRow) -> Option<StreamEntry> {
        let mut captured = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        captured.seen += 1;
        if captured.first.is_none() {
            captured.first = Some(row.position);
        }
        if row.topic == TOPIC_DOC_STATE {
            captured.events.push(RawEvent {
                position: row.position,
                txn: row.txn.clone(),
                ordinal: row.ordinal,
                topic: row.topic.clone(),
                entity: row.entity.clone(),
                id: row.id.clone(),
                version: row.version,
                schema_version: row.schema_version,
                // Decoded once; a payload that does not decode is `Null` and the gap is then not
                // planned (never partly).
                body: serde_json::from_str(&row.payload).unwrap_or(Value::Null),
            });
        }
        None
    }
}

/// The hub's stores as the feed's source.
pub struct PgSource {
    hub: PgHub,
    tenant: String,
    policy: Arc<PolicyHandle>,
    report: NodeReport,
}

impl PgSource {
    /// A source for `tenant` over `hub`, building scopes from `policy`. `report` is the hub's own
    /// release report (a destination needs one; the scope never reads it).
    #[must_use]
    pub fn new(hub: PgHub, tenant: &str, policy: Arc<PolicyHandle>, report: NodeReport) -> Self {
        Self {
            hub,
            tenant: tenant.to_owned(),
            policy,
            report,
        }
    }

    /// The active registry row of `node` with a readable subscription, or `None`.
    fn active_row(&self, node: &str) -> Result<Option<spot_sync_hub_pg::NodeRow>, FeedFault> {
        let Ok(id) = NodeId::new(node) else {
            return Ok(None);
        };
        let row = self.hub.nodes().get(&id).map_err(|_| FeedFault)?;
        Ok(row.filter(|row| row.state == RegistryState::Active && row.subscription.is_some()))
    }

    /// One bounded read of the outbox after `after`, `max` rows.
    fn read(&self, after: u64, max: usize) -> Result<Captured, FeedFault> {
        let db = self.hub.db();
        let generation = current_generation(db).map_err(|_| FeedFault)?;
        let sink = Arc::new(Mutex::new(Captured::default()));
        let stream = PgHubStream::new(db.clone(), generation, Capture(Arc::clone(&sink)));
        // The entries are ignored (the mapper forwards none): what the stream decoded is in `sink`.
        stream.read_after(after, max).map_err(|_| FeedFault)?;
        let mut guard = sink.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(std::mem::take(&mut *guard))
    }
}

impl FeedSource for PgSource {
    fn head(&self) -> Result<HubHead, FeedFault> {
        let db = self.hub.db();
        let generation = current_generation(db).map_err(|_| FeedFault)?;
        let stream = PgHubStream::new(
            db.clone(),
            generation,
            Capture(Arc::new(Mutex::new(Captured::default()))),
        );
        let head_position = stream.head().map_err(|_| FeedFault)?;
        Ok(HubHead {
            generation,
            head_position,
        })
    }

    fn oldest_position(&self) -> Result<Option<u64>, FeedFault> {
        Ok(self.read(0, 1)?.first)
    }

    fn node(&self, node: &str) -> Result<Option<NodeFacts>, FeedFault> {
        Ok(self.active_row(node)?.map(|row| NodeFacts {
            applied_prefix: row.applied_prefix,
            ever_acknowledged: row.last_ack_serve_seq > 0,
        }))
    }

    fn events_after(&self, after: u64, max_rows: usize) -> Result<Window, FeedFault> {
        // One row more than the bound: seeing it means the gap is larger than the bound.
        let captured = self.read(after, max_rows.saturating_add(1))?;
        if captured.seen > max_rows {
            return Ok(Window {
                events: Vec::new(),
                truncated: true,
            });
        }
        Ok(Window {
            events: captured.events,
            truncated: false,
        })
    }

    fn scope(
        &self,
        node: &str,
        events: &[RawEvent],
    ) -> Result<Option<Box<dyn NodeScope>>, FeedFault> {
        let Some(row) = self.active_row(node)? else {
            return Ok(None);
        };
        let Some(subscription) = row.subscription else {
            return Ok(None);
        };
        let db = self.hub.db();
        let generation = current_generation(db).map_err(|_| FeedFault)?;
        let images: Arc<dyn FieldReader> =
            Arc::new(ImageFields(PgOutboxImages::new(db.clone(), generation)));
        let reader: Arc<dyn FieldReader> = Arc::new(GapFields::new(events, images));
        let Some(factory) = self.policy.destination_factory(reader) else {
            return Ok(None);
        };
        let destination = factory.destination(&row.node_id, subscription, self.report.clone());
        Ok(Some(Box::new(NodeScopeOf {
            scope: destination.scope,
            redactor: destination.redactor,
            tenant: self.tenant.clone(),
            source: format!("{}#hub", self.tenant),
        })))
    }
}

/// The hub's own release report, for the destinations the scope is built from.
#[must_use]
pub fn node_report(epoch: u64) -> NodeReport {
    let version = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0))
        .collect::<Vec<_>>();
    let part = |at: usize| version.get(at).copied().unwrap_or(0);
    NodeReport {
        software_version: SoftwareVersion::new(part(0), part(1), part(2)),
        metadata_version: u32::try_from(epoch).unwrap_or(u32::MAX),
        migration_epoch: epoch,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: [(TOPIC_DOC_STATE.to_owned(), BTreeSet::from([1_u32]))]
            .into_iter()
            .collect(),
    }
}

/// The stream entry the relay's mapper would make of `row`, or `None` for an event the hub does
/// not forward.
#[must_use]
pub fn entry_of(tenant: &str, source: &str, row: &RawEvent) -> Option<StreamEntry> {
    if row.topic != TOPIC_DOC_STATE {
        return None;
    }
    let version = row.version.filter(|version| *version >= 1)?;
    let major = u32::try_from(row.schema_version).ok()?;
    Some(StreamEntry {
        position: row.position,
        tenant: tenant.to_owned(),
        source: source.to_owned(),
        origin: None,
        txn: row.txn.clone(),
        role: if row.ordinal == 0 {
            EntryRole::Root
        } else {
            EntryRole::Satellite
        },
        key: RecordKey {
            entity: row.entity.clone(),
            id: row.id.clone(),
        },
        version,
        topic: row.topic.clone(),
        major,
        // The lane is a function of the entity; nothing here is obligation-bearing.
        lane: Lane::Bulk,
        deleted: row.deleted(),
    })
}

/// A node's scope and redaction, from a destination the sync policy built.
struct NodeScopeOf {
    scope: Box<dyn DestinationScope>,
    redactor: Box<dyn Redactor>,
    tenant: String,
    source: String,
}

impl NodeScope for NodeScopeOf {
    fn covers(&self, event: &RawEvent) -> bool {
        entry_of(&self.tenant, &self.source, event).is_some_and(|entry| self.scope.covers(&entry))
    }

    fn redact(&self, entity: &str, image: &mut Value) {
        self.redactor.redact_down(entity, image);
    }
}

/// The text of a scope-key field of an image, as the relay's field reader reads it.
fn field_text(image: &Value, field: &str) -> Option<String> {
    match image.get(field)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Reads a scope-key field from the record's current image in the outbox (one read).
struct ImageFields(PgOutboxImages);

impl FieldReader for ImageFields {
    fn scope_value(&self, entity: &str, id: &str, field: &str) -> Option<String> {
        let key = RecordKey {
            entity: entity.to_owned(),
            id: id.to_owned(),
        };
        let image = self.0.image(&key).ok().flatten()?;
        field_text(&image.image, field)
    }
}

/// Reads scope keys from the gap's own images first (the newest image of a record in the gap, a
/// tombstone for a deleted one), and from the outbox only for a record the gap does not show.
struct GapFields {
    latest: BTreeMap<(String, String), Value>,
    fallback: Arc<dyn FieldReader>,
}

impl GapFields {
    fn new(events: &[RawEvent], fallback: Arc<dyn FieldReader>) -> Self {
        let mut latest = BTreeMap::new();
        for event in events {
            let key = (event.entity.clone(), event.id.clone());
            if event.deleted() {
                latest.insert(key, Value::Null);
            } else if let Some(image) = event.body.get("image") {
                latest.insert(key, image.clone());
            }
        }
        Self { latest, fallback }
    }
}

impl FieldReader for GapFields {
    fn scope_value(&self, entity: &str, id: &str, field: &str) -> Option<String> {
        match self.latest.get(&(entity.to_owned(), id.to_owned())) {
            Some(image) => field_text(image, field),
            None => self.fallback.scope_value(entity, id, field),
        }
    }
}

/// Record links read from the site's metadata: fixed links in the parent and in every child row,
/// and dynamic links named by the record's type field.
pub struct MetaLinks {
    meta: Arc<dyn spot_api_services::MetaSource>,
}

impl MetaLinks {
    /// Links over the site's metadata source.
    #[must_use]
    pub fn new(meta: Arc<dyn spot_api_services::MetaSource>) -> Self {
        Self { meta }
    }
}

/// The deepest child table followed.
const MAX_DEPTH: u8 = 4;

fn collect(
    meta: &spot_meta::EffectiveMeta,
    entity: &str,
    row: &Value,
    out: &mut Vec<(String, String)>,
    depth: u8,
) -> Result<(), ()> {
    use spot_meta::FieldType;
    let Some(definition) = meta.definition(entity) else {
        return Ok(());
    };
    for field in &definition.fields {
        let Some(value) = row.get(&field.name) else {
            continue;
        };
        match field.ty {
            FieldType::Link => {
                if let (Some(target), Some(id)) = (field.options.as_deref(), element_id(value)) {
                    out.push((target.to_owned(), id));
                }
            }
            FieldType::DynamicLink => {
                let target = field
                    .options
                    .as_deref()
                    .and_then(|type_field| row.get(type_field))
                    .and_then(Value::as_str);
                if let (Some(target), Some(id)) = (target, element_id(value)) {
                    if meta.definition(target).is_none() {
                        return Err(());
                    }
                    out.push((target.to_owned(), id));
                }
            }
            FieldType::Table | FieldType::TableMultiselect if depth < MAX_DEPTH => {
                if let (Some(child), Some(rows)) = (field.options.as_deref(), value.as_array()) {
                    for child_row in rows {
                        collect(meta, child, child_row, out, depth + 1)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

impl Links for MetaLinks {
    fn references(&self, entity: &str, image: &Value) -> Option<Vec<(String, String)>> {
        let current = self.meta.current().ok()?;
        let mut out = Vec::new();
        collect(&current.meta, entity, image, &mut out, 0).ok()?;
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn raw(position: u64, id: &str, body: Value) -> RawEvent {
        RawEvent {
            position,
            txn: format!("t{position}"),
            ordinal: 0,
            topic: TOPIC_DOC_STATE.to_owned(),
            entity: "note".to_owned(),
            id: id.to_owned(),
            version: Some(1),
            schema_version: 1,
            body,
        }
    }

    #[test]
    fn an_entry_follows_the_relays_mapper() {
        let row = raw(7, "a", serde_json::json!({"op": "upsert", "image": {}}));
        let entry = entry_of("acme", "acme#hub", &row).unwrap();
        assert_eq!(entry.position, 7);
        assert_eq!(entry.tenant, "acme");
        assert_eq!(entry.source, "acme#hub");
        assert_eq!(entry.role, EntryRole::Root);
        assert!(!entry.deleted);
        assert_eq!(entry.major, 1);
        // A satellite row, a delete, and the rows the relay drops.
        let satellite = RawEvent {
            ordinal: 1,
            ..row.clone()
        };
        assert_eq!(
            entry_of("acme", "acme#hub", &satellite).unwrap().role,
            EntryRole::Satellite
        );
        let gone = raw(8, "a", serde_json::json!({"op": "delete"}));
        assert!(entry_of("acme", "acme#hub", &gone).unwrap().deleted);
        let unversioned = RawEvent {
            version: None,
            ..row.clone()
        };
        assert!(entry_of("acme", "acme#hub", &unversioned).is_none());
        let zero = RawEvent {
            version: Some(0),
            ..row.clone()
        };
        assert!(entry_of("acme", "acme#hub", &zero).is_none());
        let negative = RawEvent {
            schema_version: -1,
            ..row.clone()
        };
        assert!(entry_of("acme", "acme#hub", &negative).is_none());
        let other = RawEvent {
            topic: "sys_note".to_owned(),
            ..row
        };
        assert!(entry_of("acme", "acme#hub", &other).is_none());
    }

    struct Fallback;

    impl FieldReader for Fallback {
        fn scope_value(&self, _entity: &str, _id: &str, _field: &str) -> Option<String> {
            Some("from-the-outbox".to_owned())
        }
    }

    #[test]
    fn scope_keys_come_from_the_gap_first_and_the_outbox_for_the_rest() {
        let events = vec![
            raw(
                1,
                "a",
                serde_json::json!({"op": "upsert", "image": {"branch": "north", "n": 3}}),
            ),
            raw(
                2,
                "a",
                serde_json::json!({"op": "upsert", "image": {"branch": "south"}}),
            ),
            raw(3, "gone", serde_json::json!({"op": "delete"})),
        ];
        let fields = GapFields::new(&events, Arc::new(Fallback));
        // The newest image of the record in the gap wins.
        assert_eq!(
            fields.scope_value("note", "a", "branch").as_deref(),
            Some("south")
        );
        // A deleted record reads as no value, as the tombstone image would.
        assert_eq!(fields.scope_value("note", "gone", "branch"), None);
        // A record the gap does not show asks the outbox.
        assert_eq!(
            fields.scope_value("note", "other", "branch").as_deref(),
            Some("from-the-outbox")
        );
    }

    #[test]
    fn a_field_reads_as_text_for_strings_and_numbers_only() {
        let image = serde_json::json!({"s": "x", "e": "", "n": 4, "b": true, "z": null});
        assert_eq!(field_text(&image, "s").as_deref(), Some("x"));
        assert_eq!(field_text(&image, "e"), None);
        assert_eq!(field_text(&image, "n").as_deref(), Some("4"));
        assert_eq!(field_text(&image, "b"), None);
        assert_eq!(field_text(&image, "z"), None);
        assert_eq!(field_text(&image, "missing"), None);
    }

    #[test]
    fn the_report_decodes_the_one_topic_the_hub_forwards() {
        let report = node_report(3);
        assert!(report.can_decode(TOPIC_DOC_STATE, 1));
        assert!(!report.can_decode(TOPIC_DOC_STATE, 2));
        assert_eq!(report.migration_epoch, 3);
    }
}
