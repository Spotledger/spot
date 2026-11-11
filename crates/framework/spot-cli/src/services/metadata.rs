//! Live metadata: one [`LiveMeta`] per site, a version store the nodes share, and the
//! additive schema plan that must land before a new snapshot serves.
//!
//! # What "live" means here
//!
//! `spot-document`'s engine takes its metadata when it is constructed, so the host's swap
//! point is the site runtime: a new version is *built and validated* by [`LiveMeta`] first,
//! the additive schema plan is applied to the database, and only then does the host rebuild
//! the site (its engine, router and permission snapshot) and swap it in — a request that
//! began on version N finishes on N. This is the documented seam gap with the engine; the
//! alternative (an engine that reads a snapshot per request) is not in this crate's paths.
//!
//! # Who talks to whom
//!
//! - The bus is [`PollingBus`] on the database in the `cluster` profile — every node polls
//!   the version store, and a change written by one node is seen by the others within one
//!   poll — and an in-memory broadcast ([`MemoryVersionStore`]) otherwise. The selection is
//!   `services.metadata.bus`, which `profile = "cluster"` pins to `polling`.
//! - A runtime change (a custom field) is applied to the local snapshot, appended to the
//!   site's `metadata.json` (the definitions a restarted node rebuilds from), and then
//!   published: the publish is what makes it live on the other nodes.
//! - The metadata table the polling store needs is an additive change to the site's
//!   catalogue, planned and applied with `spot-pg-migrate` before the store is used.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use spot_core::{Clock, SiteId};
use spot_meta::{EffectiveMeta, FieldSpec, MetaError, Registry};
use spot_meta_live::{
    Definitions, Invalidation, InvalidationBus, LiveMeta, LoadFuture, MemoryVersionStore,
    MetaChange, MetaHash, MetaLoader, PollingBus, Snapshot, StorageVersionStore, Version,
    VersionStore,
};
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, EntityDef};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service};
use crate::{Error, Result, apps, config, error, schema};

/// One runtime change to a site's metadata, as it is written to `metadata.json`.
///
/// Only the custom-field change is recorded today; the other `MetaChange` kinds need the
/// definitions service (WP-MET-01.T2) to be replayable, and are listed as an open question.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case", deny_unknown_fields)]
pub enum Change {
    /// Add a `custom_`-prefixed field to a DocType.
    AddCustomField {
        /// The DocType that gains the field.
        doctype: String,
        /// The field's definition.
        spec: Box<FieldSpec>,
        /// Place it right after this field; at the end when `None`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        insert_after: Option<String>,
    },
}

impl Change {
    /// The name the change names, for messages.
    pub fn doctype(&self) -> &str {
        match self {
            Change::AddCustomField { doctype, .. } => doctype,
        }
    }

    /// The `spot-meta-live` change this applies.
    pub fn meta_change(&self) -> MetaChange {
        match self {
            Change::AddCustomField {
                doctype,
                spec,
                insert_after,
            } => MetaChange::AddCustomField {
                doctype: doctype.clone(),
                spec: spec.clone(),
                insert_after: insert_after.clone(),
            },
        }
    }
}

/// The recorded runtime changes of one site, with the version they produced.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The metadata version this file describes.
    #[serde(default)]
    pub version: u64,
    /// The changes, oldest first.
    #[serde(default)]
    pub changes: Vec<Change>,
}

/// The path of a site's runtime metadata changes.
pub fn record_path(root: &Path, site: &str) -> PathBuf {
    root.join("sites")
        .join(site)
        .join(concat!("metadata", ".json"))
}

/// Reads the site's recorded changes. A missing file is an empty record.
pub fn load_record(root: &Path, site: &str) -> Result<Record> {
    let path = record_path(root, site);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Record::default()),
        Err(e) => Err(error(format!("cannot read {}: {e}", path.display()))),
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| Error(format!("{}: invalid metadata record: {e}", path.display()))),
    }
}

fn write_record(root: &Path, site: &str, record: &Record) -> Result<()> {
    let path = record_path(root, site);
    let text = serde_json::to_string_pretty(record)
        .map_err(|e| Error(format!("the metadata record cannot be written: {e}")))?;
    let temporary = path.with_extension(concat!("json", ".tmp"));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

/// The definitions of `site`'s installed apps and its recorded runtime changes.
pub fn source(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
) -> Result<(Definitions, Record)> {
    let definitions = apps::installed_definitions(root, loaded, site)?;
    let record = load_record(root, &site.site)?;
    Ok((definitions, record))
}

/// The effective metadata a site serves: its apps' definitions with every recorded runtime
/// change applied, built and validated by `spot-meta`.
pub fn effective(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
) -> Result<Arc<EffectiveMeta>> {
    let (definitions, record) = source(root, loaded, site)?;
    let mut registry = Registry::new(definitions).map_err(|errors| Error(render(&errors)))?;
    for change in &record.changes {
        apply(&mut registry, change)?;
    }
    Ok(registry.effective())
}

fn apply(registry: &mut Registry, change: &Change) -> Result<()> {
    let result = match change {
        Change::AddCustomField {
            doctype,
            spec,
            insert_after,
        } => registry.add_custom_field(doctype, (**spec).clone(), insert_after.as_deref()),
    };
    result.map(|_| ()).map_err(|errors| Error(render(&errors)))
}

fn render(errors: &[MetaError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The loader the follower reads definitions back with: the site's files and recorded
/// changes are the source of truth, so a version is only ever a claim about them.
struct FileLoader {
    root: PathBuf,
    loaded: config::Loaded,
    site: config::Site,
}

impl MetaLoader for FileLoader {
    fn load<'a>(&'a self, _version: Version) -> LoadFuture<'a, Definitions> {
        Box::pin(async move {
            let (definitions, _) = source(&self.root, &self.loaded, &self.site).map_err(|e| {
                spot_meta_live::LiveError::Store {
                    message: e.to_string(),
                }
            })?;
            Ok(definitions)
        })
    }
}

/// A definition sink that keeps nothing: followers rebuild from the shared records.
struct NoDefinitions;

impl spot_customise::DefinitionSink for NoDefinitions {
    fn put_definitions(&self, _version: Version, _definitions: Vec<spot_meta::Definition>) {}
}

/// The metadata table's entity, merged into the site's catalogue.
fn merged_catalog(catalog: &Catalog) -> Result<Catalog> {
    let mut entities: Vec<EntityDef> = catalog.entities().cloned().collect();
    let reserved = StorageVersionStore::<PgBackend>::entity_def().map_err(error)?;
    if !entities.iter().any(|e| e.name == reserved.name) {
        entities.push(reserved);
    }
    Catalog::new(catalog.version + 1, catalog.collation.clone(), entities).map_err(error)
}

/// The polling store's version store over the site's database.
enum Store {
    Memory(Arc<MemoryVersionStore>),
    Pg(Arc<StorageVersionStore<PgBackend>>),
}

impl Store {
    fn as_version_store(&self) -> Arc<dyn VersionStore> {
        match self {
            Store::Memory(store) => store.clone(),
            Store::Pg(store) => store.clone(),
        }
    }
}

/// How the site's metadata service was built.
pub struct Settings {
    /// The configuration root (the directory holding `common.toml` and `sites/`).
    pub root: PathBuf,
    /// The validated configuration.
    pub loaded: config::Loaded,
    /// The site.
    pub site: config::Site,
    /// The site's pooled database, when the profile needs it.
    pub backend: Option<PgBackend>,
    /// The site's own storage catalogue (the metadata table is merged into it).
    pub catalog: Option<Catalog>,
    /// Run after a snapshot was adopted and its schema plan applied: the host rebuilds the
    /// site runtime.
    pub notify: Arc<dyn Fn() + Send + Sync>,
    /// The clock the poll schedule reads.
    pub clock: Arc<dyn Clock>,
}

/// One site's live metadata service.
pub struct LiveMetadata {
    site: SiteId,
    settings: Settings,
    live: Arc<LiveMeta>,
    bus: Arc<dyn InvalidationBus>,
    store: Store,
    /// The last version this node reconciled with, so an idle poll costs one read.
    seen: Mutex<Version>,
    /// How many of the site's recorded changes this node's snapshot carries, so a node that
    /// catches up replays the ones another node recorded and not the ones it already has.
    applied: Mutex<usize>,
    schedule: Duration,
    /// The start-up schema problem, reported by `check`.
    schema_problem: Mutex<Option<String>>,
    /// Set when runtime customisation follows the version store itself (it rebuilds from the
    /// shared records, which this service's file-based catch-up cannot see).
    deferred: AtomicBool,
}

impl LiveMetadata {
    /// Builds the service: definitions, snapshot, store and follower.
    pub async fn new(settings: Settings) -> Result<Arc<Self>> {
        let site = SiteId::new(&settings.site.site).map_err(error)?;
        let (definitions, record) = source(&settings.root, &settings.loaded, &settings.site)?;
        // Every node starts from version zero and replays the record, so a node that
        // restarted and a node that applied the same changes live carry the same version
        // (starting `at_version(record.version)` and then replaying would count the recorded
        // changes twice).
        let live =
            LiveMeta::new(site.clone(), definitions.clone()).map_err(|e| error(e.to_string()))?;
        for change in &record.changes {
            live.apply(change.meta_change())
                .map_err(|e| error(e.to_string()))?;
        }
        let live = Arc::new(live);
        let services = &settings.loaded.common.services;
        let (store, schema_problem) = match services.metadata.bus.as_str() {
            "polling" => {
                let backend = settings.backend.clone().ok_or_else(|| {
                    Error(format!(
                        "site `{}`: the polling metadata bus needs a database",
                        settings.site.site
                    ))
                })?;
                let catalog = settings.catalog.clone().ok_or_else(|| {
                    Error(format!(
                        "site `{}`: the polling metadata bus needs the site catalogue",
                        settings.site.site
                    ))
                })?;
                let merged = merged_catalog(&catalog)?;
                let problem = apply_schema(&backend, &merged).await.err();
                let store = Arc::new(
                    StorageVersionStore::new(Arc::new(backend), Arc::new(merged)).map_err(error)?,
                );
                let store = Store::Pg(store);
                if problem.is_none() {
                    // The first node publishes the head it boots with, so a second node
                    // starting on an empty store learns the version rather than guessing.
                    let head = store
                        .as_version_store()
                        .read()
                        .await
                        .map_err(|e| error(e.to_string()))?;
                    if head.is_none() {
                        let snapshot = live.current();
                        store
                            .as_version_store()
                            .write(snapshot.version(), snapshot.hash().clone())
                            .await
                            .map_err(|e| error(e.to_string()))?;
                    }
                }
                (store, problem)
            }
            _ => {
                let memory = Arc::new(MemoryVersionStore::new());
                let snapshot = live.current();
                memory.publish(snapshot.version(), snapshot.hash().clone(), definitions);
                (Store::Memory(memory), None)
            }
        };
        let bus: Arc<dyn InvalidationBus> = Arc::new(PollingBus::new(store.as_version_store()));
        let loader = Arc::new(FileLoader {
            root: settings.root.clone(),
            loaded: settings.loaded.clone(),
            site: settings.site.clone(),
        });
        let schedule = Duration::from_millis(services.metadata.poll_ms);
        let seen = Mutex::new(live.current().version());
        let applied = Mutex::new(record.changes.len());
        let _ = loader;
        Ok(Arc::new(Self {
            site,
            settings,
            live,
            bus,
            store,
            seen,
            applied,
            schedule,
            schema_problem: Mutex::new(schema_problem.map(|e| e.to_string())),
            deferred: AtomicBool::new(false),
        }))
    }

    /// The live snapshot handle.
    pub fn live(&self) -> &Arc<LiveMeta> {
        &self.live
    }

    /// The current snapshot.
    pub fn current(&self) -> Arc<Snapshot> {
        self.live.current()
    }

    /// The site identity.
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The bus the site's metadata changes are announced on.
    pub fn bus(&self) -> Arc<dyn InvalidationBus> {
        Arc::clone(&self.bus)
    }

    /// The version store the nodes of the site share.
    pub fn versions(&self) -> Arc<dyn VersionStore> {
        self.store.as_version_store()
    }

    /// Where the effective definitions of each published version are kept for followers: the
    /// in-process store keeps them; the database store records only `(version, hash)`, and a
    /// follower of a customised site rebuilds from the shared customisation records instead.
    pub fn definition_sink(&self) -> Arc<dyn spot_customise::DefinitionSink> {
        match &self.store {
            Store::Memory(store) => store.clone(),
            Store::Pg(_) => Arc::new(NoDefinitions),
        }
    }

    /// The definitions of the site's installed apps: what runtime customisation builds on.
    pub fn base_definitions(&self) -> Result<Definitions> {
        let (definitions, _) = source(
            &self.settings.root,
            &self.settings.loaded,
            &self.settings.site,
        )?;
        Ok(definitions)
    }

    /// The database-side schema step runtime customisation applies catalogues with: the
    /// catalogue is merged with the version store's own table so the plan never proposes to
    /// drop it.
    pub fn merge_catalog(&self, catalog: &Catalog) -> Result<Catalog> {
        match &self.store {
            Store::Pg(_) => merged_catalog(catalog),
            Store::Memory(_) => Ok(catalog.clone()),
        }
    }

    /// From now on the version store is followed by runtime customisation, not by this
    /// service's own file-based catch-up.
    pub fn defer_following(&self) {
        self.deferred.store(true, Ordering::SeqCst);
    }

    /// Applies `change` to this node's snapshot, records it for a restart, applies the
    /// additive schema plan and publishes the invalidation other nodes poll for.
    ///
    /// Order matters and is the brief's: build and validate, plan and apply the schema, then
    /// publish. A change that does not build changes nothing.
    pub async fn apply_change(&self, change: Change) -> Result<Version> {
        let version = self
            .live
            .apply(change.meta_change())
            .map_err(|e| error(e.to_string()))?;
        let snapshot = self.live.current();
        // Record after the build succeeded: a refused change is never replayed at start-up.
        let mut record = load_record(&self.settings.root, self.site.as_str())?;
        record.changes.push(change);
        record.version = version.get();
        write_record(&self.settings.root, self.site.as_str(), &record)?;
        *self.applied.lock().unwrap_or_else(|p| p.into_inner()) = record.changes.len();
        self.plan_and_apply(&snapshot).await?;
        if let Store::Memory(store) = &self.store {
            let (definitions, _) = source(
                &self.settings.root,
                &self.settings.loaded,
                &self.settings.site,
            )?;
            store.publish(version, snapshot.hash().clone(), definitions);
        }
        self.bus
            .publish(
                &self.site,
                Invalidation::meta(version, snapshot.hash().clone()),
            )
            .await
            .map_err(|e| error(e.to_string()))?;
        (self.settings.notify)();
        Ok(version)
    }

    /// The additive schema plan between the live database and the snapshot's catalogue.
    async fn plan_and_apply(&self, snapshot: &Snapshot) -> Result<()> {
        let Some(backend) = &self.settings.backend else {
            return Ok(());
        };
        let catalog = snapshot
            .meta()
            .catalog(spot_storage_contract::Collation::new("und").map_err(error)?)
            .map_err(error)?;
        let merged = merged_catalog(&catalog)?;
        apply_schema(backend, &merged).await
    }

    /// One adoption: the schema plan, then the host's rebuild.
    async fn adopt(&self, version: Version) {
        let snapshot = self.live.current();
        tracing::info!(site = %self.site, version = version.get(), "metadata snapshot adopted");
        if let Err(problem) = self.plan_and_apply(&snapshot).await {
            // The snapshot serves; a schema step that could not run is reported by `check`
            // at the next start and logged now, never guessed around.
            tracing::error!(site = %self.site, %problem, "metadata schema plan failed");
        }
        (self.settings.notify)();
    }
}

impl LiveMetadata {
    /// Brings this node to the store's announced `version`, or reports that it is already
    /// there. The definitions are re-read from the site's files, which are the source of
    /// truth: a version is only a claim about them.
    ///
    /// The snapshot is rebuilt one version at a time because `LiveMeta` mints its versions
    /// strictly and never jumps; a node that is many versions behind rebuilds each one, which
    /// is the safe direction (every build is validated) and is bounded by how often the site
    /// changed while this node was away.
    async fn catch_up(&self, version: Version, hash: &MetaHash) -> Result<bool> {
        let current = self.live.current();
        if version < current.version() {
            return Ok(false);
        }
        if version == current.version() {
            // The same version with different content is divergence: refuse to guess.
            if current.hash() != hash {
                return Err(Error(format!(
                    "site `{}`: metadata version {} has different content on another node ({} here, {} announced)",
                    self.site,
                    version.get(),
                    current.hash().as_hex(),
                    hash.as_hex()
                )));
            }
            return Ok(false);
        }
        let behind = version.get() - current.version().get();
        if behind > 1_000 {
            return Err(Error(format!(
                "site `{}`: metadata is {behind} versions behind the store; restart the node to resynchronise",
                self.site
            )));
        }
        // Loop on the version, not on `behind`: the first change on a fresh snapshot jumps
        // from version 0 straight to 2 (the served number must rise on the first change), so
        // counting `behind` steps would overshoot by one and never match the store's version.
        let mut steps = 0;
        while self.live.current().version() < version && steps < behind {
            steps += 1;
            let (definitions, record) = source(
                &self.settings.root,
                &self.settings.loaded,
                &self.settings.site,
            )?;
            // One version is one change: a change another node recorded is replayed from the
            // site's record, and any other step is the standard definitions refresh.
            let next = {
                let applied = self.applied.lock().unwrap_or_else(|p| p.into_inner());
                record.changes.get(*applied).cloned()
            };
            let change = match next {
                Some(change) => {
                    *self.applied.lock().unwrap_or_else(|p| p.into_inner()) += 1;
                    change.meta_change()
                }
                None => MetaChange::SetDefinitions { definitions },
            };
            self.live.apply(change).map_err(|e| error(e.to_string()))?;
        }
        *self.seen.lock().unwrap_or_else(|p| p.into_inner()) = version;
        Ok(true)
    }
}

/// Applies the additive plan from the database's live schema to `catalog`.
async fn apply_schema(backend: &PgBackend, catalog: &Catalog) -> Result<()> {
    let plan = schema::plan(backend, catalog).await?;
    if !plan.refused.is_empty() {
        return Err(Error(format!(
            "the metadata schema plan was refused: {}",
            schema::render(&plan)
        )));
    }
    schema::apply(backend, &plan, None).await?;
    Ok(())
}

impl Service for LiveMetadata {
    fn name(&self) -> &'static str {
        "metadata"
    }

    fn level(&self) -> Level {
        // Metadata is required: an engine without a validated snapshot cannot serve.
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if let Some(problem) = self
                .schema_problem
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
            {
                return Err(problem);
            }
            match &self.store {
                // The polling store is only trustworthy if it can be read.
                Store::Pg(store) => spot_meta_live::VersionStore::read(store.as_ref())
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("the metadata version store failed: {e}")),
                Store::Memory(_) => Ok(()),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut ticker = tokio::time::interval(self.schedule);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    _ = ticker.tick() => {
                        if self.deferred.load(Ordering::SeqCst) {
                            continue;
                        }
                        match self.store.as_version_store().read().await {
                            Ok(Some((version, hash))) => {
                                match self.catch_up(version, &hash).await {
                                    Ok(true) => self.adopt(version).await,
                                    Ok(false) => {}
                                    Err(e) => tracing::warn!(
                                        site = %self.site,
                                        %e,
                                        "the metadata version could not be adopted"
                                    ),
                                }
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!(
                                site = %self.site,
                                %e,
                                "the metadata version store could not be read"
                            ),
                        }
                    }
                }
            }
        })
    }
}
