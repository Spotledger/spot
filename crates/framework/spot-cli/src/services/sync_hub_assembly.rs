//! The hub's assembly: the relay core over the hub's durable stores, the entropy and id sources
//! the enrolment service takes, and the metadata epoch (WP-RTB-02.T1, T4, T8, T13; ADR-062 and
//! ADR-063).
//!
//! `sync_hub_parts` builds the directory, the enrolment endpoint and the stores, and takes three
//! ports it cannot make itself: a random source, an id source and a [`RelayAssembly`]. This
//! module (hub role only, before `sync_hub_parts`) provides them:
//!
//! * `OsRandom` (the operating system's source) and `RecordIds` (version-7 record ids) for
//!   enrolment codes and audit ids;
//! * `HubAssembly`, whose relay core is a `HubRelay` over the tenant's outbox (`PgHubStream`
//!   with a mapper that forwards the captured `doc_state` images), the durable per-destination
//!   queue (`PgSendQueue`), the closure builder's image source (`PgOutboxImages`) and the
//!   references declared in the site's metadata.
//!
//! # What reaches whom
//!
//! A destination is an **active** node of the registry with a valid subscription, built by the
//! sync policy's `DestinationFactory` from the current policy generation: the policy decides
//! what may leave the hub, the subscription decides who gets it, and the policy's redactor strips
//! `excluded_fields.down` before anything is queued. Without a loaded policy generation there is
//! no destination and nothing leaves (fail closed). A node's stored subscription that cannot be
//! read makes it receive nothing. Destinations are read from the registry on every tick, so a
//! node enrolled or activated later starts receiving without a restart; a revoked or suspended
//! node is no longer served.
//!
//! # Not here
//!
//! The push sink of each tenant (applying what an edge pushes up) stays empty until the apply
//! path is hosted on the hub: a push answers "this hub serves no such tenant". The node's report
//! at the handshake is taken to be the hub's own (same release, every major of the captured
//! topic): the version-skew hold needs the node's reported majors at registration.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use rand::TryRngCore as _;
use serde_json::Value;
use spot_sync_closure::{ClosureError, ImageSource, RecordKey, References, SourceImage};
use spot_sync_enrol::{Ids, RandomSource};
use spot_sync_hub::{EntryRole, HubRelay, RelayConfig, RelayState, SendQueue, StreamEntry};
use spot_sync_hub_host::{HubCore, TenantSinks};
use spot_sync_hub_pg::{
    EntryMapper, NodeState, OutboxRow, PgHub, PgNodeRegistry, PgOutboxImages, PgRelayStore,
    PgSendQueue, PgHubStream, QueueLimits, current_generation,
};
use spot_sync_node_host::envelope;
use spot_sync_obligations::Lane;
use spot_sync_policy_host::scope::FieldReader;
use spot_sync_protocol::Record;
use spot_sync_skew::{NodeReport, SoftwareVersion};

use crate::services::sync_hub_parts::RelayAssembly;
use crate::services::sync_policy::PolicyHandle;
use crate::{Error, Result};
use crate::services::SiteCtx;

/// The module's name.
pub const NAME: &str = "sync_hub_assembly";
/// After `sync_policy` (1100), whose handle builds the destinations, and before
/// `sync_hub_parts` (1250), which takes the ports.
pub const ORDER: u32 = 1240;
/// Hub only (the row in `node_role::SERVICE_SETS`).
pub const ROLES: &[&str] = &["hub"];

/// The topic of a captured document image.
const TOPIC_DOC_STATE: &str = "doc_state";
/// How many ticks apart the relay state is saved when nothing moved.
const SAVE_EVERY_TICKS: u64 = 20;

/// The operating system's random source.
struct OsRandom;

impl RandomSource for OsRandom {
    fn fill(&self, buffer: &mut [u8]) {
        // An enrolment code from a failed source would be guessable: refuse rather than hand it
        // out.
        if rand::rngs::OsRng.try_fill_bytes(buffer).is_err() {
            panic!("the operating system's random source failed");
        }
    }
}

/// Version-7 record ids for audit records and enrolment ids.
struct RecordIds;

impl Ids for RecordIds {
    fn uuid(&self) -> String {
        spot_storage_contract::RecordId::new_v7().to_string()
    }

    fn enrolment_id(&self) -> String {
        self.uuid().replace('-', "")[..24].to_owned()
    }
}

/// Turns a published outbox row into the hub's stream entry. Only the captured `doc_state`
/// images are forwarded: the policy and the subscription decide per destination what of them
/// leaves.
struct OutboxMapper {
    tenant: String,
    source: String,
}

impl EntryMapper for OutboxMapper {
    fn map(&self, row: &OutboxRow) -> Option<StreamEntry> {
        if row.topic != TOPIC_DOC_STATE {
            return None;
        }
        let version = row.version.filter(|v| *v >= 1)?;
        let major = u32::try_from(row.schema_version).ok()?;
        // A delete is a tombstone the hub builds itself from the key and the position.
        let deleted = row.payload.contains("\"delete\"")
            && serde_json::from_str::<Value>(&row.payload)
                .ok()
                .is_some_and(|event| event.get("op").and_then(Value::as_str) == Some("delete"));
        Some(StreamEntry {
            position: row.position,
            tenant: self.tenant.clone(),
            source: self.source.clone(),
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
            deleted,
        })
    }
}

/// The shared image source: the relay owns one handle and the scope-key reader another.
#[derive(Clone)]
struct SharedImages(Arc<PgOutboxImages>);

impl ImageSource for SharedImages {
    fn image(&self, key: &RecordKey) -> std::result::Result<Option<SourceImage>, ClosureError> {
        self.0.image(key)
    }
}

/// Reads the value of a scope-key field from the record's current image.
struct ImageFields(SharedImages);

impl FieldReader for ImageFields {
    fn scope_value(&self, entity: &str, id: &str, field: &str) -> Option<String> {
        let key = RecordKey {
            entity: entity.to_owned(),
            id: id.to_owned(),
        };
        let image = self.0.image(&key).ok().flatten()?;
        match image.image.get(field)? {
            Value::String(text) if !text.is_empty() => Some(text.clone()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    }
}

/// The references a record image declares, read from the site's current metadata: fixed links in
/// the parent and in every child row, and dynamic links named by the record's type field.
struct MetaReferences {
    meta: Arc<dyn spot_api_services::MetaSource>,
}

impl MetaReferences {
    fn collect(
        meta: &spot_meta::EffectiveMeta,
        entity: &str,
        row: &Value,
        out: &mut Vec<RecordKey>,
        depth: u8,
    ) -> std::result::Result<(), ClosureError> {
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
                    if let (Some(target), Some(id)) = (field.options.as_deref(), element_id(value))
                    {
                        out.push(RecordKey {
                            entity: target.to_owned(),
                            id,
                        });
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
                            return Err(ClosureError::ReferenceUnreadable {
                                entity: entity.to_owned(),
                                field: field.name.clone(),
                            });
                        }
                        out.push(RecordKey {
                            entity: target.to_owned(),
                            id,
                        });
                    }
                }
                FieldType::Table | FieldType::TableMultiselect if depth < 4 => {
                    if let (Some(child), Some(rows)) =
                        (field.options.as_deref(), value.as_array())
                    {
                        for child_row in rows {
                            Self::collect(meta, child, child_row, out, depth + 1)?;
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn element_id(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

impl References for MetaReferences {
    fn references(
        &self,
        entity: &str,
        image: &Value,
    ) -> std::result::Result<Vec<RecordKey>, ClosureError> {
        let current = self
            .meta
            .current()
            .map_err(|_| ClosureError::ReferenceUnreadable {
                entity: entity.to_owned(),
                field: String::new(),
            })?;
        let mut keys = Vec::new();
        Self::collect(&current.meta, entity, image, &mut keys, 0)?;
        Ok(keys)
    }
}

type Relay = HubRelay<PgSendQueue, PgHubStream<OutboxMapper>, SharedImages>;

/// The relay core the hub's driver thread ticks.
struct RelayCore {
    relay: Relay,
    registry: PgNodeRegistry,
    state: PgRelayStore,
    policy: Arc<PolicyHandle>,
    reader: Arc<dyn FieldReader>,
    report: NodeReport,
    tenant: String,
    restored: bool,
    ticks: u64,
    saved_cursor: u64,
}

fn text<E: std::fmt::Display>(error: E) -> String {
    error.to_string()
}

impl RelayCore {
    /// Registers every active node with a readable subscription that is not a destination yet,
    /// and returns the ids the hub serves now.
    fn sync_destinations(&mut self) -> std::result::Result<BTreeSet<String>, String> {
        let rows = self.registry.list().map_err(text)?;
        let factory = self.policy.destination_factory(Arc::clone(&self.reader));
        let mut serving = BTreeSet::new();
        for row in rows {
            if row.state != NodeState::Active {
                continue;
            }
            let Some(subscription) = row.subscription else {
                continue;
            };
            serving.insert(row.node_id.clone());
            if self.relay.snapshot().destinations.contains_key(&row.node_id) {
                continue;
            }
            let Some(factory) = factory.as_ref() else {
                serving.remove(&row.node_id);
                continue;
            };
            let destination =
                factory.destination(&row.node_id, subscription, self.report.clone());
            let destination = destination.with_scope_identity(format!("{}:{}", self.tenant, row.node_id));
            self.relay.add_destination(destination).map_err(text)?;
        }
        Ok(serving)
    }

    fn restore_once(&mut self) -> std::result::Result<(), String> {
        if self.restored {
            return Ok(());
        }
        if let Some(mut stored) = self.state.load().map_err(text)? {
            let known = self.relay.snapshot().destinations;
            stored.destinations.retain(|id, _| known.contains_key(id));
            self.relay.restore(stored).map_err(text)?;
        }
        self.saved_cursor = self.relay.cursor();
        self.restored = true;
        Ok(())
    }

    fn save(&mut self, state: &RelayState) -> std::result::Result<(), String> {
        self.state.save(state).map_err(text)?;
        self.saved_cursor = state.cursor;
        Ok(())
    }
}

impl HubCore for RelayCore {
    fn tick(&mut self, now_ms: i64) -> std::result::Result<(), String> {
        self.sync_destinations()?;
        self.restore_once()?;
        self.relay.tick(now_ms).map_err(text)?;
        self.ticks = self.ticks.wrapping_add(1);
        let snapshot = self.relay.snapshot();
        if snapshot.cursor != self.saved_cursor || self.ticks.is_multiple_of(SAVE_EVERY_TICKS) {
            self.save(&snapshot)?;
        }
        Ok(())
    }

    fn window(&mut self, dest: &str, max: usize) -> std::result::Result<Vec<Record>, String> {
        let items = self.relay.queue().peek(dest, max).map_err(text)?;
        let mut records = Vec::with_capacity(items.len());
        for pending in items {
            let queued = pending.item;
            records.push(
                envelope::encode(queued.serve_seq, &self.tenant, queued.serve_after, &queued.bundle)
                    .map_err(text)?,
            );
        }
        Ok(records)
    }

    fn ack_served(&mut self, dest: &str, through: u64, now_ms: i64) -> std::result::Result<(), String> {
        self.relay
            .on_ack_served_at(dest, through, now_ms)
            .map(|_| ())
            .map_err(text)
    }

    fn destinations(&self) -> Vec<String> {
        self.relay.snapshot().destinations.into_keys().collect()
    }
}

/// What `sync_hub_parts` takes: the relay core, the tenant sinks and the metadata epoch.
struct HubAssembly {
    tenant: String,
    policy: Arc<PolicyHandle>,
    meta: Arc<dyn spot_api_services::MetaSource>,
    epoch: u64,
    core_slot: Mutex<()>,
}

impl RelayAssembly for HubAssembly {
    fn metadata_epoch(&self) -> u64 {
        self.epoch
    }

    fn sinks(&self) -> Arc<TenantSinks> {
        Arc::new(TenantSinks::new())
    }

    fn core(&self, hub: &PgHub) -> std::result::Result<Box<dyn HubCore>, String> {
        let _guard = self.core_slot.lock().unwrap_or_else(PoisonError::into_inner);
        let db = hub.db().clone();
        let generation = current_generation(&db).map_err(text)?;
        let source = format!("{}#hub", self.tenant);
        let mapper = OutboxMapper {
            tenant: self.tenant.clone(),
            source: source.clone(),
        };
        let images = SharedImages(Arc::new(PgOutboxImages::new(db.clone(), generation)));
        let stream = PgHubStream::new(db, generation, mapper);
        let relay = HubRelay::new(
            RelayConfig::new(self.tenant.clone(), source),
            stream,
            hub.queue(QueueLimits::default()),
            images.clone(),
            Box::new(MetaReferences {
                meta: Arc::clone(&self.meta),
            }),
        )
        .map_err(text)?;
        let version = env!("CARGO_PKG_VERSION")
            .split('.')
            .map(|part| part.parse::<u32>().unwrap_or(0))
            .collect::<Vec<_>>();
        let part = |at: usize| version.get(at).copied().unwrap_or(0);
        let report = NodeReport {
            software_version: SoftwareVersion::new(part(0), part(1), part(2)),
            metadata_version: u32::try_from(self.epoch).unwrap_or(u32::MAX),
            migration_epoch: self.epoch,
            epoch_verified: true,
            mid_migration: false,
            decodable_majors: [(TOPIC_DOC_STATE.to_owned(), BTreeSet::from([1_u32]))]
                .into_iter()
                .collect(),
        };
        Ok(Box::new(RelayCore {
            relay,
            registry: hub.nodes(),
            state: hub.relay_state(),
            policy: Arc::clone(&self.policy),
            reader: Arc::new(ImageFields(images)),
            report,
            tenant: self.tenant.clone(),
            restored: false,
            ticks: 0,
            saved_cursor: 0,
        }))
    }
}

/// Provides the random source, the id source and the relay assembly.
///
/// # Errors
///
/// A port provided twice, or no sync policy to build the destinations from.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let policy = ctx.port::<PolicyHandle>().ok_or_else(|| {
        Error(format!(
            "{NAME}: the sync policy is not provided: the hub has nothing to route by"
        ))
    })?;
    let env = *ctx.env()?;
    let meta = env.live.meta_source();
    let epoch = meta.current().map_or(0, |current| current.version.unwrap_or(0));
    ctx.provide::<dyn RandomSource>(Arc::new(OsRandom))?;
    ctx.provide::<dyn Ids>(Arc::new(RecordIds))?;
    ctx.provide::<dyn RelayAssembly>(Arc::new(HubAssembly {
        tenant: env.site.site.clone(),
        policy,
        meta,
        epoch,
        core_slot: Mutex::new(()),
    }))?;
    Ok(())
}
