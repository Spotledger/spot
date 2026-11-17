//! Process distribution on the running hub and edge (WP-SYN-12.T2, ADR-062, ADR-067). Operator
//! guide: `docs/running-spot/sync_process.md`.
//!
//! `spot-sync-process` (WP-SYN-12.T1) decides; this module hosts it:
//!
//! - **Hub**: [`ProcessSync::publish`] emits a published process version as a hub-authored master
//!   record with its scope keys, through the hub stream port ([`HubStream`], provided by the hub's
//!   sync hosting). A republish is a no-op; a changed definition under one version is refused; a
//!   `process:*` record pushed by a node is refused ([`ProcessSync::refuse_from_node`]) and audited.
//! - **Edge**: [`ProcessSync::receive`] runs the intake: install **beside** the versions the node
//!   has (a running Case keeps its pinned version), verify the hash and the tenant, **hold** a
//!   record the engine cannot run ([`SCHEMA_HOLD`](HOLD_CONDITION) is raised for it and cleared when
//!   an upgrade releases it), retire a version only when no Case is pinned to it.
//!   [`ProcessSync::start_version`] is what the BPM runtime asks for a new Case.
//!
//! # Persistence
//!
//! Installed definitions are in table `spot_sync_process_definition` ([`DDL`]); the intake's
//! decisions are synchronous (T1's traits), so [`QueuedStore`] is a memory view loaded at start
//! whose writes are queued and **flushed before the call returns**: [`ProcessSync::receive`] does
//! not answer `Installed` until the row is durable. The table must exist before the host serves
//! (`spot migrate`); the start-up check degrades the service otherwise.
//!
//! # What it needs
//!
//! Ports from other services: the hub stream ([`HubStream`], hub side), the BPM runtime's pin
//! counts ([`PinSource`]; without it nothing is ever retired: every version counts as pinned) and
//! the conditions handle. Without a hub stream the hub side is off; the edge side needs none.
//!
//! Settings `services.sync_process`: `enabled`, `engine_version` (this node's engine version,
//! default the build's), `accepts_breaking`. Level optional: a node whose process distribution
//! cannot run keeps trading on the versions it has.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Deserialize;
use spot_conditions::{ConditionParams, Millis};
use spot_core::Clock;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{Catalog, Isolation, Name, StorageBackend, Transaction, TxOptions};
use spot_sync_process::{
    Capability, CompatClass, DefinitionStore, Error as ProcessError, HubSink, PinCounter,
    ProcessIntake, ProcessPublisher, ProcessRecord, Published, Received, Result as ProcessResult,
};

use crate::services::conditions::ConditionsHandle;
use crate::services::{Idle, Level, SiteCtx};

/// Start order: after the sync services and the conditions service it reports through.
pub const ORDER: u32 = 1240;

/// The condition raised for a held definition (the catalogue's `SCHEMA_HOLD`).
pub const HOLD_CONDITION: &str = "SCHEMA_HOLD";

/// The table of installed process definitions. Created by `spot migrate` (the plan registration is
/// `schema.rs`'s: see the task report); the service never creates it.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_sync_process_definition (
    process             text NOT NULL,
    version             bigint NOT NULL CHECK (version >= 1),
    tenant              text NOT NULL,
    definition_hash     text NOT NULL,
    min_engine_version  bigint NOT NULL,
    compat_class        text NOT NULL CHECK (compat_class IN ('compatible', 'breaking')),
    payload             text NOT NULL,
    PRIMARY KEY (process, version)
);
"#;

/// `[services.sync_process]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` leaves process distribution off.
    pub enabled: bool,
    /// This node's engine version. A record whose `min_engine_version` is higher is held.
    pub engine_version: u64,
    /// Whether new Cases may start on a `breaking` version.
    pub accepts_breaking: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            engine_version: 1,
            accepts_breaking: false,
        }
    }
}

/// Where the hub emits master records (the hub stream of the sync hosting).
pub trait HubStream: Send + Sync {
    /// Emits `record` requiring all of `scope_keys` (empty: every subscriber of process
    /// definitions); a node whose subscription does not cover them never receives it.
    ///
    /// # Errors
    /// A sentence without the definition.
    fn emit(&self, record: &ProcessRecord, scope_keys: &[String]) -> Result<(), String>;
}

/// How many running Cases are pinned to a version (the BPM runtime provides it).
pub trait PinSource: Send + Sync {
    /// The Cases pinned to `(process, version)`.
    fn pins(&self, process: &str, version: u64) -> u64;
}

/// Without a pin source nothing may be retired: every version counts as pinned.
struct EveryVersionPinned;

impl PinSource for EveryVersionPinned {
    fn pins(&self, _process: &str, _version: u64) -> u64 {
        1
    }
}

struct Pins(Arc<dyn PinSource>);

impl PinCounter for Pins {
    fn pins(&self, process: &str, version: u64) -> u64 {
        self.0.pins(process, version)
    }
}

/// One queued change of the persisted set.
#[derive(Clone, Debug)]
enum Op {
    Put(ProcessRecord),
    Remove(String, u64),
}

#[derive(Default)]
struct Queue(Mutex<Vec<Op>>);

impl Queue {
    fn lock(&self) -> MutexGuard<'_, Vec<Op>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The intake's [`DefinitionStore`]: a memory view whose writes are queued for the durable table.
pub struct QueuedStore {
    records: BTreeMap<(String, u64), ProcessRecord>,
    queue: Arc<Queue>,
}

impl DefinitionStore for QueuedStore {
    fn hash_of(&self, process: &str, version: u64) -> Option<String> {
        self.records
            .get(&(process.to_owned(), version))
            .map(|r| r.definition_hash.clone())
    }
    fn get(&self, process: &str, version: u64) -> Option<ProcessRecord> {
        self.records.get(&(process.to_owned(), version)).cloned()
    }
    fn install(&mut self, record: &ProcessRecord) -> ProcessResult<()> {
        self.records
            .insert((record.process.clone(), record.version), record.clone());
        self.queue.lock().push(Op::Put(record.clone()));
        Ok(())
    }
    fn remove(&mut self, process: &str, version: u64) -> ProcessResult<()> {
        self.records.remove(&(process.to_owned(), version));
        self.queue
            .lock()
            .push(Op::Remove(process.to_owned(), version));
        Ok(())
    }
    fn versions(&self, process: &str) -> Vec<u64> {
        self.records
            .keys()
            .filter(|(p, _)| p == process)
            .map(|(_, v)| *v)
            .collect()
    }
}

/// Where the installed set is made durable.
pub trait Persist: Send + Sync {
    /// Every stored record (read at start).
    fn load<'a>(&'a self) -> BoxFut<'a, Result<Vec<ProcessRecord>, String>>;
    /// Writes the queued changes, in order, atomically.
    fn apply<'a>(&'a self, ops: &'a [PersistOp]) -> BoxFut<'a, Result<(), String>>;
    /// Whether the table exists.
    fn check<'a>(&'a self) -> BoxFut<'a, Result<(), String>>;
}

/// A boxed future.
pub type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// One durable change.
#[derive(Clone, Debug)]
pub enum PersistOp {
    /// Insert (or keep) a record.
    Put(ProcessRecord),
    /// Remove `(process, version)`.
    Remove(String, u64),
}

/// The PostgreSQL [`Persist`] over the site's pool.
pub struct PgDefinitions {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl PgDefinitions {
    /// A store over `backend`'s pool.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self { backend, catalog }
    }

    async fn begin(&self, label: &str, read_only: bool) -> Result<PgTransaction, String> {
        let name = Name::new(label).map_err(|_| "label".to_owned())?;
        let mut options = TxOptions::new(name, Isolation::ReadCommitted);
        if read_only {
            options = options.read_only();
        }
        self.backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|_| "the database is unavailable".to_owned())
    }
}

impl Persist for PgDefinitions {
    fn load<'a>(&'a self) -> BoxFut<'a, Result<Vec<ProcessRecord>, String>> {
        Box::pin(async move {
            let mut tx = self.begin("sync_process_load", true).await?;
            let rows = tx
                .raw_query(
                    "SELECT tenant, process, version, definition_hash, min_engine_version, \
                            compat_class, payload \
                     FROM spot_sync_process_definition ORDER BY process, version",
                    &[],
                    None,
                )
                .await
                .map_err(|_| "the definitions could not be read".to_owned());
            let _ = tx.rollback().await;
            rows?
                .iter()
                .map(|row| {
                    let version: i64 = row.try_get(2).map_err(|_| "decode".to_owned())?;
                    let min: i64 = row.try_get(4).map_err(|_| "decode".to_owned())?;
                    let class: String = row.try_get(5).map_err(|_| "decode".to_owned())?;
                    Ok(ProcessRecord {
                        tenant: row.try_get(0).map_err(|_| "decode".to_owned())?,
                        process: row.try_get(1).map_err(|_| "decode".to_owned())?,
                        version: u64::try_from(version).map_err(|_| "decode".to_owned())?,
                        definition_hash: row.try_get(3).map_err(|_| "decode".to_owned())?,
                        min_engine_version: u64::try_from(min).map_err(|_| "decode".to_owned())?,
                        compat_class: if class == "breaking" {
                            CompatClass::Breaking
                        } else {
                            CompatClass::Compatible
                        },
                        payload: row.try_get(6).map_err(|_| "decode".to_owned())?,
                    })
                })
                .collect()
        })
    }

    fn apply<'a>(&'a self, ops: &'a [PersistOp]) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            let mut tx = self.begin("sync_process_apply", false).await?;
            for op in ops {
                match op {
                    PersistOp::Put(record) => {
                        let version =
                            i64::try_from(record.version).map_err(|_| "range".to_owned())?;
                        let min = i64::try_from(record.min_engine_version)
                            .map_err(|_| "range".to_owned())?;
                        let class = match record.compat_class {
                            CompatClass::Compatible => "compatible",
                            CompatClass::Breaking => "breaking",
                        };
                        tx.raw_execute_with(
                            PgTransaction::write(1),
                            "INSERT INTO spot_sync_process_definition \
                               (process, version, tenant, definition_hash, min_engine_version, \
                                compat_class, payload) \
                             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (process, version) DO NOTHING",
                            &[
                                &record.process,
                                &version,
                                &record.tenant,
                                &record.definition_hash,
                                &min,
                                &class,
                                &record.payload,
                            ],
                        )
                        .await
                        .map_err(|_| "the definition could not be stored".to_owned())?;
                    }
                    PersistOp::Remove(process, version) => {
                        let version = i64::try_from(*version).map_err(|_| "range".to_owned())?;
                        tx.raw_execute_with(
                            PgTransaction::write(1),
                            "DELETE FROM spot_sync_process_definition WHERE process = $1 AND version = $2",
                            &[process, &version],
                        )
                        .await
                        .map_err(|_| "the definition could not be removed".to_owned())?;
                    }
                }
            }
            tx.commit()
                .await
                .map_err(|_| "the changes could not be committed".to_owned())
        })
    }

    fn check<'a>(&'a self) -> BoxFut<'a, Result<(), String>> {
        Box::pin(async move {
            let mut tx = self.begin("sync_process_check", true).await?;
            let rows = tx
                .raw_query(
                    "SELECT to_regclass('spot_sync_process_definition') IS NOT NULL",
                    &[],
                    None,
                )
                .await
                .map_err(|_| "the database cannot be asked".to_owned());
            let _ = tx.rollback().await;
            let present: bool = rows?
                .first()
                .and_then(|r| r.try_get(0).ok())
                .unwrap_or(false);
            if present {
                Ok(())
            } else {
                Err(
                    "the table spot_sync_process_definition is missing: run `spot migrate`"
                        .to_owned(),
                )
            }
        })
    }
}

/// The hub's [`HubSink`] over the hub stream port.
struct StreamSink(Arc<dyn HubStream>);

impl HubSink for StreamSink {
    fn emit(&mut self, record: &ProcessRecord, scope_keys: &[String]) -> ProcessResult<()> {
        self.0
            .emit(record, scope_keys)
            .map_err(|message| ProcessError::new(store_failed_code(), message))
    }
}

/// A process distribution event for the audit (`sys_sync_decision`-style): names and codes only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DistributionEvent {
    /// `installed`, `held`, `released`, `retired`, `refused` or `published`.
    pub action: &'static str,
    /// The record name `process:<name>@<version>`.
    pub record: String,
    /// The refusal's code, for `refused`.
    pub code: Option<&'static str>,
}

/// Where distribution events go (the sync decision audit of the host).
pub trait EventSink: Send + Sync {
    /// Records one event.
    fn record(&self, event: &DistributionEvent);
}

struct LogEvents;

impl EventSink for LogEvents {
    fn record(&self, event: &DistributionEvent) {
        tracing::info!(
            target: "spot::audit::sync_process",
            action = event.action,
            record = %event.record,
            code = event.code.unwrap_or(""),
            "process distribution"
        );
    }
}

/// The hosted process distribution of one site.
pub struct ProcessSync {
    node: String,
    tenant: String,
    intake: Mutex<ProcessIntake<QueuedStore, Pins>>,
    queue: Arc<Queue>,
    publisher: Option<Mutex<ProcessPublisher<StreamSink>>>,
    persist: Arc<dyn Persist>,
    conditions: Option<Arc<ConditionsHandle>>,
    events: Arc<dyn EventSink>,
    clock: Arc<dyn Clock>,
}

/// What building the service needs.
pub struct ProcessSyncParts {
    /// This node's name (the `node` parameter of the held condition).
    pub node: String,
    /// The site's tenant.
    pub tenant: String,
    /// The engine capability.
    pub capability: Capability,
    /// The durable table.
    pub persist: Arc<dyn Persist>,
    /// The BPM runtime's pin counts (`None`: nothing is ever retired).
    pub pins: Option<Arc<dyn PinSource>>,
    /// The hub stream (`None`: the hub side is off).
    pub hub: Option<Arc<dyn HubStream>>,
    /// The conditions handle (`None`: holds are not raised).
    pub conditions: Option<Arc<ConditionsHandle>>,
    /// Distribution events.
    pub events: Option<Arc<dyn EventSink>>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl ProcessSync {
    /// Builds the service and loads the installed set.
    ///
    /// # Errors
    /// A sentence when the installed set cannot be read.
    pub async fn start(parts: ProcessSyncParts) -> Result<Self, String> {
        let queue = Arc::new(Queue::default());
        let mut records = BTreeMap::new();
        for record in parts.persist.load().await? {
            records.insert((record.process.clone(), record.version), record);
        }
        let store = QueuedStore {
            records,
            queue: Arc::clone(&queue),
        };
        let pins: Arc<dyn PinSource> = parts.pins.unwrap_or_else(|| Arc::new(EveryVersionPinned));
        let intake = ProcessIntake::new(store, Pins(pins), parts.tenant.clone(), parts.capability);
        let publisher = parts
            .hub
            .map(|hub| Mutex::new(ProcessPublisher::new(StreamSink(hub), parts.tenant.clone())));
        Ok(Self {
            node: parts.node,
            tenant: parts.tenant,
            intake: Mutex::new(intake),
            queue,
            publisher,
            persist: parts.persist,
            conditions: parts.conditions,
            events: parts.events.unwrap_or_else(|| Arc::new(LogEvents)),
            clock: parts.clock,
        })
    }

    fn now(&self) -> Millis {
        let millis = self.clock.now_utc().unix_timestamp_nanos() / 1_000_000;
        Millis::new(u64::try_from(millis).unwrap_or(0))
    }

    /// Makes the queued changes durable. An error leaves the queue for the next flush; the caller
    /// then reports failure, never success.
    async fn flush(&self) -> Result<(), String> {
        let ops: Vec<PersistOp> = lock_queue(&self.queue)
            .iter()
            .map(|op| match op {
                Op::Put(record) => PersistOp::Put(record.clone()),
                Op::Remove(process, version) => PersistOp::Remove(process.clone(), *version),
            })
            .collect();
        if ops.is_empty() {
            return Ok(());
        }
        self.persist.apply(&ops).await?;
        let mut queue = lock_queue(&self.queue);
        let done = ops.len().min(queue.len());
        queue.drain(..done);
        Ok(())
    }

    async fn hold(&self, record: &ProcessRecord, raised: bool) {
        let Some(handle) = &self.conditions else {
            return;
        };
        let subject = record.name();
        let at = self.now();
        let _ = if raised {
            let mut params = ConditionParams::new();
            params.set("node", &self.node);
            params.set("record", &subject);
            params.set("schema_major", record.min_engine_version.to_string());
            handle
                .raise(HOLD_CONDITION, &subject, params, at)
                .await
                .map(|_| ())
        } else {
            handle
                .observe_cleared(HOLD_CONDITION, &subject, at)
                .await
                .map(|_| ())
        };
    }

    /// Hub: publishes a version with its scope keys.
    ///
    /// # Errors
    /// The library's error (a foreign record, a changed definition under one version), or
    /// `sync_process.store_failed` when the hub side is off.
    pub fn publish(
        &self,
        record: &ProcessRecord,
        scope_keys: &[String],
    ) -> ProcessResult<Published> {
        let Some(publisher) = &self.publisher else {
            return Err(ProcessError::new(
                store_failed_code(),
                "the hub stream is not provided on this site",
            ));
        };
        let result = lock(publisher).publish(record, scope_keys);
        self.events.record(&match &result {
            Ok(_) => DistributionEvent {
                action: "published",
                record: record.name(),
                code: None,
            },
            Err(error) => DistributionEvent {
                action: "refused",
                record: record.name(),
                code: Some(error.code),
            },
        });
        result
    }

    /// Hub: refuses a `process:*` record a node pushed (process definitions are hub-authored).
    ///
    /// # Errors
    /// `sync_process.edge_authored`.
    pub fn refuse_from_node(&self, record_name: &str) -> ProcessResult<()> {
        let result = ProcessPublisher::<StreamSink>::refuse_from_node(record_name);
        if let Err(error) = &result {
            self.events.record(&DistributionEvent {
                action: "refused",
                record: record_name.chars().take(128).collect(),
                code: Some(error.code),
            });
        }
        result
    }

    /// Edge: receives a record from the hub. Answers only after the change is durable.
    ///
    /// # Errors
    /// The library's refusal (foreign tenant, hash mismatch, invalid), or
    /// `sync_process.store_failed` when the table cannot be written (the in-memory view is rolled
    /// back to the persisted set at the next start).
    pub async fn receive(&self, record: &ProcessRecord) -> ProcessResult<Received> {
        let received = lock(&self.intake).receive(record);
        match &received {
            Ok(Received::Installed) => {
                self.flush()
                    .await
                    .map_err(|m| ProcessError::new(store_failed_code(), m))?;
                self.events.record(&DistributionEvent {
                    action: "installed",
                    record: record.name(),
                    code: None,
                });
            }
            Ok(Received::Held) => {
                self.hold(record, true).await;
                self.events.record(&DistributionEvent {
                    action: "held",
                    record: record.name(),
                    code: None,
                });
            }
            // A retry of a record whose first flush failed finds it installed in memory: the
            // queued change still has to reach the store, or the retry would acknowledge a record
            // that is not durable.
            Ok(Received::NoOp) => {
                self.flush()
                    .await
                    .map_err(|m| ProcessError::new(store_failed_code(), m))?;
            }
            Err(error) => self.events.record(&DistributionEvent {
                action: "refused",
                record: record.name(),
                code: Some(error.code),
            }),
        }
        received
    }

    /// Edge: the engine was upgraded; releases every held record it can now run.
    ///
    /// # Errors
    /// `sync_process.store_failed`.
    pub async fn on_upgrade(&self, engine_version: u64) -> ProcessResult<usize> {
        let released = lock(&self.intake).on_upgrade(engine_version)?;
        self.flush()
            .await
            .map_err(|m| ProcessError::new(store_failed_code(), m))?;
        for installed in &released {
            let record = ProcessRecord {
                tenant: self.tenant.clone(),
                process: installed.process.clone(),
                version: installed.version,
                definition_hash: String::new(),
                min_engine_version: 0,
                compat_class: CompatClass::Compatible,
                payload: String::new(),
            };
            self.hold(&record, false).await;
            self.events.record(&DistributionEvent {
                action: "released",
                record: record.name(),
                code: None,
            });
        }
        Ok(released.len())
    }

    /// Edge: retires `(process, version)`: removed now when no Case is pinned to it, otherwise
    /// kept (and removed by a later [`ProcessSync::sweep`] once the last Case ends).
    ///
    /// # Errors
    /// `sync_process.store_failed`.
    pub async fn retire(&self, process: &str, version: u64) -> ProcessResult<bool> {
        let removed = lock(&self.intake).retire(process, version)?;
        self.flush()
            .await
            .map_err(|m| ProcessError::new(store_failed_code(), m))?;
        if removed {
            self.events.record(&DistributionEvent {
                action: "retired",
                record: spot_sync_process::record_name(process, version),
                code: None,
            });
        }
        Ok(removed)
    }

    /// Edge: removes retired versions no Case is pinned to.
    ///
    /// # Errors
    /// `sync_process.store_failed`.
    pub async fn sweep(&self) -> ProcessResult<usize> {
        let removed = lock(&self.intake).sweep()?;
        self.flush()
            .await
            .map_err(|m| ProcessError::new(store_failed_code(), m))?;
        for installed in &removed {
            self.events.record(&DistributionEvent {
                action: "retired",
                record: spot_sync_process::record_name(&installed.process, installed.version),
                code: None,
            });
        }
        Ok(removed.len())
    }

    /// The version a new Case of `process` starts on (the BPM runtime asks).
    #[must_use]
    pub fn start_version(&self, process: &str) -> Option<u64> {
        lock(&self.intake).start_version(process)
    }

    /// The versions held (visible in the sync health), as `process@version`.
    #[must_use]
    pub fn held(&self) -> Vec<String> {
        lock(&self.intake)
            .held()
            .iter()
            .map(|h| format!("{}@{}", h.process, h.version))
            .collect()
    }

    /// Records quarantined for a hash mismatch since start.
    #[must_use]
    pub fn quarantined(&self) -> u64 {
        lock(&self.intake).quarantined()
    }
}

/// The catalogue code `sync_process.store_failed` (the library lists its codes in `CODES`).
fn store_failed_code() -> &'static str {
    spot_sync_process::CODES
        .iter()
        .copied()
        .find(|code| code.ends_with("store_failed"))
        .unwrap_or("sync_process.store_failed")
}

fn lock_queue(queue: &Queue) -> MutexGuard<'_, Vec<Op>> {
    queue.lock()
}

/// Registers process distribution when the node can persist definitions.
pub async fn register(ctx: &mut SiteCtx<'_>) -> crate::Result<()> {
    let settings: Settings = ctx.settings_or_default("sync_process")?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    if ctx.port::<ProcessSync>().is_some() || !ctx.assembling() {
        return Ok(());
    }
    let persist: Arc<dyn Persist> = Arc::new(PgDefinitions::new(
        env.backend.clone(),
        Arc::clone(&env.permission.catalog),
    ));
    let parts = ProcessSyncParts {
        node: ctx.label().to_owned(),
        tenant: ctx.label().to_owned(),
        capability: Capability {
            engine_version: settings.engine_version,
            accepts_breaking: settings.accepts_breaking,
        },
        persist: Arc::clone(&persist),
        pins: ctx.port::<dyn PinSource>(),
        hub: ctx.port::<dyn HubStream>(),
        conditions: ctx.port::<ConditionsHandle>(),
        events: ctx.port::<dyn EventSink>(),
        clock: Arc::clone(ctx.clock()),
    };
    // The installed set is read once, at registration.
    let service = match ProcessSync::start(parts).await {
        Ok(service) => Arc::new(service),
        Err(reason) => {
            tracing::warn!(reason = %reason, "sync_process: the installed definitions could not be read; process distribution is off");
            return Ok(());
        }
    };
    let check_persist = Arc::clone(&persist);
    ctx.add(Idle::new("sync_process", Level::Optional, move || {
        let persist = Arc::clone(&check_persist);
        Box::pin(async move { persist.check().await })
    }));
    ctx.provide::<ProcessSync>(service)?;
    Ok(())
}
