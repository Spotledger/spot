//! The service side: entitlement, quota, immutable objects, listing and audited deletion.
//!
//! The cloud service stores **opaque chunks**: it can see sizes, digests, a tenant id and
//! a backup id, and nothing else. It cannot decrypt a backup, because the data key is
//! wrapped under the customer's recovery key and (optionally) an escrow key it does not
//! hold.
//!
//! [`BackupService`] is the port. [`InMemoryBackupService`] is the acceptance
//! implementation over any [`ObjectStore`]; an S3-compatible store is an adapter behind
//! a feature, because the trait and the filesystem store are enough to restore.
//!
//! Rules enforced here:
//!
//! - **entitlement:** [`TenantAuth`] carries a verified licence; the tenant must hold the
//!   `cloud_backup` feature and the licence must allow the platform at the request's
//!   instant;
//! - **quota:** [`StorageQuota`] is a `spot-metering` limit for
//!   [`Meter::StorageBytes`](spot_metering::Meter::StorageBytes), checked at the start of
//!   an upload and again for every chunk;
//! - **immutability:** an object key is written once; a repeated upload of the same bytes
//!   is idempotent, different bytes are a conflict;
//! - **deletion only through retention:** [`BackupService::delete`] requires a
//!   [`DeletionPermit`], which only a [`RetentionPlan`](crate::retention::RetentionPlan)
//!   mints, and records a [`DeletionAudit`] before removing anything.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use spot_core::SiteId;
use spot_license::{Action, Licence};
use spot_metering::{Meter, MeterLimit};

use crate::chunk::EncryptedChunk;
use crate::crypto;
use crate::error::{CloudBackupError, Result};
use crate::manifest::{BackupManifest, BackupState, is_object_segment};
use crate::store::{ObjectStore, PutOutcome};

/// The licence feature that turns cloud backup on.
pub const CLOUD_BACKUP_FEATURE: &str = "cloud_backup";

/// A boxed `Send` future, so [`BackupService`] stays object-safe.
pub type ServiceFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// The tenant's storage quota, as a metering limit for storage bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorageQuota {
    limit: MeterLimit,
}

impl StorageQuota {
    /// A quota with `limit` bytes, as the licence or the tenant policy granted.
    pub fn new(limit: MeterLimit) -> Self {
        Self { limit }
    }

    /// No quota.
    pub fn unlimited() -> Self {
        Self {
            limit: MeterLimit::Unlimited,
        }
    }

    /// A quota of exactly `bytes` bytes.
    pub fn from_bytes(bytes: u64) -> Self {
        Self {
            limit: MeterLimit::UpTo(bytes),
        }
    }

    /// The meter this quota applies to (always storage bytes).
    pub fn meter(&self) -> Meter {
        Meter::StorageBytes
    }

    /// The limit in bytes, `u64::MAX` when unlimited.
    pub fn limit_bytes(&self) -> u64 {
        match self.limit {
            MeterLimit::UpTo(bytes) => bytes,
            MeterLimit::Unlimited => u64::MAX,
        }
    }

    /// How many bytes remain after `used`, `u64::MAX` when unlimited.
    pub fn remaining(&self, used: u64) -> u64 {
        match self.limit {
            MeterLimit::UpTo(bytes) => bytes.saturating_sub(used),
            MeterLimit::Unlimited => u64::MAX,
        }
    }

    /// Checks that `needed` more bytes fit after `used`.
    ///
    /// # Errors
    /// [`CloudBackupError::QuotaExceeded`] with the needed bytes, the remaining bytes and
    /// the limit.
    pub fn check(&self, used: u64, needed: u64) -> Result<()> {
        let required = used.saturating_add(needed);
        match self.limit {
            MeterLimit::Unlimited => Ok(()),
            MeterLimit::UpTo(limit) if required <= limit => Ok(()),
            MeterLimit::UpTo(limit) => Err(CloudBackupError::QuotaExceeded {
                needed,
                remaining: limit.saturating_sub(used),
                limit,
            }),
        }
    }
}

/// A verified tenant licence, checked once when the request is built.
///
/// The licence is verified offline elsewhere (WP-CTL-04); this type proves the holder is
/// the licence's tenant, the feature is granted and the licence allows the platform at
/// `now` (Unix seconds, passed in — the service never reads the clock).
#[derive(Clone, Debug)]
pub struct TenantAuth {
    tenant: SiteId,
    licence: Licence,
    now: i64,
}

impl TenantAuth {
    /// Builds the auth for one request.
    ///
    /// # Errors
    /// [`CloudBackupError::NotEntitled`] when the licence belongs to another tenant,
    /// does not allow the platform at `now`, or does not grant `cloud_backup`.
    pub fn new(tenant: SiteId, licence: Licence, now: i64) -> Result<Self> {
        let auth = Self {
            tenant,
            licence,
            now,
        };
        auth.authorize()?;
        Ok(auth)
    }

    /// The tenant.
    pub fn tenant(&self) -> &SiteId {
        &self.tenant
    }

    /// The verified licence.
    pub fn licence(&self) -> &Licence {
        &self.licence
    }

    /// The instant this request was authorised at.
    pub fn now(&self) -> i64 {
        self.now
    }

    /// Re-checks the entitlement (cheap, and every service call does it).
    ///
    /// # Errors
    /// [`CloudBackupError::NotEntitled`] as in [`TenantAuth::new`].
    pub fn authorize(&self) -> Result<()> {
        let not_entitled = || CloudBackupError::NotEntitled {
            tenant: self.tenant.as_str().to_owned(),
        };
        if self.licence.tenant.as_deref() != Some(self.tenant.as_str()) {
            return Err(not_entitled());
        }
        let status = spot_license::evaluate(&self.licence, self.now, None);
        if !status.allows(Action::UsePlatform).is_allow() {
            return Err(not_entitled());
        }
        if !self
            .licence
            .entitlements
            .features()
            .iter()
            .any(|feature| feature == CLOUD_BACKUP_FEATURE)
        {
            return Err(not_entitled());
        }
        Ok(())
    }
}

/// What a client asks to upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UploadRequest {
    /// The backup id the client chose.
    pub backup_id: String,
    /// How many chunks the backup has.
    pub chunk_count: u32,
    /// The total ciphertext bytes of the chunks.
    pub cipher_bytes: u64,
}

/// The service's acceptance of one upload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupTicket {
    /// The tenant.
    pub tenant: SiteId,
    /// The backup id.
    pub backup_id: String,
    /// How many chunks the upload will deliver.
    pub chunk_count: u32,
    /// The ciphertext bytes the upload will deliver.
    pub cipher_bytes: u64,
    /// When the upload started, Unix seconds.
    pub started_at: i64,
}

/// What one chunk write did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkReceipt {
    /// The chunk index.
    pub index: u32,
    /// `true` when this call wrote the object; `false` for an identical retry.
    pub stored: bool,
    /// The chunk's ciphertext length.
    pub cipher_bytes: u64,
}

/// A backup as the service lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBackup {
    /// The backup id.
    pub backup_id: String,
    /// When the backup was taken, Unix seconds.
    pub created_at: i64,
    /// The state recorded by the newest manifest version.
    pub state: BackupState,
    /// How many chunks it holds.
    pub chunk_count: u32,
    /// Ciphertext bytes.
    pub cipher_bytes: u64,
    /// The newest manifest version.
    pub manifest_version: u32,
}

/// One deletion, as the audit log records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionAudit {
    /// The tenant.
    pub tenant: String,
    /// The backup that was deleted.
    pub backup_id: String,
    /// Who deleted it.
    pub actor: String,
    /// Why (the retention decision).
    pub reason: String,
    /// The retention plan that permitted it.
    pub plan: String,
    /// When, Unix seconds.
    pub at: i64,
}

/// Where deletions are audited. A host wires this to its audit chain.
pub trait AuditSink: Send + Sync {
    /// Records one deletion. The service calls this **before** removing bytes.
    fn record(&self, event: &DeletionAudit);
}

/// An in-memory audit sink, for hosts whose audit chain is not wired yet and for tests.
#[derive(Debug, Default)]
pub struct RecordingAudit {
    events: Mutex<Vec<DeletionAudit>>,
}

impl RecordingAudit {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every recorded event, in order.
    pub fn events(&self) -> Vec<DeletionAudit> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl AuditSink for RecordingAudit {
    fn record(&self, event: &DeletionAudit) {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event.clone());
    }
}

/// Permission to delete one backup, minted only by a retention plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeletionPermit {
    pub(crate) tenant: String,
    pub(crate) backup_id: String,
    pub(crate) plan: String,
}

impl DeletionPermit {
    /// The backup this permit names.
    pub fn backup_id(&self) -> &str {
        &self.backup_id
    }

    /// The plan that minted it.
    pub fn plan(&self) -> &str {
        &self.plan
    }
}

/// The cloud backup service: auth, quota, immutable storage, listing and audited delete.
pub trait BackupService: Send + Sync {
    /// Accepts an upload after checking the entitlement and the quota.
    ///
    /// # Errors
    /// [`CloudBackupError::NotEntitled`], [`CloudBackupError::QuotaExceeded`],
    /// [`CloudBackupError::InvalidState`] for a backup id already in flight.
    fn begin<'a>(
        &'a self,
        auth: &'a TenantAuth,
        request: &'a UploadRequest,
        now: i64,
    ) -> ServiceFuture<'a, BackupTicket>;

    /// Stores one sealed chunk under `object`.
    ///
    /// # Errors
    /// [`CloudBackupError::InvalidState`] for an unknown ticket, a committed backup or a
    /// key that does not name this chunk; [`CloudBackupError::ChunkCorrupt`] when the
    /// bytes do not match their digest; [`CloudBackupError::ObjectConflict`] for
    /// different bytes under the same key.
    fn put_chunk<'a>(
        &'a self,
        auth: &'a TenantAuth,
        ticket: &'a BackupTicket,
        chunk: &'a EncryptedChunk,
        object: &'a str,
    ) -> ServiceFuture<'a, ChunkReceipt>;

    /// Checks a completed upload and stores manifest version 1.
    ///
    /// # Errors
    /// [`CloudBackupError::InvalidState`] when chunks are missing or already committed,
    /// [`CloudBackupError::ManifestInvalid`] when the manifest does not describe what
    /// was uploaded.
    fn commit<'a>(
        &'a self,
        auth: &'a TenantAuth,
        ticket: &'a BackupTicket,
        manifest: &'a BackupManifest,
    ) -> ServiceFuture<'a, StoredBackup>;

    /// Stores the client's verification result as the next immutable manifest version.
    ///
    /// # Errors
    /// [`CloudBackupError::InvalidState`] when the version is not the next one or the
    /// backup is unknown, and [`CloudBackupError::ChunkCorrupt`] when a manifest marked
    /// verified names a chunk the service cannot confirm.
    fn record_verification<'a>(
        &'a self,
        auth: &'a TenantAuth,
        manifest: &'a BackupManifest,
    ) -> ServiceFuture<'a, StoredBackup>;

    /// Lists the tenant's backups, newest first.
    ///
    /// # Errors
    /// [`CloudBackupError::NotEntitled`] or [`CloudBackupError::Store`].
    fn list<'a>(&'a self, auth: &'a TenantAuth) -> ServiceFuture<'a, Vec<StoredBackup>>;

    /// Reads the newest manifest of one backup.
    ///
    /// # Errors
    /// [`CloudBackupError::NotFound`] when the tenant has no such backup.
    fn manifest<'a>(
        &'a self,
        auth: &'a TenantAuth,
        backup_id: &'a str,
    ) -> ServiceFuture<'a, BackupManifest>;

    /// Reads one object of one backup, scoped to the tenant.
    ///
    /// # Errors
    /// [`CloudBackupError::NotFound`] for a key outside the backup, and
    /// [`CloudBackupError::Store`] when the object is absent.
    fn read_object<'a>(
        &'a self,
        auth: &'a TenantAuth,
        backup_id: &'a str,
        key: &'a str,
    ) -> ServiceFuture<'a, Vec<u8>>;

    /// Deletes a backup the retention plan permitted, after auditing it.
    ///
    /// # Errors
    /// [`CloudBackupError::DeleteRefused`] when the permit names another tenant or an
    /// unknown backup.
    fn delete<'a>(
        &'a self,
        auth: &'a TenantAuth,
        permit: &'a DeletionPermit,
        actor: &'a str,
        at: i64,
    ) -> ServiceFuture<'a, ()>;

    /// The tenant's stored bytes.
    ///
    /// # Errors
    /// [`CloudBackupError::NotEntitled`] or [`CloudBackupError::Store`].
    fn used_bytes<'a>(&'a self, auth: &'a TenantAuth) -> ServiceFuture<'a, u64>;
}

/// The acceptance service: auth, quota and audited deletion over any [`ObjectStore`].
pub struct InMemoryBackupService {
    store: Arc<dyn ObjectStore>,
    quota: StorageQuota,
    audit: Arc<dyn AuditSink>,
    tickets: Mutex<BTreeMap<String, TicketState>>,
}

struct TicketState {
    tenant: String,
    chunk_count: u32,
    expected_bytes: u64,
    received: BTreeMap<u32, ReceivedChunk>,
    committed: bool,
}

#[derive(Clone)]
struct ReceivedChunk {
    object: String,
    digest: String,
    cipher_len: u64,
}

impl InMemoryBackupService {
    /// A service over `store`, with `quota` and an audit sink.
    pub fn new(
        store: Arc<dyn ObjectStore>,
        quota: StorageQuota,
        audit: Arc<dyn AuditSink>,
    ) -> Self {
        Self {
            store,
            quota,
            audit,
            tickets: Mutex::new(BTreeMap::new()),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, TicketState>> {
        self.tickets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn tenant_bytes(&self, tenant: &SiteId) -> Result<u64> {
        Ok(self
            .store
            .list(&BackupManifest::tenant_prefix(tenant.as_str()))
            .await?
            .iter()
            .map(|object| object.bytes)
            .sum())
    }

    async fn latest(&self, tenant: &str, backup_id: &str) -> Result<Option<BackupManifest>> {
        latest_manifest(self.store.as_ref(), tenant, backup_id).await
    }

    async fn require(&self, auth: &TenantAuth, backup_id: &str) -> Result<BackupManifest> {
        self.latest(auth.tenant().as_str(), backup_id)
            .await?
            .ok_or_else(|| CloudBackupError::NotFound {
                backup_id: backup_id.to_owned(),
                tenant: auth.tenant().as_str().to_owned(),
            })
    }

    async fn store_manifest(&self, manifest: &BackupManifest) -> Result<()> {
        let key = manifest.key();
        let bytes = manifest.to_json()?.into_bytes();
        match self.store.put(&key, bytes.clone()).await? {
            PutOutcome::Stored => Ok(()),
            PutOutcome::Exists => {
                let existing = self.store.get(&key).await?;
                if existing == bytes {
                    Ok(())
                } else {
                    Err(CloudBackupError::ObjectConflict { key })
                }
            }
        }
    }

    fn check_manifest_against_receipts(
        &self,
        backup_id: &str,
        manifest: &BackupManifest,
    ) -> Result<()> {
        let tickets = self.guard();
        let state = tickets
            .get(&ticket_key(&manifest.tenant, backup_id))
            .ok_or_else(|| CloudBackupError::InvalidState {
                backup_id: backup_id.to_owned(),
                operation: "commit",
                reason: "the upload has not begun".to_owned(),
            })?;
        if state.received.len() != usize::try_from(state.chunk_count).unwrap_or(usize::MAX) {
            return Err(CloudBackupError::InvalidState {
                backup_id: backup_id.to_owned(),
                operation: "commit",
                reason: format!(
                    "{} of {} chunks were uploaded",
                    state.received.len(),
                    state.chunk_count
                ),
            });
        }
        let uploaded: u64 = state.received.values().map(|chunk| chunk.cipher_len).sum();
        if uploaded != state.expected_bytes || manifest.cipher_bytes() != state.expected_bytes {
            return Err(CloudBackupError::ManifestInvalid {
                backup_id: backup_id.to_owned(),
                reason: "the manifest does not match the uploaded bytes".to_owned(),
            });
        }
        for entry in &manifest.chunks {
            let Some(received) = state.received.get(&entry.index) else {
                return Err(CloudBackupError::InvalidState {
                    backup_id: backup_id.to_owned(),
                    operation: "commit",
                    reason: format!("chunk {} was not uploaded", entry.index),
                });
            };
            if received.digest != entry.sha256
                || received.cipher_len != entry.cipher_len
                || received.object != entry.object
            {
                return Err(CloudBackupError::ManifestInvalid {
                    backup_id: backup_id.to_owned(),
                    reason: format!("chunk {} does not match what was uploaded", entry.index),
                });
            }
        }
        Ok(())
    }
}

impl BackupService for InMemoryBackupService {
    fn begin<'a>(
        &'a self,
        auth: &'a TenantAuth,
        request: &'a UploadRequest,
        now: i64,
    ) -> ServiceFuture<'a, BackupTicket> {
        Box::pin(async move {
            auth.authorize()?;
            if !is_object_segment(&request.backup_id) {
                return Err(CloudBackupError::InvalidState {
                    backup_id: request.backup_id.clone(),
                    operation: "begin",
                    reason: "the backup id is not usable as an object key".to_owned(),
                });
            }
            let minimum = u64::from(request.chunk_count)
                .saturating_mul(u64::try_from(crypto::NONCE_LEN + crypto::TAG_LEN).unwrap_or(0));
            if request.chunk_count == 0 || request.cipher_bytes < minimum {
                return Err(CloudBackupError::InvalidState {
                    backup_id: request.backup_id.clone(),
                    operation: "begin",
                    reason: "the upload declares no chunks or too few bytes".to_owned(),
                });
            }
            if self
                .guard()
                .contains_key(&ticket_key(auth.tenant().as_str(), &request.backup_id))
                || self
                    .latest(auth.tenant().as_str(), &request.backup_id)
                    .await?
                    .is_some()
            {
                return Err(CloudBackupError::InvalidState {
                    backup_id: request.backup_id.clone(),
                    operation: "begin",
                    reason: "a backup with this id already exists; ids are never reused".to_owned(),
                });
            }
            let used = self.tenant_bytes(auth.tenant()).await?;
            self.quota.check(used, request.cipher_bytes)?;
            self.guard().insert(
                ticket_key(auth.tenant().as_str(), &request.backup_id),
                TicketState {
                    tenant: auth.tenant().as_str().to_owned(),
                    chunk_count: request.chunk_count,
                    expected_bytes: request.cipher_bytes,
                    received: BTreeMap::new(),
                    committed: false,
                },
            );
            Ok(BackupTicket {
                tenant: auth.tenant().clone(),
                backup_id: request.backup_id.clone(),
                chunk_count: request.chunk_count,
                cipher_bytes: request.cipher_bytes,
                started_at: now,
            })
        })
    }

    fn put_chunk<'a>(
        &'a self,
        auth: &'a TenantAuth,
        ticket: &'a BackupTicket,
        chunk: &'a EncryptedChunk,
        object: &'a str,
    ) -> ServiceFuture<'a, ChunkReceipt> {
        Box::pin(async move {
            auth.authorize()?;
            let tenant = auth.tenant().as_str().to_owned();
            {
                let tickets = self.guard();
                let state = tickets
                    .get(&ticket_key(&tenant, &ticket.backup_id))
                    .ok_or_else(|| CloudBackupError::InvalidState {
                        backup_id: ticket.backup_id.clone(),
                        operation: "put_chunk",
                        reason: "the upload has not begun".to_owned(),
                    })?;
                if state.tenant != tenant || ticket.tenant.as_str() != tenant {
                    return Err(CloudBackupError::NotFound {
                        backup_id: ticket.backup_id.clone(),
                        tenant,
                    });
                }
                if state.committed {
                    return Err(CloudBackupError::InvalidState {
                        backup_id: ticket.backup_id.clone(),
                        operation: "put_chunk",
                        reason: "the backup is already committed".to_owned(),
                    });
                }
                if chunk.index >= state.chunk_count {
                    return Err(CloudBackupError::InvalidState {
                        backup_id: ticket.backup_id.clone(),
                        operation: "put_chunk",
                        reason: "the chunk index is past the declared chunk count".to_owned(),
                    });
                }
            }
            let expected = format!("{}/{}/chunk-{:08}-g", tenant, ticket.backup_id, chunk.index);
            if !object.starts_with(&expected)
                || !object.ends_with(".bin")
                || object
                    .get(expected.len()..)
                    .is_none_or(|rest| rest.contains('/'))
            {
                return Err(CloudBackupError::InvalidState {
                    backup_id: ticket.backup_id.clone(),
                    operation: "put_chunk",
                    reason: "the object key does not name this chunk".to_owned(),
                });
            }
            if crypto::sha256(&chunk.bytes) != chunk.digest {
                return Err(CloudBackupError::ChunkCorrupt {
                    backup_id: ticket.backup_id.clone(),
                    index: chunk.index,
                    reason: "the bytes do not match the chunk digest",
                });
            }
            let used = self.tenant_bytes(auth.tenant()).await?;
            if self.store.put(object, chunk.bytes.clone()).await? == PutOutcome::Exists {
                let existing = self.store.get(object).await?;
                if existing != chunk.bytes {
                    return Err(CloudBackupError::ObjectConflict {
                        key: object.to_owned(),
                    });
                }
                return Ok(ChunkReceipt {
                    index: chunk.index,
                    stored: false,
                    cipher_bytes: chunk.cipher_len(),
                });
            }
            if let Err(error) = self.quota.check(used, chunk.cipher_len()) {
                // Enforce the quota before the chunk counts: a rejected write leaves the
                // tenant's usage exactly where it was.
                self.store.delete(object).await?;
                return Err(error);
            }
            if let Some(state) = self
                .guard()
                .get_mut(&ticket_key(&tenant, &ticket.backup_id))
            {
                state.received.insert(
                    chunk.index,
                    ReceivedChunk {
                        object: object.to_owned(),
                        digest: chunk.digest_hex(),
                        cipher_len: chunk.cipher_len(),
                    },
                );
            }
            Ok(ChunkReceipt {
                index: chunk.index,
                stored: true,
                cipher_bytes: chunk.cipher_len(),
            })
        })
    }

    fn commit<'a>(
        &'a self,
        auth: &'a TenantAuth,
        ticket: &'a BackupTicket,
        manifest: &'a BackupManifest,
    ) -> ServiceFuture<'a, StoredBackup> {
        Box::pin(async move {
            auth.authorize()?;
            let tenant = auth.tenant().as_str().to_owned();
            {
                let tickets = self.guard();
                let state = tickets
                    .get(&ticket_key(&tenant, &ticket.backup_id))
                    .ok_or_else(|| CloudBackupError::InvalidState {
                        backup_id: ticket.backup_id.clone(),
                        operation: "commit",
                        reason: "the upload has not begun".to_owned(),
                    })?;
                if state.tenant != tenant {
                    return Err(CloudBackupError::NotFound {
                        backup_id: ticket.backup_id.clone(),
                        tenant,
                    });
                }
                if state.committed {
                    return Err(CloudBackupError::InvalidState {
                        backup_id: ticket.backup_id.clone(),
                        operation: "commit",
                        reason: "the backup is already committed".to_owned(),
                    });
                }
            }
            manifest.validate()?;
            if manifest.backup_id != ticket.backup_id
                || manifest.tenant != tenant
                || manifest.version != 1
                || manifest.state != BackupState::Pending
            {
                return Err(CloudBackupError::ManifestInvalid {
                    backup_id: ticket.backup_id.clone(),
                    reason: "the manifest is not the pending version 1 of this upload".to_owned(),
                });
            }
            self.check_manifest_against_receipts(&ticket.backup_id, manifest)?;
            self.store_manifest(manifest).await?;
            if let Some(state) = self
                .guard()
                .get_mut(&ticket_key(&tenant, &ticket.backup_id))
            {
                state.committed = true;
            }
            Ok(stored(manifest))
        })
    }

    fn record_verification<'a>(
        &'a self,
        auth: &'a TenantAuth,
        manifest: &'a BackupManifest,
    ) -> ServiceFuture<'a, StoredBackup> {
        Box::pin(async move {
            auth.authorize()?;
            let tenant = auth.tenant().as_str().to_owned();
            if manifest.tenant != tenant {
                return Err(CloudBackupError::NotFound {
                    backup_id: manifest.backup_id.clone(),
                    tenant,
                });
            }
            manifest.validate()?;
            let current = self.require(auth, &manifest.backup_id).await?;
            if manifest.version != current.version.saturating_add(1) {
                return Err(CloudBackupError::InvalidState {
                    backup_id: manifest.backup_id.clone(),
                    operation: "record_verification",
                    reason: "the manifest is not the next version".to_owned(),
                });
            }
            if manifest.state == BackupState::Pending {
                return Err(CloudBackupError::InvalidState {
                    backup_id: manifest.backup_id.clone(),
                    operation: "record_verification",
                    reason: "a verification result is verified or unverified".to_owned(),
                });
            }
            if manifest.state.is_verified() {
                for entry in &manifest.chunks {
                    let bytes = self.store.get(&entry.object).await?;
                    if crypto::sha256_hex(&bytes) != entry.sha256 {
                        return Err(CloudBackupError::ChunkCorrupt {
                            backup_id: manifest.backup_id.clone(),
                            index: entry.index,
                            reason: "the service cannot confirm the chunk digest",
                        });
                    }
                }
            }
            self.store_manifest(manifest).await?;
            Ok(stored(manifest))
        })
    }

    fn list<'a>(&'a self, auth: &'a TenantAuth) -> ServiceFuture<'a, Vec<StoredBackup>> {
        Box::pin(async move {
            auth.authorize()?;
            let tenant = auth.tenant().as_str().to_owned();
            let objects = self
                .store
                .list(&BackupManifest::tenant_prefix(&tenant))
                .await?;
            let mut ids = BTreeSet::new();
            for object in objects {
                if let Some(id) = backup_id_of_key(&tenant, &object.key) {
                    ids.insert(id);
                }
            }
            let mut backups = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(manifest) = self.latest(&tenant, &id).await? {
                    backups.push(stored(&manifest));
                }
            }
            backups.sort_by(|left, right| {
                right
                    .created_at
                    .cmp(&left.created_at)
                    .then_with(|| left.backup_id.cmp(&right.backup_id))
            });
            Ok(backups)
        })
    }

    fn manifest<'a>(
        &'a self,
        auth: &'a TenantAuth,
        backup_id: &'a str,
    ) -> ServiceFuture<'a, BackupManifest> {
        Box::pin(async move {
            auth.authorize()?;
            self.require(auth, backup_id).await
        })
    }

    fn read_object<'a>(
        &'a self,
        auth: &'a TenantAuth,
        backup_id: &'a str,
        key: &'a str,
    ) -> ServiceFuture<'a, Vec<u8>> {
        Box::pin(async move {
            auth.authorize()?;
            let tenant = auth.tenant().as_str();
            let prefix = BackupManifest::backup_prefix(tenant, backup_id);
            if !is_object_segment(backup_id) || !key.starts_with(&prefix) {
                return Err(CloudBackupError::NotFound {
                    backup_id: backup_id.to_owned(),
                    tenant: tenant.to_owned(),
                });
            }
            self.store.get(key).await
        })
    }

    fn delete<'a>(
        &'a self,
        auth: &'a TenantAuth,
        permit: &'a DeletionPermit,
        actor: &'a str,
        at: i64,
    ) -> ServiceFuture<'a, ()> {
        Box::pin(async move {
            auth.authorize()?;
            let tenant = auth.tenant().as_str().to_owned();
            if permit.tenant != tenant {
                return Err(CloudBackupError::DeleteRefused {
                    backup_id: permit.backup_id.clone(),
                    reason: "the permit belongs to another tenant".to_owned(),
                });
            }
            let Ok(current) = self.require(auth, &permit.backup_id).await else {
                return Err(CloudBackupError::DeleteRefused {
                    backup_id: permit.backup_id.clone(),
                    reason: "the backup does not exist".to_owned(),
                });
            };
            // A permit is a plan snapshot: the service itself keeps the last verified backup.
            let last_verified =
                current.state.is_verified()
                    && !self.list(auth).await?.iter().any(|other| {
                        other.backup_id != permit.backup_id && other.state.is_verified()
                    });
            self.audit.record(&DeletionAudit {
                tenant: tenant.clone(),
                backup_id: permit.backup_id.clone(),
                actor: actor.to_owned(),
                reason: if last_verified {
                    "refused: the last verified backup is never deleted"
                } else {
                    "retention plan"
                }
                .to_owned(),
                plan: permit.plan.clone(),
                at,
            });
            if last_verified {
                return Err(CloudBackupError::DeleteRefused {
                    backup_id: permit.backup_id.clone(),
                    reason: "it is the last verified backup".to_owned(),
                });
            }
            let prefix = BackupManifest::backup_prefix(&tenant, &permit.backup_id);
            for object in self.store.list(&prefix).await? {
                self.store.delete(&object.key).await?;
            }
            Ok(())
        })
    }

    fn used_bytes<'a>(&'a self, auth: &'a TenantAuth) -> ServiceFuture<'a, u64> {
        Box::pin(async move {
            auth.authorize()?;
            self.tenant_bytes(auth.tenant()).await
        })
    }
}

/// The newest manifest version of one backup, if any.
/// The ticket map key: per tenant, so identical backup ids of two tenants never meet.
fn ticket_key(tenant: &str, backup_id: &str) -> String {
    format!("{tenant}/{backup_id}")
}

async fn latest_manifest(
    store: &dyn ObjectStore,
    tenant: &str,
    backup_id: &str,
) -> Result<Option<BackupManifest>> {
    let prefix = BackupManifest::backup_prefix(tenant, backup_id);
    let objects = store.list(&prefix).await?;
    let newest = objects
        .iter()
        .filter_map(|object| manifest_version_of_key(tenant, backup_id, &object.key))
        .max();
    let Some(version) = newest else {
        return Ok(None);
    };
    let key = BackupManifest::manifest_key(tenant, backup_id, version);
    let bytes = store.get(&key).await?;
    let text = String::from_utf8(bytes).map_err(|_| CloudBackupError::ManifestInvalid {
        backup_id: backup_id.to_owned(),
        reason: "the stored manifest is not UTF-8".to_owned(),
    })?;
    Ok(Some(BackupManifest::from_json(&text)?))
}

/// The version a manifest object key names, when the key is that backup's.
fn manifest_version_of_key(tenant: &str, backup_id: &str, key: &str) -> Option<u32> {
    let prefix = BackupManifest::backup_prefix(tenant, backup_id);
    let name = key.strip_prefix(&prefix)?;
    let digits = name.strip_prefix("manifest-")?.strip_suffix(".json")?;
    if digits.len() != 4 {
        return None;
    }
    digits.parse().ok()
}

/// The backup id a key belongs to, when it is a direct child of the tenant.
fn backup_id_of_key(tenant: &str, key: &str) -> Option<String> {
    let rest = key.strip_prefix(&BackupManifest::tenant_prefix(tenant))?;
    let mut parts = rest.split('/');
    let id = parts.next()?;
    if parts.next().is_none() || !is_object_segment(id) {
        return None;
    }
    Some(id.to_owned())
}

fn stored(manifest: &BackupManifest) -> StoredBackup {
    StoredBackup {
        backup_id: manifest.backup_id.clone(),
        created_at: manifest.created_at,
        state: manifest.state.clone(),
        chunk_count: u32::try_from(manifest.chunks.len()).unwrap_or(u32::MAX),
        cipher_bytes: manifest.cipher_bytes(),
        manifest_version: manifest.version,
    }
}
