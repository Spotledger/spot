//! Backup targets: the port every destination implements and the shared file helpers.
//!
//! The [`BackupTarget`] trait is deliberately small and object-safe: `put`, `get`,
//! `list`, `delete` and `free_space`, plus the status a removable target needs. A
//! target writes a stream into a temporary sibling and renames it into place only
//! after the bytes and digest match, so an interrupted put never appears in `list`.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use crate::error::{BackupError, Result};
use crate::manifest::BackupManifest;
use crate::model::{StoredBackup, TargetStatus};
use crate::package::PACKAGE_EXTENSION;

pub mod cloud;
pub mod folder;
pub mod removable;
pub mod volume;

pub use cloud::{CloudObject, CloudStore, CloudTarget, RecordingCloud};
pub use folder::FolderTarget;
pub use removable::RemovableTarget;
pub use volume::{FakeVolumeProbe, SystemVolumeProbe, Volume, VolumeProbe, volumes_for_probe};

/// Measures free bytes on a volume. The real probe asks the platform; tests inject a
/// fixed value, so no test depends on the machine it runs on.
pub trait SpaceProbe: Send + Sync {
    /// Free bytes at `path`.
    ///
    /// # Errors
    /// [`BackupError::Unsupported`] when the platform offers no probe.
    fn free_bytes(&self, path: &Path) -> Result<u64>;
}

/// The platform's own probe: `df` on Unix (the same approach as `spot-updater`), and a
/// typed error elsewhere until the owner supplies its platform adapter.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemSpaceProbe;

impl SpaceProbe for SystemSpaceProbe {
    fn free_bytes(&self, path: &Path) -> Result<u64> {
        free_space_of(path)
    }
}

/// A probe that always reports the same number, for tests and fixed quotas.
#[derive(Debug, Clone, Copy)]
pub struct FixedSpaceProbe(pub u64);

impl SpaceProbe for FixedSpaceProbe {
    fn free_bytes(&self, _path: &Path) -> Result<u64> {
        Ok(self.0)
    }
}

#[cfg(unix)]
fn free_space_of(path: &Path) -> Result<u64> {
    let output = std::process::Command::new("df")
        .args(["-P", "-B1", "--"])
        .arg(path)
        .output()
        .map_err(|error| BackupError::io(format!("free space of {}: {error}", path.display())))?;
    if !output.status.success() {
        return Err(BackupError::io(format!(
            "free space of {}: df failed",
            path.display()
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().nth(1).ok_or_else(|| {
        BackupError::io(format!("free space of {}: df said nothing", path.display()))
    })?;
    let field = line.split_whitespace().nth(3).ok_or_else(|| {
        BackupError::io(format!(
            "free space of {}: df output was not understood",
            path.display()
        ))
    })?;
    field.parse::<u64>().map_err(|_| {
        BackupError::io(format!(
            "free space of {}: df said {field:?}",
            path.display()
        ))
    })
}

#[cfg(not(unix))]
fn free_space_of(_path: &Path) -> Result<u64> {
    Err(BackupError::Unsupported {
        feature: "free-space probing on this platform; supply a SpaceProbe".to_owned(),
    })
}

/// A destination backups can be written to and read from.
pub trait BackupTarget: Send + Sync {
    /// The target's configuration label, as the manifest records it.
    fn describe(&self) -> String;

    /// Whether the target can accept a backup right now.
    fn status(&self) -> TargetStatus;

    /// Writes `body` (exactly `size` bytes) as backup `id`.
    ///
    /// `size` and `digest` describe the stream being copied (the whole package), not
    /// the manifest's payload digest: the target proves the bytes it received are the
    /// bytes the producer built. The implementation hashes while writing and refuses
    /// the stream when the bytes or digest differ, and publishes it only after an
    /// fsync + atomic rename.
    ///
    /// # Errors
    /// [`BackupError::TargetUnavailable`], [`BackupError::VerifyFailed`] or
    /// [`BackupError::Io`].
    fn put(
        &self,
        id: &str,
        body: Box<dyn Read + Send>,
        size: u64,
        digest: &str,
    ) -> Result<StoredBackup>;

    /// Opens backup `id` for reading (the whole package, header first).
    ///
    /// # Errors
    /// [`BackupError::NotFound`] or [`BackupError::Io`].
    fn get(&self, id: &str) -> Result<Box<dyn Read + Send>>;

    /// Every backup the target currently holds, newest first.
    ///
    /// # Errors
    /// [`BackupError::TargetUnavailable`] or [`BackupError::Io`].
    fn list(&self) -> Result<Vec<StoredBackup>>;

    /// Deletes backup `id`.
    ///
    /// # Errors
    /// [`BackupError::NotFound`] or [`BackupError::Io`].
    fn delete(&self, id: &str) -> Result<()>;

    /// Free bytes for the next backup.
    ///
    /// # Errors
    /// [`BackupError::TargetUnavailable`] or [`BackupError::Unsupported`].
    fn free_space(&self) -> Result<u64>;
}

/// Reads and parses a package header from a reader without hashing the payload.
pub(crate) fn parse_header(reader: &mut dyn Read) -> Result<BackupManifest> {
    let mut magic = [0u8; 8];
    reader
        .read_exact(&mut magic)
        .map_err(|error| BackupError::VerifyFailed {
            reason: format!("the package header cannot be read: {error}"),
        })?;
    if &magic != crate::package::MAGIC {
        return Err(BackupError::VerifyFailed {
            reason: "the package magic is not SPOTBAK1".to_owned(),
        });
    }
    let mut length = [0u8; 8];
    reader.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    if length == 0 || length > 16 * 1024 * 1024 {
        return Err(BackupError::VerifyFailed {
            reason: format!("the package manifest length {length} is not plausible"),
        });
    }
    let mut text = vec![
        0u8;
        usize::try_from(length).map_err(|_| {
            BackupError::ManifestInvalid {
                reason: "the manifest length does not fit in memory".to_owned(),
            }
        })?
    ];
    reader.read_exact(&mut text)?;
    let text = String::from_utf8(text).map_err(|error| BackupError::ManifestInvalid {
        reason: format!("the package manifest is not UTF-8: {error}"),
    })?;
    BackupManifest::from_toml(&text)
}

/// The file name of a package inside a target directory.
pub(crate) fn package_name(id: &str) -> Result<String> {
    if id.is_empty()
        || id.len() > 200
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(BackupError::ManifestInvalid {
            reason: format!("the backup id {id:?} is not usable as a file name"),
        });
    }
    Ok(format!("{id}.{PACKAGE_EXTENSION}"))
}

/// Writes a package atomically into `dir`: a dot-temp file, hash and size check,
/// `sync_all`, rename, then a directory sync where the platform allows it.
pub(crate) fn write_package_atomic(
    dir: &Path,
    id: &str,
    mut body: Box<dyn Read + Send>,
    size: u64,
    digest: &str,
) -> Result<StoredBackup> {
    std::fs::create_dir_all(dir)?;
    let name = package_name(id)?;
    let final_path = dir.join(&name);
    let sequence = next_temp_sequence();
    let temp_path = dir.join(format!(".{name}.tmp-{}-{sequence}", std::process::id()));
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let result = (|| -> Result<BackupManifest> {
        let mut out = BufWriter::new(
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temp_path)?,
        );
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = body.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            out.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            written = written.saturating_add(read as u64);
        }
        out.flush()?;
        out.get_ref().sync_all()?;
        drop(out);
        if written != size {
            return Err(BackupError::VerifyFailed {
                reason: format!(
                    "the target received {written} bytes for {id} but the manifest says {size}"
                ),
            });
        }
        let actual = hex(&hasher.finalize());
        if !actual.eq_ignore_ascii_case(digest) {
            return Err(BackupError::VerifyFailed {
                reason: format!(
                    "the target computed digest {actual} for {id} but the manifest says {digest}"
                ),
            });
        }
        let mut reader = BufReader::new(File::open(&temp_path)?);
        parse_header(&mut reader)
    })();

    let manifest = match result {
        Ok(manifest) => manifest,
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(error);
        }
    };
    std::fs::rename(&temp_path, &final_path)?;
    sync_dir(dir);
    Ok(StoredBackup {
        id: id.to_owned(),
        manifest,
        package_bytes: written,
    })
}

/// Reads `<dir>/<id>.spotbak`.
pub(crate) fn read_package(dir: &Path, id: &str) -> Result<Box<dyn Read + Send>> {
    let path = dir.join(package_name(id)?);
    if !path.is_file() {
        return Err(BackupError::NotFound { id: id.to_owned() });
    }
    Ok(Box::new(BufReader::new(File::open(path)?)))
}

/// Lists the packages in `dir`, newest first.
pub(crate) fn list_packages(dir: &Path) -> Result<Vec<StoredBackup>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some(PACKAGE_EXTENSION) {
            continue;
        }
        let package_bytes = entry.metadata()?.len();
        let mut reader = BufReader::new(File::open(&path)?);
        let manifest = parse_header(&mut reader)?;
        let id = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        out.push(StoredBackup {
            id,
            manifest,
            package_bytes,
        });
    }
    out.sort_by(|left, right| {
        right
            .manifest
            .created_at
            .cmp(&left.manifest.created_at)
            .then_with(|| {
                right
                    .manifest
                    .chain_sequence
                    .cmp(&left.manifest.chain_sequence)
            })
    });
    Ok(out)
}

/// Deletes `<dir>/<id>.spotbak`, refusing an id that is not a safe file name.
pub(crate) fn delete_package(dir: &Path, id: &str) -> Result<()> {
    let path = dir.join(package_name(id)?);
    std::fs::remove_file(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BackupError::NotFound { id: id.to_owned() }
        } else {
            error.into()
        }
    })
}

/// A target over a directory, with an injected space probe.
pub(crate) fn folder_free_space(dir: &Path, probe: &Arc<dyn SpaceProbe>) -> Result<u64> {
    // The directory may not exist before the first backup. Probe its nearest existing
    // ancestor instead of creating it just to ask a question.
    let mut probe_path = dir;
    while !probe_path.exists() {
        match probe_path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => probe_path = parent,
            _ => break,
        }
    }
    probe.free_bytes(probe_path)
}

fn next_temp_sequence() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(unix)]
fn sync_dir(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) {}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The path of the package a target would write, for diagnostics and tests.
pub fn package_path(base: &Path, id: &str) -> Result<PathBuf> {
    Ok(base.join(package_name(id)?))
}
