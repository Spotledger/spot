//! Sync catch-up served and applied (WP-RTB-02.T14, ADR-066). Operator guide:
//! `docs/running-spot/sync_catchup_serve.md`.
//!
//! `sync_catchup` (WP-RTB-02.T7) routes a returning node; this module moves the data.
//!
//! # Hub
//!
//! Provides [`ChunkServer`]: bootstrap chunks cut from the latest **verified** snapshot of the
//! journal store ([`StoreSnapshots`], read only) for the node's registered scope, and catch-up
//! chunks from a planner plan, each sealed with its hash. A returning node is told to wait while no
//! snapshot is verified (`sync_catchup_serve.no_verified_snapshot`) rather than get an unverified
//! base.
//!
//! # Edge
//!
//! Provides [`ChunkApplier`]: verifies each chunk's hash, refuses an index that is not the next,
//! applies it through the node's apply path and only then moves its checkpoint (the same progress
//! table the hub uses, keyed by the hub), so a cut repeats at most the chunk in flight.
//!
//! # Not ready
//!
//! The module registers nothing on a standalone site. On a hub it needs the director
//! (`sync_catchup`), the read-only journal store and the manifest keys (`journal_wiring`) and the
//! hub's scope provider ([`SeamsProvider`], the hub sync hosting); on an edge it needs the node's
//! apply path ([`ApplyTarget`], the node sync hosting). Anything absent is named in readiness
//! (`sync_catchup_serve.not_ready`) and the module provides nothing: no chunk is served or applied
//! on a guess.
//!
//! Settings `[services.sync_catchup_serve]`: `enabled`, `chunk_max_records` (500),
//! `chunk_max_bytes` (4 MiB), `hard_limit_records` (5,000). Unknown keys are refused.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use spot_journal_snapshot::{SnapshotInfo, SnapshotKeys, SnapshotReader, list_snapshots};
use spot_journal_snapshot_pg::SnapshotKeyring;
use spot_journal_store::JournalStore;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_secrets::KeyProvider;
use spot_sync_bootstrap::{BootstrapError, RecordKey, SnapshotSource};
use spot_sync_catchup_host::{
    ApplyTarget, ChunkApplier, ChunkServer, Director, PgProgress, SeamsProvider, ServeError,
    SnapshotProvider,
};
use spot_sync_closure::Limits;
use tokio::runtime::Handle;

use crate::services::journal_access::ManifestKeys;
use crate::services::node_role::{ActiveRole, Role};
use crate::services::secrets::SecretsHost;
use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result};

/// After `sync_catchup` (1150), whose director it serves for, and after `journal_wiring` (1105),
/// whose store and keys it reads.
pub const ORDER: u32 = 1155;

/// The key the edge's checkpoint is stored under: its one hub.
const HUB: &str = "hub";

/// `[services.sync_catchup_serve]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` registers nothing.
    pub enabled: bool,
    /// Most records in one bundle.
    pub chunk_max_records: usize,
    /// Most image bytes in one bundle.
    pub chunk_max_bytes: usize,
    /// More records than this in a dependency group refuses the bootstrap.
    pub hard_limit_records: usize,
}

impl Default for Settings {
    fn default() -> Self {
        let limits = Limits::default();
        Self {
            enabled: true,
            chunk_max_records: limits.max_records,
            chunk_max_bytes: limits.max_bytes,
            hard_limit_records: limits.hard_limit_records,
        }
    }
}

impl Settings {
    /// The closure bounds the bootstrap cuts chunks with.
    ///
    /// # Errors
    /// A zero bound.
    pub fn limits(&self) -> Result<Limits> {
        if self.chunk_max_records == 0
            || self.chunk_max_bytes == 0
            || self.hard_limit_records < self.chunk_max_records
        {
            return Err(Error(
                "services.sync_catchup_serve: the chunk limits must be positive and the hard limit at least the chunk size"
                    .to_owned(),
            ));
        }
        Ok(Limits {
            max_records: self.chunk_max_records,
            max_bytes: self.chunk_max_bytes,
            hard_limit_records: self.hard_limit_records,
        })
    }
}

fn source_failed(reason: &str) -> BootstrapError {
    BootstrapError::SourceFailed {
        reason: reason.to_owned(),
    }
}

/// The latest verified snapshot of the tenant, read from the journal store. Blocking: the
/// bootstrap runs on the blocking pool and re-enters the runtime for each read.
pub struct StoreSnapshots {
    store: Arc<dyn JournalStore>,
    tenant: String,
    keys: Arc<dyn SnapshotKeys>,
    runtime: Handle,
}

impl StoreSnapshots {
    /// A provider over `store` for `tenant`.
    #[must_use]
    pub fn new(
        store: Arc<dyn JournalStore>,
        tenant: String,
        keys: Arc<dyn SnapshotKeys>,
        runtime: Handle,
    ) -> Self {
        Self {
            store,
            tenant,
            keys,
            runtime,
        }
    }
}

impl SnapshotProvider for StoreSnapshots {
    fn open(&self) -> std::result::Result<Box<dyn SnapshotSource>, ServeError> {
        let listed = self
            .runtime
            .block_on(list_snapshots(&*self.store, &self.tenant, &*self.keys))
            .map_err(|_| ServeError::Bootstrap("sync_bootstrap.source_failed"))?;
        let newest = listed
            .iter()
            .filter(|info| info.verified)
            .max_by_key(|info| (info.generation, info.position))
            .copied()
            .ok_or(ServeError::NoVerifiedSnapshot)?;
        Ok(Box::new(StoreSource {
            store: Arc::clone(&self.store),
            tenant: self.tenant.clone(),
            keys: Arc::clone(&self.keys),
            runtime: self.runtime.clone(),
            info: newest,
        }))
    }
}

/// One verified snapshot as a bootstrap source.
struct StoreSource {
    store: Arc<dyn JournalStore>,
    tenant: String,
    keys: Arc<dyn SnapshotKeys>,
    runtime: Handle,
    info: SnapshotInfo,
}

impl SnapshotSource for StoreSource {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn info(&self) -> SnapshotInfo {
        self.info
    }

    fn scan(
        &self,
        visit: &mut dyn FnMut(
            spot_journal_snapshot::ImageRecord,
        ) -> std::result::Result<(), BootstrapError>,
    ) -> std::result::Result<(), BootstrapError> {
        self.runtime.block_on(async {
            let reader = SnapshotReader::open(
                &*self.store,
                &self.tenant,
                self.info.generation,
                self.info.position,
                &*self.keys,
            )
            .await
            .map_err(|e| source_failed(e.code()))?;
            let mut images = reader.images();
            while let Some(record) = images.next().await.map_err(|e| source_failed(e.code()))? {
                visit(record)?;
            }
            Ok(())
        })
    }

    fn fetch(
        &self,
        keys: &[RecordKey],
    ) -> std::result::Result<Vec<spot_journal_snapshot::ImageRecord>, BootstrapError> {
        // The snapshot is stored sorted by key and read part by part: a fetch is one pass that
        // keeps the records asked for.
        let wanted: HashSet<(&str, &str)> = keys
            .iter()
            .map(|k| (k.entity.as_str(), k.id.as_str()))
            .collect();
        let mut found = Vec::new();
        self.scan(&mut |record| {
            if wanted.contains(&(record.entity.as_str(), record.id.as_str())) {
                found.push(record);
            }
            Ok(())
        })?;
        Ok(found)
    }
}

/// A bridge for the edge's checkpoint table.
fn build_bridge(backend: &spot_postgres::PgBackend, pool_size: usize) -> Result<Arc<Bridge>> {
    let largest = BridgeConfig::largest_in_flight(pool_size);
    if largest < 1 {
        return Err(Error(format!(
            "services.sync_catchup_serve needs a PostgreSQL pool of at least 3 connections (postgres.pool_max_size is {pool_size})"
        )));
    }
    let config = BridgeConfig {
        name: "sync_catchup_serve".to_owned(),
        max_in_flight: largest.min(2),
        workers: 2,
        queue_capacity: 8,
        default_deadline: Duration::from_secs(10),
        max_deadline: Duration::from_secs(30),
    };
    let backend = backend.clone();
    std::thread::spawn(move || {
        Bridge::for_backend(config, &backend).map_err(|e| Error(format!("{e}")))
    })
    .join()
    .map_err(|_| Error("the catch-up apply bridge thread failed".to_owned()))?
    .map(Arc::new)
}

/// What a site provides, kept across a reload (a reload must not build a second one).
#[derive(Clone, Default)]
struct Built {
    server: Option<Arc<ChunkServer>>,
    applier: Option<Arc<ChunkApplier>>,
}

fn built() -> &'static Mutex<HashMap<String, Built>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Built>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// The chunk server of a running hub (for a harness).
#[must_use]
pub fn hosted_server(site: &str) -> Option<Arc<ChunkServer>> {
    built()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .and_then(|b| b.server.clone())
}

/// Registers the chunk server on a hub and the chunk applier on an edge, each only when its
/// prerequisites are provided.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("sync_catchup_serve")?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        let existing = built()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&site)
            .cloned();
        if let Some(existing) = existing {
            if let Some(server) = existing.server {
                ctx.provide::<ChunkServer>(server)?;
            }
            if let Some(applier) = existing.applier {
                ctx.provide::<ChunkApplier>(applier)?;
            }
        }
        return Ok(());
    }
    let Some(active) = ctx.port::<ActiveRole>() else {
        return Ok(());
    };
    let result = match active.role() {
        Role::Standalone => Built::default(),
        Role::Hub => register_hub(ctx, &settings).await?,
        Role::Edge => register_edge(ctx)?,
    };
    built()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(site, result);
    Ok(())
}

/// Says what is missing, in the log and in readiness, and provides nothing.
fn not_ready(ctx: &mut SiteCtx<'_>, missing: Vec<&'static str>) {
    let reason = format!(
        "sync_catchup_serve.not_ready: {} not provided",
        missing.join(", ")
    );
    tracing::info!(code = "sync_catchup_serve.not_ready", missing = %missing.join(", "), "sync_catchup_serve is not ready");
    ctx.add(Idle::new(
        "sync_catchup_serve",
        Level::Optional,
        move || {
            let reason = reason.clone();
            Box::pin(async move { Err(reason) })
        },
    ));
}

async fn register_hub(ctx: &mut SiteCtx<'_>, settings: &Settings) -> Result<Built> {
    let limits = settings.limits()?;
    let director = ctx.port::<Director>();
    let store = ctx.port::<dyn JournalStore>();
    let manifest_keys = ctx.port::<dyn ManifestKeys>();
    let seams = ctx.port::<dyn SeamsProvider>();
    let secrets = ctx.port::<SecretsHost>();
    let mut missing = Vec::new();
    if director.is_none() {
        missing.push("the catch-up director");
    }
    if store.is_none() || manifest_keys.is_none() || secrets.is_none() {
        missing.push("the journal store and keys");
    }
    if seams.is_none() {
        missing.push("the hub's node scope provider");
    }
    let (Some(director), Some(store), Some(_keys), Some(seams), Some(secrets)) =
        (director, store, manifest_keys, seams, secrets)
    else {
        not_ready(ctx, missing);
        return Ok(Built::default());
    };
    let provider: Arc<dyn KeyProvider> = secrets.keyring().clone();
    let keyring = SnapshotKeyring::derive(&provider).await.map_err(|_| {
        Error("services.sync_catchup_serve: the snapshot keys cannot be derived".to_owned())
    })?;
    let runtime = Handle::try_current()
        .map_err(|_| Error("services.sync_catchup_serve: no async runtime".to_owned()))?;
    let snapshots = Arc::new(StoreSnapshots::new(
        store,
        ctx.label().to_owned(),
        Arc::new(keyring.keys()),
        runtime,
    ));
    let server = Arc::new(ChunkServer::new(director, snapshots, seams, limits));
    ctx.provide::<ChunkServer>(Arc::clone(&server))?;
    Ok(Built {
        server: Some(server),
        applier: None,
    })
}

fn register_edge(ctx: &mut SiteCtx<'_>) -> Result<Built> {
    let Some(target) = ctx.port::<dyn ApplyTarget>() else {
        not_ready(ctx, vec!["the node's apply path"]);
        return Ok(Built::default());
    };
    let env = *ctx.env()?;
    let bridge = build_bridge(env.backend, env.loaded.common.postgres.pool_max_size)?;
    let applier = Arc::new(ChunkApplier::new(
        HUB,
        Arc::new(PgProgress::new(bridge)),
        target,
    ));
    ctx.provide::<ChunkApplier>(Arc::clone(&applier))?;
    Ok(Built {
        server: None,
        applier: Some(applier),
    })
}
