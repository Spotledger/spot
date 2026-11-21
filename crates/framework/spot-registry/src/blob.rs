use crate::{Digest, RegistryError, Result};
use spot_bundle::{Bundle, HostInfo, Limits};
use std::{
    collections::HashMap,
    future::Future,
    io::{Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

/// Boxed, Send future, allowing object-safe storage and client adapters without a runtime.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
/// Pull-based asynchronous byte stream. Errors stop collection before any index mutation.
pub trait ByteStream: Send {
    /// Next chunk, or EOF. Implementations must eventually terminate.
    fn next(&mut self) -> BoxFuture<'_, Option<Vec<u8>>>;
}
/// An owned stream of chunks, convenient for transports and local clients.
pub struct Chunks(std::collections::VecDeque<Vec<u8>>);
impl Chunks {
    /// Builds a finite stream.
    pub fn new(chunks: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self(chunks.into_iter().collect())
    }
}
impl ByteStream for Chunks {
    fn next(&mut self) -> BoxFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move { Ok(self.0.pop_front()) })
    }
}
/// A boxed stream independent of the store's lifetime.
pub type BytesStream = Box<dyn ByteStream>;
/// One owned byte buffer as a stream.
pub fn bytes_stream(bytes: Vec<u8>) -> BytesStream {
    Box::new(Chunks::new([bytes]))
}
/// Collects with an enforced compressed-byte bound; empty chunks do not increase size.
pub async fn collect_stream(mut stream: BytesStream, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await? {
        if (bytes.len() as u64).saturating_add(chunk.len() as u64) > max {
            return Err(RegistryError::new(
                "registry.too_large",
                "stream exceeds byte limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
/// Validator shared by store and registry. Configure both with the same host and limits.
#[derive(Clone, Debug)]
pub struct BlobValidation {
    /// Bundle parser limits.
    pub limits: Limits,
    /// Host for bundle validation (requires_host is checked by the bundle API).
    pub host: HostInfo,
}
impl Default for BlobValidation {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            host: HostInfo {
                version: crate::Version::new(0, 1, 0),
            },
        }
    }
}
impl BlobValidation {
    pub(crate) fn check(&self, digest: Digest, bytes: &[u8]) -> Result<()> {
        let bundle = Bundle::open(bytes, &self.limits, &self.host).map_err(|_| {
            RegistryError::new(
                "registry.digest_mismatch",
                "stored bytes are not the addressed bundle",
            )
        })?;
        if bundle.digest() != digest {
            return Err(RegistryError::new(
                "registry.digest_mismatch",
                "canonical digest differs",
            ));
        }
        Ok(())
    }
}
/// Content-addressed, object-safe async store. Writes/read streams are bounded and checked
/// before exposure. Existing bytes are retained on put. Inventory enables orphan repair.
pub trait BlobStore: Send + Sync {
    /// Validates and durably puts bytes, without replacing an existing valid object.
    fn put_if_absent(&self, digest: Digest, stream: BytesStream) -> BoxFuture<'_, ()>;
    /// Returns a stream only after validating the whole stored bundle.
    fn get(&self, digest: Digest) -> BoxFuture<'_, BytesStream>;
    /// Checks presence and integrity, returning false only when absent.
    fn exists(&self, digest: Digest) -> BoxFuture<'_, bool>;
    /// Verified stored byte count, or not_found.
    fn size(&self, digest: Digest) -> BoxFuture<'_, u64>;
    /// Lists stored digest addresses deterministically; does not delete or repair bytes.
    fn inventory(&self) -> BoxFuture<'_, Vec<Digest>>;
}
/// Thread-safe in-memory blob storage.
pub struct MemoryBlobStore {
    validation: BlobValidation,
    blobs: Mutex<HashMap<Digest, Vec<u8>>>,
}
impl MemoryBlobStore {
    /// Empty store using a bounded bundle validator.
    pub fn new(validation: BlobValidation) -> Self {
        Self {
            validation,
            blobs: Mutex::new(HashMap::new()),
        }
    }
    fn read(&self, digest: Digest) -> Result<Vec<u8>> {
        let bytes = self
            .blobs
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .get(&digest)
            .cloned()
            .ok_or_else(not_found)?;
        self.validation.check(digest, &bytes)?;
        Ok(bytes)
    }
}
impl Default for MemoryBlobStore {
    fn default() -> Self {
        Self::new(BlobValidation::default())
    }
}
impl BlobStore for MemoryBlobStore {
    fn put_if_absent(&self, digest: Digest, stream: BytesStream) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let bytes = collect_stream(stream, self.validation.limits.max_compressed_bytes).await?;
            self.validation.check(digest, &bytes)?;
            let mut blobs = self.blobs.lock().map_err(|_| crate::error::poisoned())?;
            if let Some(old) = blobs.get(&digest) {
                self.validation.check(digest, old)?;
            } else {
                blobs.insert(digest, bytes);
            }
            Ok(())
        })
    }
    fn get(&self, digest: Digest) -> BoxFuture<'_, BytesStream> {
        Box::pin(async move { Ok(bytes_stream(self.read(digest)?)) })
    }
    fn exists(&self, digest: Digest) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            match self.read(digest) {
                Ok(_) => Ok(true),
                Err(e) if e.code == "registry.not_found" => Ok(false),
                Err(e) => Err(e),
            }
        })
    }
    fn size(&self, digest: Digest) -> BoxFuture<'_, u64> {
        Box::pin(async move { Ok(self.read(digest)?.len() as u64) })
    }
    fn inventory(&self) -> BoxFuture<'_, Vec<Digest>> {
        Box::pin(async move {
            let mut rows: Vec<_> = self
                .blobs
                .lock()
                .map_err(|_| crate::error::poisoned())?
                .keys()
                .copied()
                .collect();
            rows.sort_by_key(Digest::to_hex);
            Ok(rows)
        })
    }
}
fn not_found() -> RegistryError {
    RegistryError::new("registry.not_found", "blob absent")
}
static FS_MUTATION: Mutex<()> = Mutex::new(());
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
/// Atomic filesystem storage in a private directory controlled exclusively by the service.
/// Symlinks/nonregular files are refused. All in-process instances serialize writes. The
/// directory must not be writable by other processes: std-only path APIs cannot defend
/// against adversarial symlink replacement between metadata checks and open/rename.
/// On Unix the parent directory is fsynced; std offers no portable directory fsync on Windows.
pub struct FsBlobStore {
    root: PathBuf,
    validation: BlobValidation,
}
impl FsBlobStore {
    /// Creates the storage directory and fixes its canonical path. Caller supplies a trusted
    /// deployment path; object names are derived solely from validated Digest::to_hex().
    pub fn new(root: impl AsRef<Path>, validation: BlobValidation) -> Result<Self> {
        std::fs::create_dir_all(root.as_ref())?;
        if std::fs::symlink_metadata(root.as_ref())?
            .file_type()
            .is_symlink()
        {
            return Err(path_error());
        }
        Ok(Self {
            root: root.as_ref().canonicalize()?,
            validation,
        })
    }
    fn path(&self, digest: Digest) -> Result<PathBuf> {
        let meta = std::fs::symlink_metadata(&self.root)?;
        if !meta.is_dir() || meta.file_type().is_symlink() {
            return Err(path_error());
        }
        Ok(self.root.join(digest.to_hex()))
    }
    fn read(&self, digest: Digest) -> Result<Vec<u8>> {
        let path = self.path(digest)?;
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(not_found()),
            Err(e) => return Err(e.into()),
        };
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(path_error());
        }
        if meta.len() > self.validation.limits.max_compressed_bytes {
            return Err(RegistryError::new(
                "registry.digest_mismatch",
                "stored object exceeds limit",
            ));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(
                self.validation
                    .limits
                    .max_compressed_bytes
                    .saturating_add(1),
            )
            .read_to_end(&mut bytes)?;
        self.validation.check(digest, &bytes)?;
        Ok(bytes)
    }
    fn put(&self, digest: Digest, bytes: &[u8]) -> Result<()> {
        let _lock = FS_MUTATION.lock().map_err(|_| crate::error::poisoned())?;
        match self.read(digest) {
            Ok(_) => return Ok(()),
            Err(e) if e.code == "registry.not_found" => {}
            Err(e) => return Err(e),
        }
        let target = self.path(digest)?;
        let (temp, mut file) = loop {
            let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let temp = self.root.join(format!(".tmp-{}-{seq}", std::process::id()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
            {
                Ok(file) => break (temp, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let outcome = (|| -> Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&temp, target)?;
            #[cfg(unix)]
            std::fs::File::open(&self.root)?.sync_all()?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        outcome
    }
}
fn path_error() -> RegistryError {
    RegistryError::new(
        "registry.unsafe_path",
        "storage path is a symlink or wrong file type",
    )
}
impl BlobStore for FsBlobStore {
    fn put_if_absent(&self, digest: Digest, stream: BytesStream) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let bytes = collect_stream(stream, self.validation.limits.max_compressed_bytes).await?;
            self.validation.check(digest, &bytes)?;
            self.put(digest, &bytes)
        })
    }
    fn get(&self, digest: Digest) -> BoxFuture<'_, BytesStream> {
        Box::pin(async move { Ok(bytes_stream(self.read(digest)?)) })
    }
    fn exists(&self, digest: Digest) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            match self.read(digest) {
                Ok(_) => Ok(true),
                Err(e) if e.code == "registry.not_found" => Ok(false),
                Err(e) => Err(e),
            }
        })
    }
    fn size(&self, digest: Digest) -> BoxFuture<'_, u64> {
        Box::pin(async move { Ok(self.read(digest)?.len() as u64) })
    }
    fn inventory(&self) -> BoxFuture<'_, Vec<Digest>> {
        Box::pin(async move {
            let mut rows = Vec::new();
            for entry in std::fs::read_dir(&self.root)? {
                let entry = entry?;
                if let Some(name) = entry.file_name().to_str() {
                    if name.starts_with(".tmp-") {
                        continue;
                    }
                    let digest = name.parse::<Digest>().map_err(|_| path_error())?;
                    if name != digest.to_hex() {
                        return Err(path_error());
                    }
                    self.read(digest)?;
                    rows.push(digest);
                } else {
                    return Err(path_error());
                }
            }
            rows.sort_by_key(Digest::to_hex);
            Ok(rows)
        })
    }
}
