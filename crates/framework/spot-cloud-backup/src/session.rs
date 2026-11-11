//! The client side: take an artefact, seal it into chunks, upload, verify and record.
//!
//! A [`BackupSession`] is one scheduled backup (the scheduler of WP-PRD-04 calls it
//! through [`BackupSource`]). It runs entirely on the customer's machine:
//!
//! 1. the source produces an [`Artefact`] (the platform backup package);
//! 2. a fresh per-backup data key is generated, and wrapped under the tenant recovery
//!    key — plus one escrow wrap when the customer explicitly granted escrow;
//! 3. the artefact is split into fixed-size chunks and each chunk is sealed;
//! 4. the sealed chunks go through the destination — an [`ObjectStore`] directly (the
//!    filesystem store) or a [`BackupService`] (the cloud service), which enforces the
//!    licence and the quota;
//! 5. the signed manifest is committed;
//! 6. verification re-reads a random sample (or everything) and any chunk that is missing
//!    or corrupt is uploaded again as a new generation; the final manifest version
//!    records the state, `verified` or `unverified`.
//!
//! Only ciphertext and the wrapped data key cross the destination: the service never
//! sees the artefact, the data key or the recovery key.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use spot_core::SiteId;
use time::OffsetDateTime;
use time::format_description::FormatItem;
use time::macros::format_description;

use crate::chunk::{self, EncryptedChunk};
use crate::crypto::{self, DataKey};
use crate::error::{CloudBackupError, Result};
use crate::manifest::{BackupManifest, ChunkEntry, SourceInfo, wrap_data_key};
use crate::recovery::{Escrow, RecoveryKey, RecoveryKeyProvider, RecoveryPhrase};
use crate::service::{BackupService, BackupTicket, StorageQuota, TenantAuth, UploadRequest};
use crate::store::{ObjectReader, ObjectStore, PutOutcome, StoreFuture};
use crate::verify::{self, VerifyMode, VerifyReport};

/// A boxed `Send` future returned by [`BackupSource`].
pub type SourceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The artefact one backup packages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artefact {
    /// A name for the artefact (recorded, never interpreted as a path).
    pub name: String,
    /// The plaintext bytes.
    pub bytes: Vec<u8>,
}

impl Artefact {
    /// An artefact with a name.
    pub fn new(name: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            bytes,
        }
    }
}

/// Where a scheduled backup gets its artefact.
///
/// WP-PRD-04's runner implements this on the customer's machine; the trait is here so
/// the cloud client takes a package and nothing else.
pub trait BackupSource: Send + Sync {
    /// Produces the artefact for `tenant` at `now` (Unix seconds).
    ///
    /// # Errors
    /// Any typed [`CloudBackupError`]; a failure means no backup was uploaded.
    fn produce<'a>(&'a self, tenant: &'a SiteId, now: i64) -> SourceFuture<'a, Artefact>;
}

/// A source over one fixed artefact, for tests and for a host that already has the
/// package in hand.
#[derive(Clone, Debug)]
pub struct FixedSource {
    artefact: Artefact,
}

impl FixedSource {
    /// A source that always produces `artefact`.
    pub fn new(artefact: Artefact) -> Self {
        Self { artefact }
    }
}

impl BackupSource for FixedSource {
    fn produce<'a>(&'a self, _tenant: &'a SiteId, _now: i64) -> SourceFuture<'a, Artefact> {
        let artefact = self.artefact.clone();
        Box::pin(async move { Ok(artefact) })
    }
}

/// What one session may spend and how hard it verifies.
#[derive(Clone, Debug)]
pub struct UploadOptions {
    /// The chunk size; defaults to [`chunk::CHUNK_SIZE`].
    pub chunk_size: u64,
    /// The verification mode run after the commit.
    pub verify: VerifyMode,
    /// How many repair-and-verify rounds a failed chunk gets.
    pub retries: u32,
    /// The client-side quota used when the destination is an object store; a service
    /// destination enforces its own quota instead.
    pub quota: StorageQuota,
    /// Whether the customer granted escrow for this backup.
    pub escrow: Escrow,
}

impl Default for UploadOptions {
    fn default() -> Self {
        Self {
            chunk_size: chunk::CHUNK_SIZE,
            verify: VerifyMode::default(),
            retries: 2,
            quota: StorageQuota::unlimited(),
            escrow: Escrow::None,
        }
    }
}

/// What a session produced.
#[derive(Debug)]
pub struct BackupOutcome {
    /// The final manifest version, with its state and signature.
    pub manifest: BackupManifest,
    /// The recovery phrase, to show to the customer once. Never logged.
    pub recovery_phrase: RecoveryPhrase,
    /// The verification report behind the final state.
    pub verification: VerifyReport,
    /// Ciphertext bytes written (chunks and manifest versions).
    pub uploaded_bytes: u64,
    /// How many repair rounds ran.
    pub retries: u32,
}

/// The session's destination: an object store, or the cloud service with its ticket.
enum Destination<'a> {
    Store {
        store: &'a dyn ObjectStore,
    },
    Service {
        service: &'a dyn BackupService,
        auth: Box<TenantAuth>,
        ticket: Option<BackupTicket>,
    },
}

impl Destination<'_> {
    async fn begin(
        &mut self,
        tenant: &SiteId,
        request: &UploadRequest,
        quota: &StorageQuota,
        now: i64,
    ) -> Result<()> {
        match self {
            Self::Store { store } => {
                let used = used_bytes(*store, tenant).await?;
                quota.check(used, request.cipher_bytes)?;
                Ok(())
            }
            Self::Service {
                service,
                auth,
                ticket,
            } => {
                let begun = service.begin(auth.as_ref(), request, now).await?;
                *ticket = Some(begun);
                Ok(())
            }
        }
    }

    async fn put_chunk(&mut self, chunk: &EncryptedChunk, object: &str) -> Result<()> {
        match self {
            Self::Store { store } => {
                match store.put(object, chunk.bytes.clone()).await? {
                    PutOutcome::Stored => Ok(()),
                    PutOutcome::Exists => {
                        // A retry of the same generation: equal bytes are idempotent,
                        // different bytes are a conflict, never an overwrite.
                        let existing = store.get(object).await?;
                        if existing == chunk.bytes {
                            Ok(())
                        } else {
                            Err(CloudBackupError::ObjectConflict {
                                key: object.to_owned(),
                            })
                        }
                    }
                }
            }
            Self::Service {
                service,
                auth,
                ticket,
            } => {
                let ticket = ticket
                    .as_ref()
                    .ok_or_else(|| CloudBackupError::InvalidState {
                        backup_id: chunk.index.to_string(),
                        operation: "put_chunk",
                        reason: "the upload has not begun".to_owned(),
                    })?;
                service
                    .put_chunk(auth.as_ref(), ticket, chunk, object)
                    .await?;
                Ok(())
            }
        }
    }

    async fn commit(&mut self, manifest: &BackupManifest) -> Result<()> {
        match self {
            Self::Store { store } => put_manifest(*store, manifest).await,
            Self::Service {
                service,
                auth,
                ticket,
            } => {
                let ticket = ticket
                    .as_ref()
                    .ok_or_else(|| CloudBackupError::InvalidState {
                        backup_id: manifest.backup_id.clone(),
                        operation: "commit",
                        reason: "the upload has not begun".to_owned(),
                    })?;
                service.commit(auth.as_ref(), ticket, manifest).await?;
                Ok(())
            }
        }
    }

    async fn record(&mut self, manifest: &BackupManifest) -> Result<()> {
        match self {
            Self::Store { store } => put_manifest(*store, manifest).await,
            Self::Service { service, auth, .. } => {
                service.record_verification(auth.as_ref(), manifest).await?;
                Ok(())
            }
        }
    }
}

impl ObjectReader for Destination<'_> {
    fn read<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        match self {
            Self::Store { store } => store.get(key),
            Self::Service {
                service,
                auth,
                ticket,
            } => {
                let backup_id = ticket
                    .as_ref()
                    .map(|ticket| ticket.backup_id.as_str())
                    .unwrap_or("");
                service.read_object(auth.as_ref(), backup_id, key)
            }
        }
    }
}

/// Writes a manifest version, treating an identical existing version as success.
async fn put_manifest(store: &dyn ObjectStore, manifest: &BackupManifest) -> Result<()> {
    let key = manifest.key();
    let bytes = manifest.to_json()?.into_bytes();
    match store.put(&key, bytes.clone()).await? {
        PutOutcome::Stored => Ok(()),
        PutOutcome::Exists => {
            let existing = store.get(&key).await?;
            if existing == bytes {
                Ok(())
            } else {
                Err(CloudBackupError::ObjectConflict { key })
            }
        }
    }
}

async fn used_bytes(store: &dyn ObjectStore, tenant: &SiteId) -> Result<u64> {
    Ok(store
        .list(&BackupManifest::tenant_prefix(tenant.as_str()))
        .await?
        .iter()
        .map(|object| object.bytes)
        .sum())
}

/// One client-side cloud backup.
pub struct BackupSession<'a> {
    tenant: SiteId,
    recovery: RecoveryKeyProvider,
    source: &'a dyn BackupSource,
    destination: Destination<'a>,
    options: UploadOptions,
}

impl<'a> BackupSession<'a> {
    /// A session that writes straight to `store` (the filesystem store or a test store).
    pub fn new(
        tenant: SiteId,
        recovery: RecoveryKey,
        source: &'a dyn BackupSource,
        store: &'a dyn ObjectStore,
        options: UploadOptions,
    ) -> Self {
        Self {
            tenant,
            recovery: recovery.provider(),
            source,
            destination: Destination::Store { store },
            options,
        }
    }

    /// A session that uploads through `service` under `auth`.
    pub fn with_service(
        tenant: SiteId,
        recovery: RecoveryKey,
        source: &'a dyn BackupSource,
        service: &'a dyn BackupService,
        auth: TenantAuth,
        options: UploadOptions,
    ) -> Self {
        Self {
            tenant,
            recovery: recovery.provider(),
            source,
            destination: Destination::Service {
                service,
                auth: Box::new(auth),
                ticket: None,
            },
            options,
        }
    }

    /// The tenant this session backs up.
    pub fn tenant(&self) -> &SiteId {
        &self.tenant
    }

    /// Runs the backup: produce, seal, upload, commit, verify and repair.
    ///
    /// # Errors
    /// [`CloudBackupError::NotEntitled`] or [`CloudBackupError::QuotaExceeded`] before any
    /// object is written, [`CloudBackupError::Store`] for a transport failure, and
    /// [`CloudBackupError::ManifestInvalid`] for a bad artefact or chunk size.
    pub async fn run(&mut self, now: i64) -> Result<BackupOutcome> {
        let artefact = self.source.produce(&self.tenant, now).await?;
        let backup_id = backup_id(now)?;
        let data_key = DataKey::generate()?;
        let sealed = chunk::seal(
            &backup_id,
            &data_key,
            &artefact.bytes,
            self.options.chunk_size,
        )?;

        let mut keys = Vec::with_capacity(2);
        keys.push(
            wrap_data_key(
                &self.recovery,
                self.tenant.as_str(),
                &backup_id,
                "recovery",
                &data_key,
            )
            .await?,
        );
        if let (Some(provider), Some(label)) =
            (self.options.escrow.provider(), self.options.escrow.label())
        {
            keys.push(
                wrap_data_key(
                    provider.as_ref(),
                    self.tenant.as_str(),
                    &backup_id,
                    label,
                    &data_key,
                )
                .await?,
            );
        }

        let chunk_count = u32::try_from(sealed.len()).unwrap_or(u32::MAX);
        let cipher_bytes: u64 = sealed.iter().map(EncryptedChunk::cipher_len).sum();
        let request = UploadRequest {
            backup_id: backup_id.clone(),
            chunk_count,
            cipher_bytes,
        };
        self.destination
            .begin(&self.tenant, &request, &self.options.quota, now)
            .await?;

        // Generation 0 for every chunk.
        let mut generations: BTreeMap<u32, u32> = BTreeMap::new();
        for chunk in &sealed {
            generations.insert(chunk.index, 0);
            let object =
                BackupManifest::chunk_object(self.tenant.as_str(), &backup_id, chunk.index, 0);
            self.destination.put_chunk(chunk, &object).await?;
        }

        let entries = entries_for(&sealed, &self.tenant, &backup_id, &generations);
        let mut manifest = BackupManifest::new(
            &backup_id,
            self.tenant.as_str(),
            now,
            SourceInfo {
                name: artefact.name,
                bytes: u64::try_from(artefact.bytes.len()).unwrap_or(u64::MAX),
                sha256: crypto::sha256_hex(&artefact.bytes),
            },
            self.options.chunk_size,
            entries,
            keys,
        );
        manifest.sign(&data_key)?;
        let mut uploaded_bytes = cipher_bytes
            .saturating_add(u64::try_from(manifest.to_json()?.len()).unwrap_or(u64::MAX));
        self.destination.commit(&manifest).await?;

        let mut report =
            verify::verify(&self.destination, &manifest, &data_key, self.options.verify).await?;
        let mut retries = 0;
        while !report.is_ok() && retries < self.options.retries {
            retries += 1;
            for failure in &report.failures {
                let Some(chunk) = sealed.get(failure.index as usize) else {
                    continue;
                };
                let generation = generations.get(&chunk.index).copied().unwrap_or(0) + 1;
                let object = BackupManifest::chunk_object(
                    self.tenant.as_str(),
                    &backup_id,
                    chunk.index,
                    generation,
                );
                self.destination.put_chunk(chunk, &object).await?;
                generations.insert(chunk.index, generation);
                uploaded_bytes = uploaded_bytes.saturating_add(chunk.cipher_len());
            }
            let entries = entries_for(&sealed, &self.tenant, &backup_id, &generations);
            manifest.chunks = entries;
            report = verify::verify(&self.destination, &manifest, &data_key, self.options.verify)
                .await?;
        }

        manifest.version = manifest.version.saturating_add(1);
        manifest.state = report.state();
        manifest.sign(&data_key)?;
        uploaded_bytes = uploaded_bytes
            .saturating_add(u64::try_from(manifest.to_json()?.len()).unwrap_or(u64::MAX));
        self.destination.record(&manifest).await?;

        Ok(BackupOutcome {
            manifest,
            recovery_phrase: self.recovery.recovery_key().phrase(),
            verification: report,
            uploaded_bytes,
            retries,
        })
    }
}

fn entries_for(
    sealed: &[EncryptedChunk],
    tenant: &SiteId,
    backup_id: &str,
    generations: &BTreeMap<u32, u32>,
) -> Vec<ChunkEntry> {
    sealed
        .iter()
        .map(|chunk| ChunkEntry {
            index: chunk.index,
            offset: chunk.offset,
            plain_len: chunk.plain_len,
            cipher_len: chunk.cipher_len(),
            sha256: chunk.digest_hex(),
            object: BackupManifest::chunk_object(
                tenant.as_str(),
                backup_id,
                chunk.index,
                generations.get(&chunk.index).copied().unwrap_or(0),
            ),
        })
        .collect()
}

/// A backup id: the instant it started and a random suffix, unique per run.
const INSTANT: &[FormatItem<'static>] =
    format_description!("[year][month][day]T[hour][minute][second]Z");

fn backup_id(now: i64) -> Result<String> {
    let stamp = OffsetDateTime::from_unix_timestamp(now)
        .ok()
        .and_then(|instant| instant.format(INSTANT).ok())
        .unwrap_or_else(|| now.to_string());
    Ok(format!("bk-{stamp}-{}", crypto::random_hex(4)?))
}
