//! The filesystem seam: everything an update does to disk goes through [`FileSystem`].
//!
//! The update engine is platform-neutral logic over this trait, so the same code runs on a
//! Windows service host and under the in-memory fake in `testing` (which injects failures and
//! power cuts). [`StdFileSystem`] is the real implementation over `std::fs`: every write
//! lands through a temporary file and a rename, so a crash never leaves a half-written file
//! where a whole one is expected, and [`durable_write`](FileSystem::durable_write) also flushes
//! the file and its directory so the bytes survive a power loss.
//!
//! The trait is object-safe, so the engine holds `&dyn FileSystem`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::UpdaterError;

/// Every filesystem operation an update needs. Implementations must make
/// [`write_atomic`](FileSystem::write_atomic) and
/// [`durable_write`](FileSystem::durable_write) atomic: a reader sees either the old bytes or
/// the new bytes, never a mix, whatever the crash.
pub trait FileSystem: Send + Sync {
    /// The whole contents of `path`.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when the file is missing or unreadable.
    fn read(&self, path: &Path) -> Result<Vec<u8>, UpdaterError>;

    /// Writes `bytes` to `path` atomically (a temporary file, then a rename).
    ///
    /// # Errors
    /// [`UpdaterError::DiskFull`] on a full disk, [`UpdaterError::Io`] otherwise.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), UpdaterError>;

    /// Writes `bytes` to `path` atomically and flushes to durable storage, so the new bytes
    /// survive a power loss. Used for the journal and the pointers.
    ///
    /// # Errors
    /// [`UpdaterError::DiskFull`] on a full disk, [`UpdaterError::Io`] otherwise.
    fn durable_write(&self, path: &Path, bytes: &[u8]) -> Result<(), UpdaterError>;

    /// Renames `from` over `to`, atomically where the platform allows.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when either path is missing or the rename fails.
    fn rename(&self, from: &Path, to: &Path) -> Result<(), UpdaterError>;

    /// Removes `path` and everything under it.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when it cannot be removed.
    fn remove_dir_all(&self, path: &Path) -> Result<(), UpdaterError>;

    /// Removes one file.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when it cannot be removed.
    fn remove_file(&self, path: &Path) -> Result<(), UpdaterError>;

    /// Creates `path` and every missing parent.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when it cannot be created.
    fn create_dir_all(&self, path: &Path) -> Result<(), UpdaterError>;

    /// Whether `path` exists (a file or a directory).
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when existence cannot be determined.
    fn exists(&self, path: &Path) -> Result<bool, UpdaterError>;

    /// The names directly under `path`, sorted; names, not full paths.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when `path` is not a readable directory.
    fn list(&self, path: &Path) -> Result<Vec<String>, UpdaterError>;

    /// The bytes free on the volume that holds `path`.
    ///
    /// # Errors
    /// [`UpdaterError::Io`] when the free space cannot be determined.
    fn free_space(&self, path: &Path) -> Result<u64, UpdaterError>;
}

/// The real filesystem, over `std::fs`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdFileSystem;

impl StdFileSystem {
    /// A real filesystem handle.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// A per-process counter that names temporary files without reading a clock or randomness.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl StdFileSystem {
    /// Writes the bytes via a temporary file in the same directory, then renames it over
    /// `path`. `sync` also flushes the file and directory to durable
    /// storage.
    fn write_via_temp(path: &Path, bytes: &[u8], sync: bool) -> Result<(), UpdaterError> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let name = path
            .file_name()
            .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp: PathBuf = parent.join(format!(".{name}.tmp-{}-{n}", std::process::id()));
        let outcome = (|| -> std::io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(bytes)?;
            file.flush()?;
            if sync {
                file.sync_all()?;
            }
            drop(file);
            std::fs::rename(&temp, path)?;
            if sync {
                sync_directory(parent)?;
            }
            Ok(())
        })();
        match outcome {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = std::fs::remove_file(&temp);
                Err(io_error("write", path, &error))
            }
        }
    }
}

impl FileSystem for StdFileSystem {
    fn read(&self, path: &Path) -> Result<Vec<u8>, UpdaterError> {
        std::fs::read(path).map_err(|error| io_error("read", path, &error))
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), UpdaterError> {
        Self::write_via_temp(path, bytes, false)
    }

    fn durable_write(&self, path: &Path, bytes: &[u8]) -> Result<(), UpdaterError> {
        Self::write_via_temp(path, bytes, true)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), UpdaterError> {
        std::fs::rename(from, to).map_err(|error| io_error("rename", from, &error))?;
        sync_parent(from)?;
        sync_parent(to)
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), UpdaterError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => sync_parent(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove directory", path, &error)),
        }
    }

    fn remove_file(&self, path: &Path) -> Result<(), UpdaterError> {
        match std::fs::remove_file(path) {
            Ok(()) => sync_parent(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove file", path, &error)),
        }
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), UpdaterError> {
        std::fs::create_dir_all(path)
            .map_err(|error| io_error("create directory", path, &error))?;
        // Persist newly created ancestors as well as the leaf before a pointer may name it.
        for directory in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
            sync_directory(directory)
                .map_err(|error| io_error("sync directory", directory, &error))?;
        }
        Ok(())
    }

    fn exists(&self, path: &Path) -> Result<bool, UpdaterError> {
        path.try_exists()
            .map_err(|error| io_error("exists", path, &error))
    }

    fn list(&self, path: &Path) -> Result<Vec<String>, UpdaterError> {
        let entries = std::fs::read_dir(path).map_err(|error| io_error("list", path, &error))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| io_error("list", path, &error))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    fn free_space(&self, path: &Path) -> Result<u64, UpdaterError> {
        free_space_of(path)
    }
}

// Directory sync is required, not best effort, on the Unix adapter. Windows needs its
// separately owned adapter: std does not expose a portable durable directory handle.
#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "durable directories require a platform FileSystem adapter",
    ))
}

fn sync_parent(path: &Path) -> Result<(), UpdaterError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    sync_directory(parent).map_err(|error| io_error("sync parent", parent, &error))
}

/// Reports a `std::io::Error` as an [`UpdaterError::Io`], with `NotFound` folded into a
/// plain message. A full disk is reported as [`UpdaterError::DiskFull`].
fn io_error(what: &str, path: &Path, error: &std::io::Error) -> UpdaterError {
    if error.kind() == std::io::ErrorKind::StorageFull {
        return UpdaterError::DiskFull {
            detail: format!("{what} {}", path.display()),
        };
    }
    UpdaterError::Io {
        detail: format!("{what} {}: {error}", path.display()),
    }
}

/// The free bytes on the volume holding `path`.
///
/// `std` exposes no free-space API and this crate may add no dependency for it, so on Unix
/// this asks the system's `df` for the volume's available blocks; on any other platform it is
/// a typed error and the caller supplies its own [`FileSystem`].
#[cfg(unix)]
fn free_space_of(path: &Path) -> Result<u64, UpdaterError> {
    let output = std::process::Command::new("df")
        .arg("-P")
        .arg("-k")
        .arg(path)
        .output()
        .map_err(|error| UpdaterError::Io {
            detail: format!("free space of {}: {error}", path.display()),
        })?;
    if !output.status.success() {
        return Err(UpdaterError::Io {
            detail: format!("free space of {}: df failed", path.display()),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // POSIX `df -P`: header, then one line per filesystem, `Available` in the fourth field
    // (0-based index 3), in 1024-byte blocks.
    let line = text.lines().nth(1).ok_or_else(|| UpdaterError::Io {
        detail: format!("free space of {}: df said nothing", path.display()),
    })?;
    let field = line
        .split_whitespace()
        .nth(3)
        .ok_or_else(|| UpdaterError::Io {
            detail: format!(
                "free space of {}: df output was not understood",
                path.display()
            ),
        })?;
    let blocks: u64 = field.parse().map_err(|_| UpdaterError::Io {
        detail: format!("free space of {}: df reported `{field}`", path.display()),
    })?;
    Ok(blocks.saturating_mul(1024))
}

#[cfg(not(unix))]
fn free_space_of(path: &Path) -> Result<u64, UpdaterError> {
    Err(UpdaterError::Io {
        detail: format!(
            "free space of {}: not available on this platform; supply a FileSystem implementation",
            path.display()
        ),
    })
}
