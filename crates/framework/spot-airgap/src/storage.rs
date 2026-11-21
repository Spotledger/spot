//! The storage seam: every byte the air-gap layer reads or writes goes through [`Storage`].
//!
//! The import engine is platform-neutral logic over this trait, so the same code runs on a
//! real removable medium and under the in-memory fake in `testing` (which injects failures and
//! lets a test resume an interrupted import). [`StdStorage`] is the real implementation over
//! `std::fs`: every write lands through a temporary file and a rename, so a crash never leaves
//! a half-written file where a whole one is expected.
//!
//! The trait is object-safe, so the engine holds `&dyn Storage`.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::AirgapError;

/// Every filesystem operation the air-gap layer needs. Implementations must make
/// [`write_atomic`](Storage::write_atomic) atomic: a reader sees either the old bytes or the
/// new bytes, never a mix, whatever the crash.
pub trait Storage: Send + Sync {
    /// The whole contents of `path`.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when the file is missing or unreadable.
    fn read(&self, path: &Path) -> Result<Vec<u8>, AirgapError>;

    /// Writes `bytes` to `path` atomically (a temporary file, then a rename). The parent
    /// directory must exist.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when the parent is missing or the write fails.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), AirgapError>;

    /// Renames `from` over `to`, atomically where the platform allows. A directory rename
    /// moves the whole subtree.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when either path is missing or the rename fails.
    fn rename(&self, from: &Path, to: &Path) -> Result<(), AirgapError>;

    /// Removes `path` and everything under it; absent paths are a no-op.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when it cannot be removed.
    fn remove_dir_all(&self, path: &Path) -> Result<(), AirgapError>;

    /// Removes one file; an absent file is a no-op.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when it cannot be removed.
    fn remove_file(&self, path: &Path) -> Result<(), AirgapError>;

    /// Creates `path` and every missing parent.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when it cannot be created.
    fn create_dir_all(&self, path: &Path) -> Result<(), AirgapError>;

    /// Whether `path` exists (a file or a directory).
    ///
    /// # Errors
    /// [`AirgapError::Io`] when existence cannot be determined.
    fn exists(&self, path: &Path) -> Result<bool, AirgapError>;

    /// The names directly under `path`, sorted; names, not full paths.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when `path` is not a readable directory.
    fn list(&self, path: &Path) -> Result<Vec<String>, AirgapError>;

    /// Whether `path` is a directory.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when the kind cannot be determined.
    fn is_dir(&self, path: &Path) -> Result<bool, AirgapError>;
}

/// The real filesystem, over `std::fs`.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdStorage;

impl StdStorage {
    /// A real filesystem handle.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// A per-process counter that names temporary files without reading a clock or randomness.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl StdStorage {
    fn write_via_temp(path: &Path, bytes: &[u8]) -> Result<(), AirgapError> {
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
            drop(file);
            std::fs::rename(&temp, path)?;
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

impl Storage for StdStorage {
    fn read(&self, path: &Path) -> Result<Vec<u8>, AirgapError> {
        std::fs::read(path).map_err(|error| io_error("read", path, &error))
    }

    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), AirgapError> {
        Self::write_via_temp(path, bytes)
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<(), AirgapError> {
        std::fs::rename(from, to).map_err(|error| io_error("rename", from, &error))
    }

    fn remove_dir_all(&self, path: &Path) -> Result<(), AirgapError> {
        match std::fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove directory", path, &error)),
        }
    }

    fn remove_file(&self, path: &Path) -> Result<(), AirgapError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error("remove file", path, &error)),
        }
    }

    fn create_dir_all(&self, path: &Path) -> Result<(), AirgapError> {
        std::fs::create_dir_all(path).map_err(|error| io_error("create directory", path, &error))
    }

    fn exists(&self, path: &Path) -> Result<bool, AirgapError> {
        path.try_exists()
            .map_err(|error| io_error("exists", path, &error))
    }

    fn list(&self, path: &Path) -> Result<Vec<String>, AirgapError> {
        let entries = std::fs::read_dir(path).map_err(|error| io_error("list", path, &error))?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| io_error("list", path, &error))?;
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    fn is_dir(&self, path: &Path) -> Result<bool, AirgapError> {
        match std::fs::metadata(path) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_error("stat", path, &error)),
        }
    }
}

/// Reports a `std::io::Error` as [`AirgapError::Io`].
fn io_error(what: &str, path: &Path, error: &std::io::Error) -> AirgapError {
    AirgapError::Io {
        detail: format!("{what} {}: {error}", path.display()),
    }
}

/// Every file under `root`, as paths relative to `root`, sorted; directories are not returned.
///
/// # Errors
/// [`AirgapError::Io`] when the tree cannot be read.
pub fn walk_files(storage: &dyn Storage, root: &Path) -> Result<Vec<String>, AirgapError> {
    let mut files = Vec::new();
    walk_into(storage, root, "", &mut files)?;
    files.sort();
    Ok(files)
}

fn walk_into(
    storage: &dyn Storage,
    dir: &Path,
    prefix: &str,
    out: &mut Vec<String>,
) -> Result<(), AirgapError> {
    for name in storage.list(dir)? {
        let child = dir.join(&name);
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if storage.is_dir(&child)? {
            walk_into(storage, &child, &relative, out)?;
        } else {
            out.push(relative);
        }
    }
    Ok(())
}

#[cfg(any(test, feature = "testing"))]
pub use memory::MemoryStorage;

#[cfg(any(test, feature = "testing"))]
mod memory {
    //! The in-memory [`Storage`] fake with failure injection, for tests and rehearsal.

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::Storage;
    use crate::error::AirgapError;

    #[derive(Clone, Debug)]
    enum Node {
        Dir,
        File(Vec<u8>),
    }

    #[derive(Debug, Default)]
    struct State {
        nodes: BTreeMap<PathBuf, Node>,
        ops: u64,
        fail_at: Option<u64>,
        writes: u64,
        fail_write_at: Option<u64>,
    }

    /// An in-memory filesystem: a map of paths to files or directories, with a failure
    /// injected after a chosen number of operations so an interrupted import can be rehearsed.
    #[derive(Debug, Default)]
    pub struct MemoryStorage {
        state: Mutex<State>,
    }

    impl MemoryStorage {
        /// An empty filesystem.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Seeds a directory (and its ancestors).
        pub fn mkdir(&self, path: &Path) {
            let mut state = self.lock();
            for ancestor in path.ancestors() {
                state
                    .nodes
                    .entry(ancestor.to_path_buf())
                    .or_insert(Node::Dir);
            }
        }

        /// Seeds a file, creating its parents.
        pub fn put_file(&self, path: &Path, bytes: &[u8]) {
            self.mkdir(path.parent().unwrap_or_else(|| Path::new("/")));
            self.lock()
                .nodes
                .insert(path.to_path_buf(), Node::File(bytes.to_vec()));
        }

        /// The bytes of a file, if it exists.
        #[must_use]
        pub fn file(&self, path: &Path) -> Option<Vec<u8>> {
            match self.lock().nodes.get(path) {
                Some(Node::File(bytes)) => Some(bytes.clone()),
                _ => None,
            }
        }

        /// How many operations have run.
        #[must_use]
        pub fn ops(&self) -> u64 {
            self.lock().ops
        }

        /// Makes the operation numbered `op` (0-based) fail with [`AirgapError::Io`].
        pub fn fail_at(&self, op: u64) {
            self.lock().fail_at = Some(op);
        }

        /// Makes the write numbered `n` (0-based) fail with [`AirgapError::Io`].
        pub fn fail_write_at(&self, n: u64) {
            self.lock().fail_write_at = Some(n);
        }

        /// Clears any injected failure.
        pub fn clear_failure(&self) {
            let mut state = self.lock();
            state.fail_at = None;
            state.fail_write_at = None;
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }

        /// Counts an operation, returning the injected failure when its turn has come.
        fn step(&self) -> Result<(), AirgapError> {
            let mut state = self.lock();
            let op = state.ops;
            state.ops += 1;
            if state.fail_at == Some(op) {
                return Err(AirgapError::Io {
                    detail: format!("injected failure at operation {op}"),
                });
            }
            Ok(())
        }
    }

    impl Storage for MemoryStorage {
        fn read(&self, path: &Path) -> Result<Vec<u8>, AirgapError> {
            self.step()?;
            match self.lock().nodes.get(path) {
                Some(Node::File(bytes)) => Ok(bytes.clone()),
                _ => Err(AirgapError::Io {
                    detail: format!("read {}: not a file", path.display()),
                }),
            }
        }

        fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), AirgapError> {
            self.step()?;
            let parent = path.parent().unwrap_or_else(|| Path::new("/"));
            let mut state = self.lock();
            let write = state.writes;
            state.writes += 1;
            if state.fail_write_at == Some(write) {
                return Err(AirgapError::Io {
                    detail: format!("injected write failure at write {write}"),
                });
            }
            if !matches!(state.nodes.get(parent), Some(Node::Dir)) {
                return Err(AirgapError::Io {
                    detail: format!("write {}: parent missing", path.display()),
                });
            }
            state
                .nodes
                .insert(path.to_path_buf(), Node::File(bytes.to_vec()));
            Ok(())
        }

        fn rename(&self, from: &Path, to: &Path) -> Result<(), AirgapError> {
            self.step()?;
            let mut state = self.lock();
            if !state.nodes.contains_key(from) {
                return Err(AirgapError::Io {
                    detail: format!("rename {}: missing", from.display()),
                });
            }
            let moved: Vec<(PathBuf, Node)> = state
                .nodes
                .iter()
                .filter(|(key, _)| key.starts_with(from))
                .map(|(key, node)| (key.clone(), node.clone()))
                .collect();
            for (key, _) in &moved {
                state.nodes.remove(key);
            }
            for (key, node) in moved {
                let suffix = key.strip_prefix(from).unwrap_or(Path::new(""));
                state.nodes.insert(to.join(suffix), node);
            }
            Ok(())
        }

        fn remove_dir_all(&self, path: &Path) -> Result<(), AirgapError> {
            self.step()?;
            let mut state = self.lock();
            let doomed: Vec<PathBuf> = state
                .nodes
                .keys()
                .filter(|key| key.starts_with(path))
                .cloned()
                .collect();
            for key in doomed {
                state.nodes.remove(&key);
            }
            Ok(())
        }

        fn remove_file(&self, path: &Path) -> Result<(), AirgapError> {
            self.step()?;
            self.lock().nodes.remove(path);
            Ok(())
        }

        fn create_dir_all(&self, path: &Path) -> Result<(), AirgapError> {
            self.step()?;
            let mut state = self.lock();
            for ancestor in path.ancestors() {
                state
                    .nodes
                    .entry(ancestor.to_path_buf())
                    .or_insert(Node::Dir);
            }
            Ok(())
        }

        fn exists(&self, path: &Path) -> Result<bool, AirgapError> {
            self.step()?;
            Ok(self.lock().nodes.contains_key(path))
        }

        fn list(&self, path: &Path) -> Result<Vec<String>, AirgapError> {
            self.step()?;
            let state = self.lock();
            if !matches!(state.nodes.get(path), Some(Node::Dir)) {
                return Err(AirgapError::Io {
                    detail: format!("list {}: not a directory", path.display()),
                });
            }
            let mut names = Vec::new();
            for key in state.nodes.keys() {
                if key.parent() == Some(path)
                    && let Some(name) = key.file_name()
                {
                    names.push(name.to_string_lossy().into_owned());
                }
            }
            names.sort();
            Ok(names)
        }

        fn is_dir(&self, path: &Path) -> Result<bool, AirgapError> {
            self.step()?;
            Ok(matches!(self.lock().nodes.get(path), Some(Node::Dir)))
        }
    }
}
