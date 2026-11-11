use crate::crypto::KEY_LEN;
use crate::error::SecretError;
use crate::keyring::{self, KeyState};
use crate::provider::{KeyProvider, KeyRef, ProviderFuture, WrappedKey, fresh_key};
use crate::secret::SecretBytes;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
struct KeyFile {
    id: String,
    current: u32,
    keys: Vec<KeyFileEntry>,
}

#[derive(Serialize, Deserialize)]
struct KeyFileEntry {
    version: u32,
    /// Base64 key; absent once the version is retired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

/// A keyring stored in a key file with versioned entries.
///
/// The file is created with `0600` permissions on Unix. On Windows it inherits the directory
/// ACL; see the crate README.
pub struct LocalKeyring {
    path: PathBuf,
    state: RwLock<KeyState>,
    /// Serialises rotate/retire so file writes cannot interleave.
    write_lock: tokio::sync::Mutex<()>,
}

fn io_err(what: &str, e: std::io::Error) -> SecretError {
    SecretError::Provider(format!("{what}: {e}"))
}

impl LocalKeyring {
    /// Creates a new key file at `path` with a random version 1 key. Fails if it exists.
    pub async fn create(path: impl AsRef<Path>, id: &str) -> Result<Self, SecretError> {
        let path = path.as_ref().to_path_buf();
        if tokio::fs::try_exists(&path)
            .await
            .map_err(|e| io_err("key file", e))?
        {
            return Err(SecretError::Provider("key file already exists".into()));
        }
        let state = KeyState::single(id, 1, fresh_key()?);
        persist(&path, &state).await?;
        Ok(Self::with_state(path, state))
    }

    /// Loads an existing key file.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, SecretError> {
        let path = path.as_ref().to_path_buf();
        let raw = tokio::fs::read(&path)
            .await
            .map_err(|e| io_err("reading key file", e))?;
        let raw = Zeroizing::new(raw);
        let file: KeyFile = serde_json::from_slice(&raw)
            .map_err(|_| SecretError::Provider("key file is not valid".into()))?;
        let mut state = KeyState {
            id: file.id,
            current: file.current,
            entries: Default::default(),
        };
        for entry in file.keys {
            let key = match entry.key {
                None => None,
                Some(text) => {
                    let text = Zeroizing::new(text);
                    let bytes = STANDARD
                        .decode(text.as_bytes())
                        .map(Zeroizing::new)
                        .map_err(|_| {
                            SecretError::Provider("key file holds an invalid key".into())
                        })?;
                    let key: [u8; KEY_LEN] = bytes.as_slice().try_into().map_err(|_| {
                        SecretError::Provider("key file holds a key of the wrong length".into())
                    })?;
                    Some(Zeroizing::new(key))
                }
            };
            state.entries.insert(entry.version, key);
        }
        if !matches!(state.entries.get(&state.current), Some(Some(_))) {
            return Err(SecretError::Provider(
                "key file has no usable current key".into(),
            ));
        }
        Ok(Self::with_state(path, state))
    }

    /// Opens the key file, creating it with a fresh key if it does not exist.
    pub async fn open_or_create(path: impl AsRef<Path>, id: &str) -> Result<Self, SecretError> {
        match tokio::fs::try_exists(path.as_ref()).await {
            Ok(true) => Self::open(path).await,
            Ok(false) => Self::create(path, id).await,
            Err(e) => Err(io_err("key file", e)),
        }
    }

    fn with_state(path: PathBuf, state: KeyState) -> Self {
        Self {
            path,
            state: RwLock::new(state),
            write_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Adds version n+1, makes it current and saves the file. Returns the new version.
    /// Values under older versions keep opening until they are rewrapped and the old version
    /// is retired.
    pub async fn rotate(&self) -> Result<u32, SecretError> {
        let _guard = self.write_lock.lock().await;
        let next = keyring::read(&self.state).rotated()?;
        persist(&self.path, &next).await?;
        let version = next.current;
        *keyring::write(&self.state) = next;
        Ok(version)
    }

    /// Drops the key material of `version` and saves the file. Refuses while
    /// `values_still_use` is true, and always for the current version.
    pub async fn retire(&self, version: u32, values_still_use: bool) -> Result<(), SecretError> {
        let _guard = self.write_lock.lock().await;
        let next = keyring::read(&self.state).retired(version, values_still_use)?;
        persist(&self.path, &next).await?;
        *keyring::write(&self.state) = next;
        Ok(())
    }
}

/// Writes the state to a `0600` temp file, then renames it over `path`.
async fn persist(path: &Path, state: &KeyState) -> Result<(), SecretError> {
    let file = KeyFile {
        id: state.id.clone(),
        current: state.current,
        keys: state
            .entries
            .iter()
            .map(|(version, key)| KeyFileEntry {
                version: *version,
                key: key.as_ref().map(|k| STANDARD.encode(k.as_slice())),
            })
            .collect(),
    };
    let json = Zeroizing::new(
        serde_json::to_vec_pretty(&file)
            .map_err(|_| SecretError::Provider("cannot encode key file".into()))?,
    );
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    // `persist` is reached from `create`/`rotate`/`retire` under the in-process write lock only;
    // the key file's cross-process lock (`<kek_file>.lock`) is taken by the rotation service in
    // spot-cli, not here. So a leftover `<path>.tmp` cannot be proven stale and is never removed:
    // the write is refused and the operator removes it (the message names the path only).
    //
    // `create_new` is `O_EXCL`, which also fails on a symlink at `tmp`, so a planted link is never
    // followed or written through. No `O_NOFOLLOW` flag is added: it would need `libc`, and
    // `O_EXCL` already covers the final path component.
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut out = match options.open(&tmp).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(SecretError::Provider(format!(
                "key file temp exists: {} (remove it if no rotation is running)",
                tmp.display()
            )));
        }
        Err(e) => return Err(io_err("writing key file", e)),
    };
    out.write_all(&json)
        .await
        .map_err(|e| io_err("writing key file", e))?;
    out.sync_all()
        .await
        .map_err(|e| io_err("writing key file", e))?;
    drop(out);
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| io_err("replacing key file", e))?;
    // The new directory entry is durable only once the directory itself is synced.
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    dir_sync(dir)
        .await
        .map_err(|e| io_err("syncing key directory", e))
}

/// Syncs a directory so a rename inside it survives a power loss.
#[cfg(unix)]
async fn dir_sync(dir: PathBuf) -> std::io::Result<()> {
    tokio::fs::File::open(&dir).await?.sync_all().await
}

/// Windows cannot open a directory as a file; the rename is flushed by the filesystem.
#[cfg(not(unix))]
async fn dir_sync(_dir: PathBuf) -> std::io::Result<()> {
    Ok(())
}

impl KeyProvider for LocalKeyring {
    fn current(&self) -> KeyRef {
        keyring::read(&self.state).current_ref()
    }

    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        Box::pin(async move { keyring::read(&self.state).wrap(data_key, aad) })
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { keyring::read(&self.state).unwrap(key, wrapped, aad) })
    }

    fn index_key<'a>(&'a self, key: &'a KeyRef) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { keyring::read(&self.state).index_key(key) })
    }

    fn derive_key<'a>(
        &'a self,
        key: &'a KeyRef,
        info: &'a [u8],
        len: usize,
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move { keyring::read(&self.state).derive_key(key, info, len) })
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
mod tests {
    use super::dir_sync;
    use std::path::PathBuf;

    #[tokio::test]
    async fn dir_sync_opens_and_syncs_the_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        dir_sync(dir.path().to_path_buf()).await.expect("sync");
        // A directory that cannot be opened is an error, which `persist` returns as a failed write.
        let missing: PathBuf = dir.path().join("absent");
        assert!(dir_sync(missing).await.is_err());
    }
}
