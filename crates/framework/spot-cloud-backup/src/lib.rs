//! Client-side encrypted cloud backups for desktop tenants, and restore on a new PC
//! (blueprint WS18, WP-CTL-08).
//!
//! The customer's data never leaves their machine in the clear. A [`BackupSession`] takes
//! an artefact from the scheduled backup runner through [`BackupSource`], splits it into
//! fixed-size chunks (4 MiB to start), seals each chunk with XChaCha20-Poly1305 under a
//! fresh per-backup data key, and uploads the ciphertext through an [`ObjectStore`] or
//! the [`BackupService`] port. The data key is wrapped under the tenant **recovery key**
//! through the `spot-secrets` envelope; the service holds only the wrapped key. A
//! recovery phrase, printed once, is the only thing a new PC needs to restore.
//!
//! # The pieces
//!
//! - [`recovery`]: the tenant recovery key, its checksummed phrase, and the optional,
//!   explicit escrow wrap;
//! - [`chunk`]: fixed-size chunking, sealing and opening one chunk;
//! - [`manifest`]: the signed manifest — chunk digests, state, wrapped keys — and the
//!   immutable object key layout;
//! - [`session`]: produce, seal, upload, commit, verify and repair one backup;
//! - [`verify`]: sample or full re-reads that check digests and authentication tags;
//! - [`service`]: the service port with licence entitlement, the storage quota
//!   (`spot-metering`), immutable objects, listing and audited deletion, plus the
//!   in-memory acceptance service;
//! - [`store`]: the object store port with in-memory, recording and filesystem stores;
//! - [`retention`]: the pure daily/weekly/monthly planner and the audited executor;
//! - [`restore`]: restore from the recovery key alone, with chunk-level resume, handing
//!   the verified artefact to a [`Restorer`] (`spot-cli restore`).
//!
//! # What the service can see
//!
//! Nothing but opaque bytes: object keys (tenant, backup id, chunk index and generation),
//! ciphertext sizes, ciphertext digests and the wrapped data key. It cannot decrypt a
//! chunk, and no key, phrase or plaintext ever crosses the port — the test suite proves
//! it with a recording store and a canary plaintext.
//!
//! # Rules
//!
//! Time is always an argument (no function reads the clock); sizes are integers (no
//! floats); no `unsafe`; a key, a phrase and plaintext are never logged or returned in a
//! message. The object layout is append-only: a repaired chunk is a new generation and a
//! state change is a new manifest version.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
// Unit tests may unwrap what the test itself built; production code may not.
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used, clippy::panic))]

pub mod chunk;
pub mod crypto;
pub mod error;
pub mod manifest;
pub mod recovery;
pub mod restore;
pub mod retention;
pub mod service;
pub mod session;
pub mod store;
pub mod verify;

pub use chunk::{CHUNK_SIZE, ChunkRange, EncryptedChunk};
pub use error::{CloudBackupError, Result};
pub use manifest::{
    BackupManifest, BackupState, ChunkEntry, MANIFEST_FORMAT, ManifestKey, ManifestSignature,
    SIGNATURE_ALGORITHM, SourceInfo,
};
pub use recovery::{Escrow, RECOVERY_KEY_ID, RecoveryKey, RecoveryKeyProvider, RecoveryPhrase};
pub use restore::{
    RestoreCheckpoint, RestoreOptions, RestoredArtefact, Restorer, restore, restore_into,
    restore_with_key,
};
pub use retention::{
    BackupSummary, KeepReason, KeptBackup, RetentionPlan, RetentionPolicy, execute_retention,
    plan_retention,
};
pub use service::{
    AuditSink, BackupService, BackupTicket, CLOUD_BACKUP_FEATURE, ChunkReceipt, DeletionAudit,
    DeletionPermit, InMemoryBackupService, RecordingAudit, StorageQuota, StoredBackup, TenantAuth,
    UploadRequest,
};
pub use session::{
    Artefact, BackupOutcome, BackupSession, BackupSource, FixedSource, UploadOptions,
};
pub use store::{
    FileSystemStore, MemoryObjectStore, ObjectInfo, ObjectReader, ObjectStore, PutOutcome,
    RecordedOp, RecordingStore, StoreFuture,
};
pub use verify::{
    ChunkFailure, ChunkFailureReason, VerifyMode, VerifyReport, verify as verify_backup,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-cloud-backup";
