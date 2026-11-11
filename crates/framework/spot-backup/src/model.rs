//! The shapes a backup job, its target, its retention policy and its reports use.
//!
//! Time is always an argument ([`time::OffsetDateTime`]); nothing here reads a clock.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::retention::RetentionPolicy;

/// How thoroughly a backup is checked after it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// Manifest digest, byte size and package header only; cheap enough for every run.
    Quick,
    /// Restore into a scratch site, run schema checks and the L7 invariants; the
    /// acceptance path for a restorable backup.
    Full,
}

impl VerifyMode {
    /// The lower-case name the manifest records.
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyMode::Quick => "quick",
            VerifyMode::Full => "full",
        }
    }
}

impl fmt::Display for VerifyMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A key-encryption-key reference (`spot_secrets::KeyRef`'s shape).
///
/// Only the identifier and version travel in a manifest; the key itself never enters
/// this crate. The host maps the reference onto its key provider before handing the
/// stream to a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRef {
    /// Key id (a keyring name or a KMS key id).
    pub id: String,
    /// Key version, starting at 1.
    pub version: u32,
}

/// Where a backup is stored, as the job configuration names it.
///
/// The live [`crate::BackupTarget`] is constructed by the host from this reference;
/// the job never opens a device or a connection itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TargetRef {
    /// A directory on a fixed disk or a network share.
    Folder {
        /// The directory backups are written to.
        path: PathBuf,
    },
    /// A removable volume, mounted by the platform.
    Removable {
        /// The volume id the platform reports (Linux `/dev/sdb1`, Windows `E:`).
        volume_id: String,
        /// The directory below the mount point that holds the backups.
        subdir: String,
    },
    /// A cloud prefix, implemented by `spot-cloud-backup` (WP-CTL-08).
    Cloud {
        /// The prefix below the provider's bucket or container.
        prefix: String,
    },
}

impl fmt::Display for TargetRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetRef::Folder { path } => write!(formatter, "folder:{}", path.display()),
            TargetRef::Removable { volume_id, .. } => write!(formatter, "removable:{volume_id}"),
            TargetRef::Cloud { prefix } => write!(formatter, "cloud:{prefix}"),
        }
    }
}

/// One scheduled backup definition.
///
/// The schedule is a [`spot_scheduler::Schedule`], so it fires DST-correctly and its
/// [`spot_scheduler::RunSlot`] carries the idempotency key the job engine uses.
#[derive(Debug, Clone)]
pub struct BackupJob {
    /// The tenant the backup covers (one tenant = one site, ADR-34).
    pub tenant: String,
    /// The configured schedule.
    pub schedule: spot_scheduler::Schedule,
    /// The target reference recorded in the manifest.
    pub target: TargetRef,
    /// The grandfather-father-son retention policy.
    pub retention: RetentionPolicy,
    /// The key the host should encrypt the package with, when encryption is on.
    pub encrypt: Option<KeyRef>,
    /// How the written backup is verified.
    pub verify: VerifyMode,
    /// Stable id of the chain this backup belongs to (first backup's id).
    pub chain_id: String,
    /// Position in the chain, starting at 1.
    pub sequence: u64,
    /// Digest of the previous backup in the chain, when there is one.
    pub parent_digest: Option<String>,
}

/// A request the producer turns into a package. The runner builds it from the job and
/// the ledger, so a producer never invents a chain position or a time.
#[derive(Debug, Clone)]
pub struct BackupRequest {
    /// The job being run.
    pub job: BackupJob,
    /// The instant the run started (injected; never the wall clock).
    pub now: OffsetDateTime,
    /// The backup's id (`<tenant>-<chain>-<sequence>`); unique within a target.
    pub id: String,
    /// The tenant's database identity as the snapshot provider reported it.
    pub database: String,
}

/// A backup as a target lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBackup {
    /// The backup id.
    pub id: String,
    /// The manifest read from the package header.
    pub manifest: crate::manifest::BackupManifest,
    /// Total package bytes, integer.
    pub package_bytes: u64,
}

/// The state of one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetStatus {
    /// The target can accept a backup now.
    Ready,
    /// Removable media is not inserted; the job waits instead of failing.
    WaitingForMedia {
        /// The volume id that is absent.
        volume: String,
    },
    /// The target is misconfigured or unhealthy; a job fails.
    Unavailable {
        /// Plain-language reason.
        reason: String,
    },
}

/// What happened to one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupRunOutcome {
    /// A package was written and verified.
    Created {
        /// The stored backup (id, manifest, bytes).
        backup: Box<StoredBackup>,
        /// The verification that passed.
        verification: Box<crate::verify::VerifyReport>,
    },
    /// The target's media is absent; nothing failed and the job waits.
    WaitingForMedia {
        /// The absent volume.
        volume: String,
    },
}

/// A completed run. `retention` lists what the planner deleted afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRunReport {
    /// The run's outcome.
    pub outcome: BackupRunOutcome,
    /// Ids deleted by the retention plan (empty when nothing aged out).
    pub deleted: Vec<String>,
}

/// Version and size facts about a site, used by preflight checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteInfo {
    /// The site's tenant.
    pub tenant: String,
    /// The runtime schema version the target currently runs.
    pub schema_version: u64,
    /// PostgreSQL major version.
    pub pg_major: u32,
    /// Installed apps.
    pub app_set: Vec<String>,
    /// Free bytes on the volume that will receive the restore.
    pub free_space: u64,
    /// Whether the site holds no tenant data yet (a fresh machine).
    pub empty: bool,
}

/// A record the ledger and the retention planner share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupRecord {
    /// Backup id.
    pub id: String,
    /// When the package was created.
    pub created_at: OffsetDateTime,
    /// Whether the package passed its verification.
    pub verified: bool,
    /// Position in the backup chain.
    pub sequence: u64,
    /// Whether an open restore references this backup.
    pub open_restore: bool,
}

impl BackupRecord {
    /// A record from a manifest.
    pub fn from_manifest(
        manifest: &crate::manifest::BackupManifest,
        verified: bool,
        open_restore: bool,
    ) -> crate::error::Result<BackupRecord> {
        Ok(BackupRecord {
            id: manifest.id.clone(),
            created_at: manifest.created_at()?,
            verified,
            sequence: manifest.chain_sequence,
            open_restore,
        })
    }
}
