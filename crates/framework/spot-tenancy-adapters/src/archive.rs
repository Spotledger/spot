//! The archive adapter: a tenant's final backup, taken and verified through `spot-snapshot`.
//!
//! `final_backup` writes into one deterministic directory per tenant (`root/<tenant id>`), so a
//! replayed step finds the completed snapshot there instead of taking a second one. `verify`
//! reopens that manifest and re-hashes every file against it: a directory that merely exists, or
//! whose bytes changed since the take, does not verify.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_snapshot::error::SnapshotError;
use spot_snapshot::{
    Destination, Progress, ProgressSink, Snapshot, SnapshotOptions, SnapshotProvider,
    SnapshotTarget,
};
use spot_tenancy::Tenant;
use spot_tenancy::model::{ClusterId, DatabaseId};
use spot_tenancy::ports::{Archiver, PortResult};

use crate::block::block_on;
use crate::error::AdapterError;

/// A sink that ignores progress; the adapter never prints or logs by itself.
struct NoProgress;

impl ProgressSink for NoProgress {
    fn on(&self, _event: Progress) {}
}

/// [`Archiver`](spot_tenancy::ports::Archiver) over a [`SnapshotProvider`].
pub struct SnapshotArchiver<P> {
    provider: P,
    connections: BTreeMap<(ClusterId, DatabaseId), String>,
    root: PathBuf,
    runtime: Arc<tokio::runtime::Runtime>,
    options: SnapshotOptions,
    progress: Arc<dyn ProgressSink>,
}

impl<P: SnapshotProvider> SnapshotArchiver<P> {
    /// An archiver that connects to each placement database with the connection string the
    /// caller supplied and writes snapshots under `root`.
    ///
    /// `runtime` is what the synchronous port methods block on. A placement without a
    /// connection string is not backed up: the step fails with
    /// [`AdapterError::NotConfigured`] rather than reporting a backup that does not exist.
    pub fn new(
        provider: P,
        connections: Vec<((ClusterId, DatabaseId), String)>,
        root: impl Into<PathBuf>,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Self {
        Self {
            provider,
            connections: connections.into_iter().collect(),
            root: root.into(),
            runtime,
            options: SnapshotOptions::default(),
            progress: Arc::new(NoProgress),
        }
    }

    /// The provider, for the caller that also restores from it.
    #[must_use]
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// The directory the tenant's final backup lives in (and its reference, as text).
    #[must_use]
    pub fn destination(&self, tenant: &Tenant) -> PathBuf {
        self.root.join(tenant.id.to_string())
    }

    fn block<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        block_on(self.runtime.as_ref(), future)
    }

    fn target(&self, tenant: &Tenant) -> Result<SnapshotTarget, AdapterError> {
        let connection = self
            .connections
            .get(&(
                tenant.placement.cluster.clone(),
                tenant.placement.database.clone(),
            ))
            .ok_or(AdapterError::NotConfigured)?;
        Ok(SnapshotTarget::Database {
            connection: connection.clone(),
            tenant: tenant.id.to_string(),
            database: tenant.placement.database.to_string(),
        })
    }

    fn completed(&self, tenant: &Tenant, dir: &Path) -> Option<Snapshot> {
        let snapshot = self.block(Snapshot::open(dir.to_path_buf())).ok()?;
        (snapshot.manifest().tenant == tenant.id.to_string()).then_some(snapshot)
    }
}

impl<P: SnapshotProvider> Archiver for SnapshotArchiver<P> {
    fn final_backup(&self, tenant: &Tenant) -> PortResult<String> {
        let dir = self.destination(tenant);
        if self.completed(tenant, &dir).is_some() {
            // A replay of a step whose backup landed: reuse it, never take a second one.
            return Ok(reference(&dir));
        }
        if dir.exists() {
            // Something that is not this tenant's completed snapshot is in the way; a backup
            // must never be written over data the manifest does not describe.
            return Err(AdapterError::Backup.failure(false));
        }
        let target = self.target(tenant).map_err(|error| error.failure(false))?;
        let destination = Destination { dir: dir.clone() };
        let taken = self.block(self.provider.take(
            &target,
            &destination,
            &self.options,
            Arc::clone(&self.progress),
        ));
        match taken {
            Ok(outcome) => Ok(reference(outcome.snapshot.dir())),
            Err(_) => Err(AdapterError::Backup.failure(true)),
        }
    }

    fn verify(&self, tenant: &Tenant, backup_ref: &str) -> PortResult<bool> {
        // Only the tenant's own, deterministic directory is its final backup: any other
        // reference is not this tenant's archive and cannot verify.
        if Path::new(backup_ref) != self.destination(tenant) {
            return Ok(false);
        }
        let Some(snapshot) = self.completed(tenant, Path::new(backup_ref)) else {
            return Ok(false);
        };
        match self.block(self.provider.verify(&snapshot)) {
            Ok(_) => Ok(true),
            // The manifest or a file's checksum disagrees: the backup did not verify.
            Err(SnapshotError::VerifyFailed { .. } | SnapshotError::VersionMismatch { .. }) => {
                Ok(false)
            }
            // Verification could not be attempted (the directory or the provider is unusable).
            Err(_) => Err(AdapterError::Backup.failure(true)),
        }
    }
}

fn reference(dir: &Path) -> String {
    dir.to_string_lossy().into_owned()
}
