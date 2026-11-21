//! [`VssProvider`]: the Windows Volume Shadow Copy backend plugged into the
//! [`spot_snapshot::SnapshotProvider`] trait, so the platform's snapshot registry can hold
//! it beside the portable providers.
//!
//! A take is the fixed sequence of [`crate::backup`]: PostgreSQL backup mode, a VSS
//! shadow copy of the volume that holds the instance's data directory, the read-only
//! exposure of the shadow, then backup mode off. The exposed tree is copied into the
//! snapshot's staging directory with a SHA-256 per file (the same checksummed copy the
//! portable providers use), and the canonical manifest is published last. On any failure
//! the shadow copy is deleted and backup mode is left before the error returns.
//!
//! The provider holds no connection: the database facts a manifest records (catalogue
//! version, feed schema, outbox generation and the transaction horizon) come from an
//! injected [`VssFacts`], which the host wires to the `spot-postgres` connection facade.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use spot_snapshot::SnapshotError;
use spot_snapshot::hash;
use spot_snapshot::manifest::{
    Consistency, DestinationPath, Snapshot, SnapshotManifest, check_destination, staging_of,
};
use spot_snapshot::provider::{
    Capabilities, Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget,
    SnapshotOptions, SnapshotProvider, SnapshotTarget, TakeOutcome, VerifyReport,
};
use spot_snapshot::restore::Restored;

use crate::backup::{PgBackupPort, SnapshotCoordinator};
use crate::error::WindowsSysError;
use crate::vss::{VolumeSet, VssApi, WriterSet};

/// The database facts a VSS manifest records. Read from the live server while backup mode
/// is on; supplied by the host, never read here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotFacts {
    /// The catalogue (`spot_meta` `catalog_version`) at take time.
    pub catalogue_version: String,
    /// The feed schema version at take time.
    pub feed_schema: String,
    /// The outbox stream generation at take time.
    pub outbox_generation: u64,
    /// The last published position within the generation at take time.
    pub outbox_last_published_seq: u64,
    /// The transaction horizon the copy is consistent at.
    pub xmin: u64,
    /// The epoch of `xmin`.
    pub epoch: u64,
}

/// Where a VSS manifest's database facts come from.
pub trait VssFacts: Send + Sync {
    /// Reads the facts from the live server.
    ///
    /// # Errors
    /// [`WindowsSysError::Io`] when the facts cannot be read.
    fn facts(&self) -> Result<SnapshotFacts, WindowsSysError>;
}

/// A [`VssFacts`] that always reports one fixed set of facts — the test and offline seam.
#[derive(Clone, Debug)]
pub struct FixedFacts(pub SnapshotFacts);

impl VssFacts for FixedFacts {
    fn facts(&self) -> Result<SnapshotFacts, WindowsSysError> {
        Ok(self.0.clone())
    }
}

/// The provider name recorded in the manifest's `provider_kind`.
pub const PROVIDER_KIND: &str = "vss";

/// The Windows Volume Shadow Copy snapshot provider.
pub struct VssProvider {
    vss: Arc<dyn VssApi>,
    backup: Arc<dyn PgBackupPort>,
    facts: Arc<dyn VssFacts>,
    clock: Arc<dyn spot_core::Clock>,
}

impl VssProvider {
    /// A provider over the given VSS API, backup port, facts source and clock.
    #[must_use]
    pub fn new(
        vss: Arc<dyn VssApi>,
        backup: Arc<dyn PgBackupPort>,
        facts: Arc<dyn VssFacts>,
        clock: Arc<dyn spot_core::Clock>,
    ) -> Self {
        VssProvider {
            vss,
            backup,
            facts,
            clock,
        }
    }

    /// The VSS API the provider drives.
    #[must_use]
    pub fn vss(&self) -> &Arc<dyn VssApi> {
        &self.vss
    }

    /// The backup port the provider drives.
    #[must_use]
    pub fn backup(&self) -> &Arc<dyn PgBackupPort> {
        &self.backup
    }
}

impl std::fmt::Debug for VssProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("VssProvider").finish()
    }
}

impl SnapshotProvider for VssProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_online: true,
            needs_stopped_instance: false,
            incremental: false,
            max_parallel: 1,
            estimated_throughput: 0,
        }
    }

    fn name(&self) -> &'static str {
        PROVIDER_KIND
    }

    fn take(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        _opts: &SnapshotOptions,
        progress: Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = Result<TakeOutcome, SnapshotError>> + Send + '_>> {
        let vss = self.vss.clone();
        let backup = self.backup.clone();
        let facts = self.facts.clone();
        let clock = self.clock.clone();
        let target = target.clone();
        let dest = dest.clone();
        Box::pin(async move {
            take_vss(&vss, &backup, &facts, &clock, &target, &dest, &*progress).await
        })
    }

    fn restore(
        &self,
        snapshot: &Snapshot,
        target: &RestoreTarget,
        opts: &RestoreOptions,
        progress: Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = Result<Restored, SnapshotError>> + Send + '_>> {
        let clock = self.clock.clone();
        let snapshot = snapshot.clone();
        let target = target.clone();
        let opts = opts.clone();
        Box::pin(async move { restore_vss(&clock, &snapshot, &target, &opts, &*progress).await })
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = Result<VerifyReport, SnapshotError>> + Send + '_>> {
        let snapshot = snapshot.clone();
        Box::pin(async move { snapshot.verify().await })
    }

    fn delete(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = Result<(), SnapshotError>> + Send + '_>> {
        let dir = snapshot.dir().to_path_buf();
        Box::pin(async move {
            tokio::fs::remove_dir_all(&dir)
                .await
                .map_err(SnapshotError::from)
        })
    }
}

/// Whole milliseconds of a clock instant, as an integer.
fn unix_ms(clock: &Arc<dyn spot_core::Clock>) -> i64 {
    let nanos = clock.now_utc().unix_timestamp_nanos();
    i64::try_from(nanos / 1_000_000).unwrap_or(i64::MAX)
}

/// The take, factored from the trait so the steps read in order.
async fn take_vss(
    vss: &Arc<dyn VssApi>,
    backup: &Arc<dyn PgBackupPort>,
    facts: &Arc<dyn VssFacts>,
    clock: &Arc<dyn spot_core::Clock>,
    target: &SnapshotTarget,
    dest: &Destination,
    progress: &dyn ProgressSink,
) -> Result<TakeOutcome, SnapshotError> {
    let SnapshotTarget::Instance { layout, tenant, .. } = target else {
        return Err(SnapshotError::io(
            "the VSS provider snapshots an instance's volumes, not a remote database",
        ));
    };
    check_destination(&DestinationPath(dest.dir.clone())).await?;
    let started_at = clock.now_utc();
    progress.on(Progress::Started {
        phase: 1,
        files_total: 0,
    });

    let volume = vss
        .volume_of(&layout.data_dir)
        .map_err(windows_to_snapshot)?;
    let volumes = VolumeSet::one(volume);
    let writers = WriterSet::all();

    let coordinator = SnapshotCoordinator::new(vss.as_ref(), backup.as_ref());
    let coordinated = coordinator
        .run(
            &volumes,
            &writers,
            &layout.data_dir,
            u64::try_from(unix_ms(clock)).unwrap_or(u64::MAX),
        )
        .map_err(windows_to_snapshot)?;

    // Stage into `<dest>.partial`; the manifest moves last and drops the suffix.
    let staged = staging_of(&dest.dir);
    let _ = tokio::fs::remove_dir_all(&staged).await;
    tokio::fs::create_dir_all(&staged).await?;
    let copied = hash::copy_tree_checked(coordinated.exposed_path(), &staged, progress).await;
    let inventory = match copied {
        Ok(inventory) => inventory,
        Err(error) => {
            let _ = coordinated.finish();
            let _ = tokio::fs::remove_dir_all(&staged).await;
            return Err(error);
        }
    };
    if let Err(error) = coordinated.finish() {
        let _ = tokio::fs::remove_dir_all(&staged).await;
        return Err(windows_to_snapshot(error));
    }

    let facts = match facts.facts() {
        Ok(facts) => facts,
        Err(error) => {
            let _ = tokio::fs::remove_dir_all(&staged).await;
            return Err(windows_to_snapshot(error));
        }
    };
    let finished_at = clock.now_utc();
    let manifest = build_manifest(
        tenant,
        &facts,
        &volumes,
        started_at,
        finished_at,
        &inventory,
    );
    manifest.publish(&dest.dir).await?;
    progress.on(Progress::Finished { ok: true });
    Ok(TakeOutcome {
        snapshot: Snapshot::open(&dest.dir).await?,
    })
}

/// Builds the canonical manifest for a VSS take.
fn build_manifest(
    tenant: &str,
    facts: &SnapshotFacts,
    volumes: &VolumeSet,
    started_at: time::OffsetDateTime,
    finished_at: time::OffsetDateTime,
    inventory: &hash::Inventory,
) -> SnapshotManifest {
    let total_bytes: u64 = inventory
        .values()
        .fold(0u64, |sum, file| sum.saturating_add(file.bytes));
    let mut platform_extra = std::collections::BTreeMap::new();
    let names: Vec<&str> = volumes
        .as_slice()
        .iter()
        .map(|volume| volume.as_str())
        .collect();
    platform_extra.insert("volumes".to_owned(), names.join(","));
    SnapshotManifest {
        id: uuid::Uuid::now_v7().to_string(),
        tenant: tenant.to_owned(),
        provider: crate::CRATE_NAME.to_owned(),
        provider_version: crate::PROVIDER_VERSION.to_owned(),
        provider_kind: PROVIDER_KIND.to_owned(),
        pg_major: spot_snapshot::SUPPORTED_MAJOR_VERSION,
        catalogue_version: facts.catalogue_version.clone(),
        feed_schema: facts.feed_schema.clone(),
        outbox_generation: facts.outbox_generation,
        outbox_last_published_seq: facts.outbox_last_published_seq,
        started_at: SnapshotManifest::format_instant(started_at),
        finished_at: SnapshotManifest::format_instant(finished_at),
        consistency: Consistency::OnlineConsistent {
            xmin: facts.xmin,
            epoch: facts.epoch,
        },
        total_bytes,
        file_count: u64::try_from(inventory.len()).unwrap_or(u64::MAX),
        files: inventory.clone(),
        node_local: std::collections::BTreeMap::new(),
        platform_extra,
    }
}

/// The restore, factored from the trait. The caller must have stopped the instance: this
/// provider copies files back into the data directory and does not own a supervisor.
async fn restore_vss(
    clock: &Arc<dyn spot_core::Clock>,
    snapshot: &Snapshot,
    target: &RestoreTarget,
    opts: &RestoreOptions,
    progress: &dyn ProgressSink,
) -> Result<Restored, SnapshotError> {
    opts.check_replace(target)?;
    let RestoreTarget::Instance { layout, .. } = target else {
        return Err(SnapshotError::io(
            "the VSS provider restores into an instance's data directory, not a database",
        ));
    };
    progress.on(Progress::Started {
        phase: 2,
        files_total: 0,
    });
    snapshot.verify().await?;
    let data_dir = &layout.data_dir;
    let populated = dir_nonempty(data_dir).await;
    if populated && !opts.replace {
        return Err(SnapshotError::ReplaceRequired {
            target: data_dir.display().to_string(),
        });
    }
    let replaced_aside = if populated {
        let aside = PathBuf::from(format!(
            "{}.pre-restore-{}",
            data_dir.display(),
            unix_ms(clock)
        ));
        tokio::fs::rename(data_dir, &aside).await?;
        Some(aside.display().to_string())
    } else {
        None
    };
    hash::copy_manifest_files(
        snapshot.dir(),
        &snapshot.manifest().files,
        data_dir,
        "",
        progress,
    )
    .await?;
    progress.on(Progress::Finished { ok: true });
    Ok(Restored {
        snapshot: snapshot.clone(),
        location: data_dir.display().to_string(),
        activated: true,
        replaced_aside,
    })
}

/// Whether `dir` exists and holds at least one entry.
async fn dir_nonempty(dir: &std::path::Path) -> bool {
    match tokio::fs::read_dir(dir).await {
        Ok(mut entries) => entries.next_entry().await.ok().flatten().is_some(),
        Err(_) => false,
    }
}

/// Maps a Windows module error onto the snapshot error vocabulary, keeping the code in
/// the message (the snapshot error has no `agent_windows_sys.*` variants).
fn windows_to_snapshot(error: WindowsSysError) -> SnapshotError {
    SnapshotError::io(format!("{}: {error}", error.code()))
}
