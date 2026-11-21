//! Restore on a new PC: the recovery key, the manifest and the chunks, nothing else.
//!
//! [`restore`] needs no local state: the newest manifest names the chunks, the manifest's
//! recovery wrap is opened with the recovery key, every chunk is checked against its
//! digest and its authentication tag before it is appended, and the finished artefact is
//! checked against the source digest the manifest recorded. A wrong key fails with
//! [`CloudBackupError::WrongKey`] before any chunk is read, and the error never echoes the
//! key or a byte of plaintext.
//!
//! A restore is resumable at chunk granularity. The caller passes a
//! [`RestoreCheckpoint`]; every completed chunk is recorded there together with the
//! plaintext prefix restored so far. A dropped connection then resumes at the first
//! missing chunk instead of starting again. The checkpoint holds customer plaintext, so
//! it is as sensitive as the backup itself and must stay on the machine.
//!
//! [`Restorer`] is the handoff to the restore path (`spot-cli restore` in WP-PRD-01): the
//! caller decides what to do with the verified artefact.

use std::future::Future;
use std::pin::Pin;

use spot_core::SiteId;
use spot_secrets::KeyProvider;

use crate::chunk;
use crate::crypto;
use crate::error::{CloudBackupError, Result};
use crate::manifest::{BackupManifest, BackupState, unwrap_data_key};
use crate::recovery::RecoveryKey;
use crate::store::ObjectStore;

/// A boxed `Send` future returned by [`Restorer`].
pub type RestoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The verified artefact handed to the restore path.
pub struct RestoredArtefact {
    /// The manifest the artefact was restored from.
    pub manifest: BackupManifest,
    /// The artefact's name, as the backup source recorded it.
    pub name: String,
    /// The plaintext bytes.
    pub bytes: Vec<u8>,
    /// How many chunks this call downloaded and opened.
    pub chunks_downloaded: u64,
    /// How many chunks the checkpoint already held.
    pub chunks_skipped: u64,
}

impl std::fmt::Debug for RestoredArtefact {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The plaintext bytes are the customer's data: never printed.
        formatter
            .debug_struct("RestoredArtefact")
            .field("name", &self.name)
            .field("bytes", &self.bytes.len())
            .field("chunks_downloaded", &self.chunks_downloaded)
            .field("chunks_skipped", &self.chunks_skipped)
            .finish_non_exhaustive()
    }
}

/// Where a restored artefact goes (`spot-cli restore` implements this).
pub trait Restorer: Send + Sync {
    /// Takes the verified artefact and applies it.
    ///
    /// # Errors
    /// Any typed [`CloudBackupError`]; the caller keeps the checkpoint so a failed
    /// handoff can be retried without downloading again.
    fn deliver<'a>(&'a self, artefact: &'a RestoredArtefact) -> RestoreFuture<'a, ()>;
}

/// What a restore may accept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RestoreOptions {
    /// Restore a backup whose newest manifest says `unverified`. Off by default: an
    /// unverified backup is never presented as restorable.
    pub allow_unverified: bool,
}

/// How far a restore got, and the plaintext prefix it already produced.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RestoreCheckpoint {
    /// The backup this checkpoint belongs to.
    pub backup_id: String,
    completed: Vec<u32>,
    partial: Vec<u8>,
}

impl std::fmt::Debug for RestoreCheckpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The partial plaintext is the customer's data: only its length is shown.
        formatter
            .debug_struct("RestoreCheckpoint")
            .field("backup_id", &self.backup_id)
            .field("completed", &self.completed.len())
            .field("partial_bytes", &self.partial.len())
            .finish()
    }
}

impl RestoreCheckpoint {
    /// An empty checkpoint for `backup_id`.
    pub fn new(backup_id: impl Into<String>) -> Self {
        Self {
            backup_id: backup_id.into(),
            completed: Vec::new(),
            partial: Vec::new(),
        }
    }

    /// The chunk indices restored so far, in order.
    pub fn completed(&self) -> &[u32] {
        &self.completed
    }

    /// The plaintext bytes restored so far.
    pub fn partial_len(&self) -> u64 {
        u64::try_from(self.partial.len()).unwrap_or(u64::MAX)
    }

    /// Forgets everything, so the restore starts at chunk 0.
    pub fn reset(&mut self) {
        self.completed.clear();
        self.partial.clear();
    }

    fn is_complete(&self, index: u32) -> bool {
        self.completed.contains(&index)
    }
}

/// Restores one backup into memory, resuming from `checkpoint`.
///
/// # Errors
/// [`CloudBackupError::NotFound`] when the tenant has no such backup,
/// [`CloudBackupError::WrongKey`] for a recovery key that does not open the manifest,
/// [`CloudBackupError::ManifestSignature`] for a manifest altered after signing,
/// [`CloudBackupError::InvalidState`] for a pending or (unless allowed) unverified
/// backup, [`CloudBackupError::ChunkCorrupt`] for a chunk that fails its digest or tag,
/// and [`CloudBackupError::Restore`] when the finished artefact does not match the
/// manifest.
pub async fn restore(
    store: &dyn ObjectStore,
    tenant: &SiteId,
    backup_id: &str,
    recovery: &RecoveryKey,
    options: RestoreOptions,
    checkpoint: &mut RestoreCheckpoint,
) -> Result<RestoredArtefact> {
    let provider = recovery.provider();
    restore_with_key(
        store, tenant, backup_id, &provider, "recovery", options, checkpoint,
    )
    .await
}

/// Restores a backup with any key provider and manifest label: the recovery key's
/// `recovery` wrap, or an escrow wrap the customer named when granting escrow.
///
/// # Errors
/// As [`restore`]; [`CloudBackupError::ManifestInvalid`] when the manifest has no wrap
/// under `label`.
pub async fn restore_with_key(
    store: &dyn ObjectStore,
    tenant: &SiteId,
    backup_id: &str,
    provider: &dyn KeyProvider,
    label: &str,
    options: RestoreOptions,
    checkpoint: &mut RestoreCheckpoint,
) -> Result<RestoredArtefact> {
    if checkpoint.backup_id != backup_id {
        *checkpoint = RestoreCheckpoint::new(backup_id);
    }
    let manifest = newest_manifest(store, tenant.as_str(), backup_id).await?;
    if manifest.tenant != tenant.as_str() || manifest.backup_id != backup_id {
        return Err(CloudBackupError::ManifestInvalid {
            backup_id: backup_id.to_owned(),
            reason: "the manifest belongs to another tenant or backup".to_owned(),
        });
    }
    match &manifest.state {
        BackupState::Verified => {}
        BackupState::Pending => {
            return Err(CloudBackupError::InvalidState {
                backup_id: backup_id.to_owned(),
                operation: "restore",
                reason: "the backup is still pending verification".to_owned(),
            });
        }
        BackupState::Unverified { .. } if !options.allow_unverified => {
            return Err(CloudBackupError::InvalidState {
                backup_id: backup_id.to_owned(),
                operation: "restore",
                reason: "the backup is unverified; pass allow_unverified to restore it".to_owned(),
            });
        }
        BackupState::Unverified { .. } => {}
    }

    // The key opens the manifest's data-key wrap; a wrong key fails here, before a
    // single chunk is read.
    let data_key = unwrap_data_key(provider, tenant.as_str(), backup_id, label, &manifest).await?;
    manifest.verify_signature(&data_key)?;

    let mut downloaded = 0u64;
    let mut skipped = 0u64;
    for entry in &manifest.chunks {
        if checkpoint.is_complete(entry.index) {
            skipped = skipped.saturating_add(1);
            continue;
        }
        let bytes = store
            .get(&entry.object)
            .await
            .map_err(|error| CloudBackupError::Restore {
                backup_id: backup_id.to_owned(),
                stage: "download",
                reason: format!("chunk {} could not be read: {}", entry.index, error.code()),
            })?;
        let plain = chunk::open(
            &manifest.backup_id,
            &data_key,
            entry.index,
            &entry.sha256,
            &bytes,
        )?;
        if u64::try_from(plain.len()).unwrap_or(u64::MAX) != entry.plain_len {
            return Err(CloudBackupError::ChunkCorrupt {
                backup_id: backup_id.to_owned(),
                index: entry.index,
                reason: "the plaintext length does not match the manifest",
            });
        }
        checkpoint.partial.extend_from_slice(&plain);
        checkpoint.completed.push(entry.index);
        downloaded = downloaded.saturating_add(1);
    }

    if u64::try_from(checkpoint.partial.len()).unwrap_or(u64::MAX) != manifest.source.bytes {
        return Err(CloudBackupError::Restore {
            backup_id: backup_id.to_owned(),
            stage: "verify",
            reason: "the restored length does not match the manifest".to_owned(),
        });
    }
    if crypto::sha256_hex(&checkpoint.partial) != manifest.source.sha256 {
        return Err(CloudBackupError::Restore {
            backup_id: backup_id.to_owned(),
            stage: "verify",
            reason: "the restored digest does not match the manifest".to_owned(),
        });
    }

    let bytes = std::mem::take(&mut checkpoint.partial);
    let name = manifest.source.name.clone();
    Ok(RestoredArtefact {
        manifest,
        name,
        bytes,
        chunks_downloaded: downloaded,
        chunks_skipped: skipped,
    })
}

/// Restores and hands the artefact to `restorer`.
///
/// # Errors
/// As [`restore`], plus whatever the restorer reports.
pub async fn restore_into(
    store: &dyn ObjectStore,
    tenant: &SiteId,
    backup_id: &str,
    recovery: &RecoveryKey,
    options: RestoreOptions,
    checkpoint: &mut RestoreCheckpoint,
    restorer: &dyn Restorer,
) -> Result<RestoredArtefact> {
    let artefact = restore(store, tenant, backup_id, recovery, options, checkpoint).await?;
    restorer.deliver(&artefact).await?;
    Ok(artefact)
}

/// The newest manifest of one backup, if the tenant has it.
async fn newest_manifest(
    store: &dyn ObjectStore,
    tenant: &str,
    backup_id: &str,
) -> Result<BackupManifest> {
    let prefix = BackupManifest::backup_prefix(tenant, backup_id);
    let objects = store.list(&prefix).await?;
    let newest = objects
        .iter()
        .filter_map(|object| {
            let name = object.key.strip_prefix(&prefix)?;
            let digits = name.strip_prefix("manifest-")?.strip_suffix(".json")?;
            if digits.len() != 4 {
                return None;
            }
            digits.parse::<u32>().ok()
        })
        .max();
    let Some(version) = newest else {
        return Err(CloudBackupError::NotFound {
            backup_id: backup_id.to_owned(),
            tenant: tenant.to_owned(),
        });
    };
    let key = BackupManifest::manifest_key(tenant, backup_id, version);
    let bytes = store.get(&key).await?;
    let text = String::from_utf8(bytes).map_err(|_| CloudBackupError::ManifestInvalid {
        backup_id: backup_id.to_owned(),
        reason: "the stored manifest is not UTF-8".to_owned(),
    })?;
    BackupManifest::from_json(&text)
}
