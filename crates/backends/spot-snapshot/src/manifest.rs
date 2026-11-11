//! The canonical snapshot manifest (`snapshot.toml` beside the data) and the handle
//! [`Snapshot`]. The manifest is written last, atomically: until then the snapshot dir
//! carries a `.partial` suffix and no [`Snapshot::open`] can succeed, so an aborted
//! take never leaves a half snapshot that looks whole. Every data file carries a
//! SHA-256; a changed byte anywhere fails `verify` naming the file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{SnapshotError, SnapshotResult};
use crate::hash::{Inventory, is_safe_relative, verify_tree};

/// The manifest's file name, beside the data.
pub const MANIFEST_NAME: &str = "snapshot.toml";

/// The staging suffix: a snapshot carries it from its first written byte until the
/// manifest is moved into place. `Snapshot::open` refuses any path that holds one.
pub const PARTIAL_SUFFIX: &str = ".partial";

/// How the snapshot was made consistent. `OnlineConsistent` states the transaction
/// horizon (the exported snapshot's xmin and epoch) the whole dump reflects;
/// `Offline` needs none: a stopped instance's copy is the instance itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Consistency {
    /// Every file reflects one exported PostgreSQL snapshot; the horizon names it.
    OnlineConsistent {
        /// The exported snapshot's xmin (the lowest transaction id still visible).
        xmin: u64,
        /// The exported snapshot's epoch for the xmin, as `pg_current_snapshot()`
        /// reports it.
        epoch: u64,
    },
    /// The instance was stopped for the copy; the files are mutually consistent.
    Offline,
}

/// The canonical snapshot manifest, exactly what `snapshot.toml` holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotManifest {
    /// The snapshot id (UUIDv7, time-ordered).
    pub id: String,
    /// The tenant's identity as the caller named it (ADR-058 rule 1: a snapshot is
    /// tenant data). Recorded verbatim; never interpreted here.
    pub tenant: String,
    /// The provider's name (for example `spot-snapshot`).
    pub provider: String,
    /// The provider's version (the workspace version that took the snapshot).
    pub provider_version: String,
    /// Which logical provider took it (`pg_dump` or `stopped_instance`).
    pub provider_kind: String,
    /// The PostgreSQL major version the data came from. Only 18 is supported: any
    /// other major is refused at verify and at restore (`snapshot.version_mismatch`,
    /// whose remedy names the logical-restore possibility).
    pub pg_major: u32,
    /// The catalogue (spot_meta `catalog_version`) at take time.
    pub catalogue_version: String,
    /// The feed schema version (spot_meta `feed_schema`) at take time.
    pub feed_schema: String,
    /// The outbox stream generation at take time; positions of this generation are
    /// invalid after a restore, which must start a new one (see `crate::restore`).
    pub outbox_generation: u64,
    /// The last published position `(seq)` within the generation at take time.
    pub outbox_last_published_seq: u64,
    /// When the take started (the injected [`spot_core::clock::Clock`]).
    pub started_at: String,
    /// When the manifest was written (the injected clock; after every data byte).
    pub finished_at: String,
    /// How consistency was achieved (see [`Consistency`]).
    pub consistency: Consistency,
    /// Total snapshot bytes, integer; no floats for sizes anywhere.
    pub total_bytes: u64,
    /// Number of data files, integer.
    pub file_count: u64,
    /// Every data file with its SHA-256 and size, keyed by path relative to the
    /// snapshot directory. Paths are safe relatives; the manifest is never read for
    /// paths that are not.
    pub files: Inventory,
    /// Node-local state the manifest marks, so a caller can reset it after a restore
    /// (the node id first; see [`crate::NODE_LOCAL_NODE_ID_KEY`]).
    pub node_local: BTreeMapString,
    /// Free-form platform marks for later work packages (VSS/LVM providers); kept
    /// out of the canonical field list so this manifest stays stable.
    #[serde(default, skip_serializing_if = "BTreeMapString::is_empty")]
    pub platform_extra: BTreeMapString,
}

/// A string-keyed string map (aliases kept short in the TOML output).
type BTreeMapString = std::collections::BTreeMap<String, String>;

/// A read, validated snapshot: the manifest plus its directory.
#[derive(Debug, Clone)]
pub struct Snapshot {
    manifest: SnapshotManifest,
    dir: PathBuf,
}

impl Snapshot {
    /// Opens and validates the snapshot at `dir`: parses `snapshot.toml`, checks the
    /// canonical rules (id form, major version, path safety) and refuses a dir that
    /// still carries a `.partial` staging suffix. Does **not** hash the files (that
    /// is `verify`); opening is cheap so callers can inspect the manifest.
    ///
    /// # Errors
    /// `snapshot.io` when the manifest is missing or unparsable, `snapshot.verify_failed`
    /// when a canonical rule is broken.
    pub async fn open(dir: impl Into<PathBuf>) -> SnapshotResult<Snapshot> {
        let dir = dir.into();
        let dir_name = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if dir_name.ends_with(PARTIAL_SUFFIX) {
            return Err(SnapshotError::io(format!(
                "{} is a staged, incomplete snapshot ({} suffix): it never completed",
                dir.display(),
                PARTIAL_SUFFIX
            )));
        }
        let path = dir.join(MANIFEST_NAME);
        let text = tokio::fs::read_to_string(&path).await.map_err(|e| {
            SnapshotError::io(format!(
                "no readable {} in {}: {e}",
                MANIFEST_NAME,
                dir.display()
            ))
        })?;
        let manifest: SnapshotManifest = toml::from_str(&text)
            .map_err(|e| SnapshotError::io(format!("{} does not parse: {e}", path.display())))?;
        manifest.canonical_checks()?;
        Ok(Snapshot { manifest, dir })
    }

    /// The manifest.
    pub fn manifest(&self) -> &SnapshotManifest {
        &self.manifest
    }

    /// The snapshot's directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Re-checks every data file's checksum against the manifest, and refuses extra
    /// files the manifest does not name.
    ///
    /// # Errors
    /// `snapshot.verify_failed` naming the file whose byte changed.
    pub async fn verify(&self) -> SnapshotResult<crate::provider::VerifyReport> {
        verify_tree(&self.dir, &self.manifest.files).await
    }
}

impl SnapshotManifest {
    /// Validates the canonical rules without touching the files.
    ///
    /// # Errors
    /// `snapshot.verify_failed` naming the broken rule (the manifest is the corrupt
    /// thing in this case, so the verify code names it like a file).
    pub fn canonical_checks(&self) -> SnapshotResult<()> {
        let uuid_ok = uuid::Uuid::parse_str(&self.id)
            .map(|parsed| parsed.get_version_num() == 7)
            .unwrap_or(false);
        if !uuid_ok {
            return Err(SnapshotError::VerifyFailed {
                reason: format!("the snapshot id {} is not a UUIDv7", self.id),
            });
        }
        if self.pg_major != crate::SUPPORTED_MAJOR_VERSION {
            return Err(SnapshotError::VersionMismatch {
                snapshot_major: self.pg_major,
                target_major: crate::SUPPORTED_MAJOR_VERSION,
            });
        }
        if self.provider.trim() != self.provider || self.provider.is_empty() {
            return Err(SnapshotError::VerifyFailed {
                reason: "the manifest's provider name is empty or padded".to_owned(),
            });
        }
        // A database snapshot must record its catalogue version (the take reads it on
        // its horizon). An offline copy — the stopped instance's data dir — has no
        // live connection to read `spot_meta` from, so an empty `catalogue_version` is
        // its honest statement and allowed exactly there; the restored site's values
        // are read once it runs again.
        if self.catalogue_version.is_empty() && !matches!(self.consistency, Consistency::Offline) {
            return Err(SnapshotError::VerifyFailed {
                reason: "the manifest's catalogue_version is empty".to_owned(),
            });
        }
        if self.started_at > self.finished_at {
            return Err(SnapshotError::VerifyFailed {
                reason: format!(
                    "the manifest's started_at {} is after finished_at {}",
                    self.started_at, self.finished_at
                ),
            });
        }
        for rel in self.files.keys() {
            if !is_safe_relative(rel) {
                return Err(SnapshotError::VerifyFailed {
                    reason: format!("the manifest names an unsafe path: {rel}"),
                });
            }
        }
        let counted: u64 = self
            .files
            .values()
            .try_fold(0u64, |sum, file| Some(sum.saturating_add(file.bytes)))
            .unwrap_or(u64::MAX);
        if counted != self.total_bytes {
            return Err(SnapshotError::VerifyFailed {
                reason: format!(
                    "the manifest's total_bytes {} does not equal the sum of its files ({counted})",
                    self.total_bytes
                ),
            });
        }
        if self.file_count != u64::try_from(self.files.len()).unwrap_or(u64::MAX) {
            return Err(SnapshotError::VerifyFailed {
                reason: format!(
                    "the manifest's file_count {} does not equal the number of files ({})",
                    self.file_count,
                    self.files.len()
                ),
            });
        }
        Ok(())
    }

    /// The RFC 3339 rendering used for the instants (canonical, offset kept).
    pub fn format_instant(at: OffsetDateTime) -> String {
        use time::format_description::well_known::Rfc3339;
        at.format(&Rfc3339)
            .unwrap_or_else(|_| at.unix_timestamp().to_string())
    }

    /// Parses an instant back.
    ///
    /// # Errors
    /// `snapshot.io` when the text is not RFC 3339.
    pub fn parse_instant(text: &str) -> SnapshotResult<OffsetDateTime> {
        use time::format_description::well_known::Rfc3339;
        OffsetDateTime::parse(text, &Rfc3339)
            .map_err(|e| SnapshotError::io(format!("instant {text} does not parse: {e}")))
    }

    /// Renders the manifest as canonical TOML (fields in declaration order).
    ///
    /// # Errors
    /// `snapshot.io` when serialisation fails (a non-serialisable value is a bug).
    pub fn to_toml(&self) -> SnapshotResult<String> {
        toml::to_string_pretty(self)
            .map_err(|e| SnapshotError::io(format!("the manifest cannot be rendered: {e}")))
    }

    /// Moves the manifest into place atomically, dropping the staging suffix of `dir`
    /// (renaming `dir.partial` to `dir`) and then writing `snapshot.toml` inside.
    /// The write itself is `write-then-rename` of a temp file, so a reader never sees a
    /// half manifest.
    ///
    /// # Errors
    /// Typed [`SnapshotError`].
    pub async fn publish(&self, dir: &Path) -> SnapshotResult<()> {
        let staged = staging_of(dir);
        let text = self.to_toml()?;
        if staged != dir && tokio::fs::try_exists(&staged).await.unwrap_or(false) {
            tokio::fs::rename(&staged, dir).await?;
        }
        tokio::fs::create_dir_all(dir).await?;
        let tmp = dir.join(concat!("snapshot", ".toml.partial-manifest"));
        tokio::fs::write(&tmp, text.as_bytes()).await?;
        tokio::fs::rename(&tmp, dir.join(MANIFEST_NAME)).await?;
        Ok(())
    }
}

/// The staging neighbour `<name>.partial` of `dir`.
pub fn staging_of(dir: &Path) -> PathBuf {
    let mut name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.push_str(PARTIAL_SUFFIX);
    dir.parent().unwrap_or(Path::new(".")).join(name)
}

/// Whether a take into `dest` is possible: absent or empty directory, no staging
/// neighbour in the way.
///
/// # Errors
/// `snapshot.io` explaining the obstruction when it is not.
pub async fn check_destination(dest: &DestinationPath) -> SnapshotResult<()> {
    let dir = &dest.0;
    if tokio::fs::try_exists(&staging_of(dir))
        .await
        .unwrap_or(false)
    {
        return Err(SnapshotError::io(format!(
            "a staged take ({}) is already in the way; remove it first",
            staging_of(dir).display()
        )));
    }
    match tokio::fs::symlink_metadata(dir).await {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SnapshotError::io(format!(
            "cannot inspect {}: {e}",
            dir.display()
        ))),
        Ok(meta) if !meta.is_dir() => Err(SnapshotError::io(format!(
            "{} is not a directory; choose an empty destination",
            dir.display()
        ))),
        Ok(_) => {
            let mut entries = tokio::fs::read_dir(dir)
                .await
                .map_err(|e| SnapshotError::io(format!("cannot list {}: {e}", dir.display())))?;
            if entries
                .next_entry()
                .await
                .map_err(|e| SnapshotError::io(format!("cannot list {}: {e}", dir.display())))?
                .is_some()
            {
                return Err(SnapshotError::io(format!(
                    "{} already holds a snapshot; choose an empty destination",
                    dir.display()
                )));
            }
            Ok(())
        }
    }
}

/// Newtype so the crate-internal destination check takes a path without importing the
/// provider module everywhere.
#[derive(Debug, Clone)]
pub struct DestinationPath(pub PathBuf);

/// Whether `dir` exists and holds a manifest (the restore-side check).
pub async fn holds_manifest(dir: &Path) -> bool {
    tokio::fs::try_exists(dir.join(MANIFEST_NAME))
        .await
        .unwrap_or(false)
}
